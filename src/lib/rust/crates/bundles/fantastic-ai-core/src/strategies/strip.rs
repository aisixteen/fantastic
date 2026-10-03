//! `strip` — the context is the ledger at MIXED RESOLUTION. Bodies go, titles stay.
//!
//! Every call the model makes carries a one-sentence `title` it wrote at the
//! moment it made the call. The reply comes back tagged with that title. So when
//! the window fills, the cheapest safe move is not to drop TURNS but to drop
//! reply BODIES: the 6.8KB read becomes one line — its tag, its title, its size,
//! its pid — and stays addressable by `recall`. Every assistant turn survives
//! untouched, which matters more than it looks: the model's own prose after a
//! reply IS its distillation of that reply, written with the task in mind, and
//! it is tiny.
//!
//! ```text
//! L0  full row  [reply to pid · notes read · "survey chunk 2"]\n<6800 bytes>
//! L1  stub      [reply to pid · notes read · "survey chunk 2"] (stripped: 6800 bytes)
//! ```
//!
//! TWO RULES THAT KEEP THE KV CACHE WARM. Measured on this stack (vLLM /metrics):
//! an append after the cached prefix reuses it in full; any edit inside it reuses
//! NOTHING. So —
//!   * HYSTERESIS. Strip only past the HIGH watermark, and strip down to LOW in
//!     one batch: ten stubs in one event is one cache miss, not ten.
//!   * MONOTONE, OLDEST FIRST. Once stripped, always stripped, and the stripped
//!     region is a contiguous run from the front — the stable prefix only ever
//!     EXTENDS, never gets holes.
//!
//! No LLM call. If every body is stripped and the prose alone still overflows,
//! the second stage is the cut — not a fallback, the next stage of the same
//! strategy, and it says so in the artifact.
//!
//! Mirrors python `ai_core/strategies/strip.py`.

use super::{fit_tail, Projection, ProjectionCtx};
use crate::constants::NOTICE_ENVELOPE_RESERVE;
use crate::context::{estimate_one, estimate_tokens};
use crate::history_view::reply_blocks;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

pub use crate::constants::LOW;

/// Per-agent set of stripped pids, as the TAG writes them. In-memory,
/// process-lifetime — a strip is a VIEW decision and the durable store is
/// untouched, so losing this on restart costs one re-strip and nothing else.
pub(crate) fn stripped_state() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static S: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// TEST SEAM: forget an agent's stripped set. `context_recall` drops RAM scratch
/// the same way python's `_strip_state.pop` does.
pub fn forget(self_id: &str) {
    stripped_state()
        .lock()
        .expect("strip state poisoned")
        .remove(self_id);
}

/// The pid off a reply block's first line: `[reply to <pid> · …`. Python's
/// `^\[reply to (\S+) · ` — a hand scan, this crate has no regex.
#[must_use]
pub fn pid_of(block: &str) -> Option<&str> {
    let rest = block.strip_prefix("[reply to ")?;
    let end = rest.find(char::is_whitespace)?;
    let (pid, after) = rest.split_at(end);
    if pid.is_empty() || !after.starts_with(" · ") {
        return None;
    }
    Some(pid)
}

/// A reply block reduced to its stub: the tag line, then what it cost.
#[must_use]
pub fn stub(block: &str) -> String {
    // CODE POINTS: python prints `len(body)`, and a byte count reported a
    // non-ASCII body as bigger than the model's own `pull` of it.
    match block.split_once('\n') {
        Some((head, body)) => format!(
            "{head} (stripped: {} bytes — recall by pid)",
            body.chars().count()
        ),
        None => format!("{block} (stripped: 0 bytes — recall by pid)"),
    }
}

/// A delivered reply turn — the only rows whose bodies strip.
#[must_use]
pub fn is_reply(m: &Value) -> bool {
    m.get("role").and_then(Value::as_str) == Some("user")
        && m.get("content")
            .and_then(Value::as_str)
            .is_some_and(|c| c.starts_with("[reply to "))
}

/// Project `body` with every stripped block reduced to its stub. Deterministic
/// in (body, stripped) — the same inputs render the same bytes, which is what
/// keeps the prefix stable between strip events.
fn render(body: &[Value], done: &HashSet<String>) -> (Vec<Value>, usize, i64) {
    let mut out = Vec::with_capacity(body.len());
    // `saved` is SIGNED in python (`len(b) - len(stub)`): a tiny block's stub is
    // LONGER than the block, and saturating at 0 reported a strip that cost
    // bytes as one that saved none.
    let (mut n, mut saved) = (0usize, 0i64);
    for m in body {
        if !is_reply(m) {
            out.push(m.clone());
            continue;
        }
        let content = m.get("content").and_then(Value::as_str).unwrap_or("");
        let mut parts: Vec<String> = Vec::new();
        for b in reply_blocks(content) {
            match pid_of(b) {
                Some(p) if done.contains(p) => {
                    let s = stub(b);
                    n += 1;
                    saved += b.chars().count() as i64 - s.chars().count() as i64;
                    parts.push(s);
                }
                _ => parts.push(b.to_string()),
            }
        }
        let mut row = m.clone();
        if let Some(o) = row.as_object_mut() {
            o.insert("content".to_string(), json!(parts.join("\n\n")));
        }
        out.push(row);
    }
    (out, n, saved)
}

