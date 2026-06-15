//! Context-window budgeting primitives for the overflow strategies — a
//! char-based token ESTIMATE, deliberately tokenizer-agnostic (exactness
//! is irrelevant for a fit-to-window budget, and tiktoken would be the
//! WRONG tokenizer for gemma/nemotron). Plus window/budget resolution off
//! the agent record meta. Mirrors the Python `ai_core/context.py`.

use serde_json::{Map, Value};

/// ~chars per token for the estimate.
pub const CHARS_PER_TOKEN: usize = 4;
/// Conservative default window when nothing is configured.
pub const DEFAULT_CONTEXT_WINDOW: i64 = 4096;
/// Default output headroom reserved out of the window.
pub const DEFAULT_OUTPUT_RESERVE: i64 = 1024;
/// Never project to a budget below this.
pub const BUDGET_FLOOR: i64 = 256;
/// Fixed token budget reserved at the seam for the notice WRAPPER prose
/// (everything in the notice except the summary the strategy already
/// pays for). Keeps ONE budget authority.
pub const NOTICE_ENVELOPE_RESERVE: i64 = 80;

/// Rough token estimate for ONE message — counts the SERIALIZED form,
/// because role + content + tool_calls + role:tool replies + the JSON
/// envelope all consume real context.
pub fn estimate_one(message: &Value) -> i64 {
    let n = serde_json::to_string(message).map(|s| s.len()).unwrap_or(0);
    n.div_ceil(CHARS_PER_TOKEN) as i64
}

/// Sum of [`estimate_one`] across a message slice.
pub fn estimate_tokens(messages: &[Value]) -> i64 {
    messages.iter().map(estimate_one).sum()
}

/// Read a positive integer meta value (number or numeric string); `None`
/// otherwise (bools and non-positive values are rejected).
fn as_pos_int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Bool(_)) => None,
        Some(Value::Number(n)) => {
            let i = n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))?;
            (i > 0).then_some(i)
        }
        Some(Value::String(s)) => s.trim().parse::<i64>().ok().filter(|&i| i > 0),
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

/// Output headroom reserved out of the window (default 1024).
pub fn output_reserve(meta: &Map<String, Value>) -> i64 {
    match meta.get("output_reserve") {
        Some(Value::Bool(_)) => DEFAULT_OUTPUT_RESERVE,
        Some(Value::Number(n)) => n
            .as_i64()
            .filter(|&i| i >= 0)
            .unwrap_or(DEFAULT_OUTPUT_RESERVE),
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .ok()
            .unwrap_or(DEFAULT_OUTPUT_RESERVE),
        _ => DEFAULT_OUTPUT_RESERVE,
    }
}

/// Token budget for the INPUT (window minus output headroom), floored.
pub fn budget(meta: &Map<String, Value>) -> i64 {
    (resolve_context_window(meta) - output_reserve(meta)).max(BUDGET_FLOOR)
}

/// The agent's configured `recent_n` (verbatim recent turns kept by
/// compact/truncate), clamped to [1, 50]; default 6.
pub fn recent_n(meta: &Map<String, Value>) -> usize {
    let n = match meta.get("recent_n") {
        Some(Value::Number(num)) => num.as_i64().unwrap_or(6),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(6),
        _ => 6,
    };
    n.clamp(1, 50) as usize
}
