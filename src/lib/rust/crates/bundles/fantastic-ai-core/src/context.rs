//! Context-window budgeting primitives for the overflow strategies — a
//! char-based token ESTIMATE, deliberately tokenizer-agnostic (exactness
//! is irrelevant for a fit-to-window budget, and tiktoken would be the
//! WRONG tokenizer for gemma/nemotron). Plus window/budget resolution off
//! the agent record meta. Mirrors the Python `ai_core/context.py`.

use fantastic_bundle::{decimal_value, py_int, py_json_dumps, py_strip};
use serde_json::{json, Map, Value};

// The tunables live in [`crate::constants`]; re-exported here so every
// `crate::context::…` path keeps working.
pub use crate::constants::{
    BUDGET_FLOOR, CHARS_PER_TOKEN, DEFAULT_CONTEXT_WINDOW, DEFAULT_OUTPUT_RESERVE,
    NOTICE_ENVELOPE_RESERVE, RECENT_N, RECENT_N_RANGE,
};

/// Rough token estimate for ONE message — counts the SERIALIZED form, because
/// role + content + the JSON envelope all consume real context.
pub fn estimate_one(message: &Value) -> i64 {
    // PYTHON'S `json.dumps`, PYTHON'S LENGTH: `, `/`: ` separators, `\uXXXX`
    // escapes (ensure_ascii), and CODE POINTS. `serde_json`'s compact separators
    // and byte length made every estimate smaller than python's — by 2 chars per
    // key, and by 5 per non-ASCII character.
    let n = py_json_dumps(message, true, None).chars().count();
    n.div_ceil(CHARS_PER_TOKEN) as i64
}

/// Sum of [`estimate_one`] across a message slice.
pub fn estimate_tokens(messages: &[Value]) -> i64 {
    messages.iter().map(estimate_one).sum()
}

/// Python `_as_pos_int`: a bool is never one; an int must be > 0; a float must
/// be > 0 and then TRUNCATES (`0.5` → 0, which is why the budget can floor); a
/// string counts only when `strip().isdigit()` (so `+12` and `-3` are not).
fn as_pos_int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Bool(_)) => None,
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                (i > 0).then_some(i)
            } else if let Some(f) = n.as_f64() {
                (f > 0.0).then_some(f as i64)
            } else {
                None
            }
        }
        Some(Value::String(s)) => {
            let t = py_strip(s);
            if !t.is_empty() && t.chars().all(|c| decimal_value(c).is_some()) {
                py_int(&Value::String(t.to_string()))
                    .ok()
                    .filter(|&i| i > 0)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The model's usable window, by STATIC precedence (no fallback-chain):
/// `context_window` (the explicit per-agent override — works on any
/// backend incl. NIM which has no `num_ctx`) → `num_ctx` (ollama's real
/// knob) → a conservative default.
pub fn resolve_context_window(meta: &Map<String, Value>) -> i64 {
    for key in ["context_window", "num_ctx"] {
        if let Some(v) = as_pos_int(meta.get(key)) {
            return v;
        }
    }
    DEFAULT_CONTEXT_WINDOW
}

/// Output headroom reserved out of the window — python `output_reserve`: a
/// NON-BOOL int ≥ 0, or a `strip().isdigit()` string, else 1024. A float (even
/// `1000.0`), a negative, and `+5` all take the default.
pub fn output_reserve(meta: &Map<String, Value>) -> i64 {
    match meta.get("output_reserve") {
        Some(Value::Bool(_)) => DEFAULT_OUTPUT_RESERVE,
        Some(Value::Number(n)) => n
            .as_i64()
            .filter(|&i| i >= 0)
            .unwrap_or(DEFAULT_OUTPUT_RESERVE),
        Some(Value::String(s)) => {
            let t = py_strip(s);
            if !t.is_empty() && t.chars().all(|c| decimal_value(c).is_some()) {
                py_int(&Value::String(t.to_string())).unwrap_or(DEFAULT_OUTPUT_RESERVE)
            } else {
                DEFAULT_OUTPUT_RESERVE
            }
        }
        _ => DEFAULT_OUTPUT_RESERVE,
    }
}

/// Token budget for the INPUT (window minus output headroom), floored.
pub fn budget(meta: &Map<String, Value>) -> i64 {
    (resolve_context_window(meta) - output_reserve(meta)).max(BUDGET_FLOOR)
}

/// The agent's configured `recent_n` (verbatim recent turns kept by
/// compact/truncate) — python `int(v)` clamped to [1, 50], and 6 on the
/// TypeError/ValueError `int()` raises. `True` is 1, `7.9` is 7, `" 7 "` is 7.
pub fn recent_n(meta: &Map<String, Value>) -> usize {
    let n = match meta.get("recent_n") {
        Some(v) => py_int(v).unwrap_or(RECENT_N),
        None => RECENT_N,
    };
    let (lo, hi) = RECENT_N_RANGE;
    n.clamp(lo, hi) as usize
}

/// Tokens a reply may occupy INLINE in the model's next turn. Above this it is
/// stored and digested. An eighth of the input budget: a 12k window inlines
/// ~1.4k tokens (a reflect, an error, a short read) and digests a 6.8KB file
/// body. Python `_inline_cap`.
#[must_use]
pub fn inline_cap(meta: &Map<String, Value>) -> i64 {
    (budget(meta) / 8).max(BUDGET_FLOOR)
}

/// THE CAP INVARIANT. Every verb an agent calls on itself must answer UNDER the
/// inline cap, or its answer is stored and digested and the model pulls its own
/// reply (measured 2026-09-06, python: a 4788-byte grep result came back as a
/// receipt; the run drowned in pulls). Keep items from the front while the
/// rendered reply fits. Returns `(kept, truncated)`.
///
/// MEASURED ON THE SERIALIZED FORM, with headroom for the envelope the verb
/// wraps around these items (`total`, `next`, …). JSON escaping inflates raw
/// text by up to ~30% on newline-heavy bodies; the margin is what keeps "fits"
/// true at the seam. Python `_fit_reply`.
#[must_use]
pub fn fit_reply(items: &[Value], cap_tokens: i64, limit: Option<i64>) -> (Vec<Value>, bool) {
    let cap = (cap_tokens as f64 * 0.8) as i64;
    let mut kept: Vec<Value> = Vec::new();
    for it in items {
        // SIGNED, as python's `len(kept) >= limit`: a NEGATIVE limit is already
        // satisfied by the empty page, so it returns one — `as u64`/`as usize`
        // turned `-1` into "no limit" and served every row that fit.
        if limit.is_some_and(|l| kept.len() as i64 >= l) {
            return (kept, true);
        }
        let mut trial = kept.clone();
        trial.push(it.clone());
        let rendered = py_json_dumps(&Value::Array(trial.clone()), true, None);
        if estimate_one(&json!({"role": "user", "content": rendered})) > cap {
            return (kept, true);
        }
        kept = trial;
    }
    (kept, false)
}
