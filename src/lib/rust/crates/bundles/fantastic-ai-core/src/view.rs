//! THE VIEW ARRAY — the agent's STATE: the ordered ids it sees, each at a
//! resolution. A `view` row in the agent's memory store (see
//! [`crate::memory::save_view`]), written by every projection and by
//! `context_recall`, so the next turn (and the next PROCESS) rebuilds the same
//! bytes rather than a freshly-guessed prefix.
//!
//! Mirrors python `ai_core/core.py` `_res_of` / `_apply_res`.

use crate::history_view::reply_blocks;
use crate::strategies::strip::stub;
use serde_json::{json, Value};

/// The resolution a rendered row is at: a fold, a stripped stub, or the row
/// itself.
#[must_use]
pub fn res_of(row: &Value) -> &'static str {
    if row.get("kind").and_then(Value::as_str) == Some("fold") {
        return "fold";
    }
    let c = row.get("content").and_then(Value::as_str).unwrap_or("");
    if c.starts_with("[reply to ") && c.contains("(stripped:") {
        return "stub";
    }
    "full"
}

/// Render a stored row at the resolution the view array asks for.
#[must_use]
pub fn apply_res(row: &Value, res: &str) -> Value {
    if res != "stub" {
        return row.clone();
    }
    let Some(c) = row.get("content").and_then(Value::as_str) else {
        return row.clone();
    };
    if !c.starts_with("[reply to ") {
        return row.clone();
    }
    let stubbed = reply_blocks(c)
        .into_iter()
        .map(stub)
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut out = row.clone();
    if let Some(o) = out.as_object_mut() {
        o.insert("content".to_string(), json!(stubbed));
    }
    out
}