/// The back half — strip's second stage, what happens when every body is
/// already a stub and the prose alone still overflows.
fn halve(body: &[Value]) -> Vec<Value> {
    body[body.len() / 2..].to_vec()
}

/// SEEDED FROM THE VIEW ARRAY on first use — stubs survive a restart, and the
/// prefix bytes are the same before and after it.
async fn seed(
    ctx: &ProjectionCtx<'_>,
    body: &[Value],
) -> Result<HashSet<String>, crate::memory::MemoryStoreError> {
    let mut out = HashSet::new();
    let Some(view) = crate::memory::load_view(ctx.self_id, ctx.kernel).await? else {
        return Ok(out);
    };
    let Some(ids) = view.get("ids").and_then(Value::as_array) else {
        return Ok(out);
    };
    let stubbed: HashSet<i64> = ids
        .iter()
        .filter(|e| e.get("res").and_then(Value::as_str) == Some("stub"))
        .filter_map(|e| e.get("id").and_then(Value::as_i64))
        .collect();
    for m in body {
        let Some(id) = m.get("id").and_then(Value::as_i64) else {
            continue;
        };
        if !stubbed.contains(&id) || !is_reply(m) {
            continue;
        }
        let content = m.get("content").and_then(Value::as_str).unwrap_or("");
        for b in reply_blocks(content) {
            if let Some(p) = pid_of(b) {
                out.insert(p.to_string());
            }
        }
    }
    Ok(out)
}

/// Reduce delivered reply BODIES to their stubs, oldest first, down to the low
/// watermark; cut only when the prose alone still overflows.
pub async fn strip(
    body: &[Value],
    ctx: &ProjectionCtx<'_>,
) -> Result<Projection, crate::memory::MemoryStoreError> {
    if body.len() <= 1 {
        return Ok(Projection {
            body: fit_tail(body, ctx.budget),
            ..Default::default()
        });
    }
    let key = ctx.self_id.as_str().to_string();
    let seeded = stripped_state()
        .lock()
        .expect("strip state poisoned")
        .contains_key(&key);
    if !seeded {
        let s = seed(ctx, body).await?;
        stripped_state()
            .lock()
            .expect("strip state poisoned")
            .entry(key.clone())
            .or_insert(s);
    }
    let mut done: HashSet<String> = stripped_state()
        .lock()
        .expect("strip state poisoned")
        .get(&key)
        .cloned()
        .unwrap_or_default();

    let budget = ctx.budget - NOTICE_ENVELOPE_RESERVE;
    let (mut rendered, mut n, mut saved) = render(body, &done);
    if estimate_tokens(&rendered) > budget {
        // OVER THE HIGH WATERMARK: strip oldest-first, in ONE batch, down to LOW.
        let target = (budget as f64 * LOW) as i64;
        for m in &body[1..body.len() - 1] {
            if estimate_tokens(&rendered) <= target {
                break;
            }
            if !is_reply(m) {
                continue;
            }
            let content = m.get("content").and_then(Value::as_str).unwrap_or("");
            for b in reply_blocks(content) {
                if let Some(p) = pid_of(b) {
                    done.insert(p.to_string());
                }
            }
            let r = render(body, &done);
            rendered = r.0;
            n = r.1;
            saved = r.2;
        }
        stripped_state()
            .lock()
            .expect("strip state poisoned")
            .insert(key, done);
    }
    let mut cut = false;
    if estimate_tokens(&rendered) > budget {
        // Everything strippable is stripped and the prose alone still overflows.
        // Stage two: the cut. The goal turn is pinned.
        let first = rendered[0].clone();
        let head_cost = estimate_one(&first);
        let tail = fit_tail(&halve(&rendered[1..]), (budget - head_cost).max(0));
        let mut out = vec![first];
        out.extend(tail);
        rendered = out;
        cut = true;
    }
    Ok(Projection {
        body: rendered,
        summary: None,
        omitted_marker: cut,
        stripped: n,
        stripped_bytes: saved,
        folded: 0,
        fold_refused: 0,
    })
}
