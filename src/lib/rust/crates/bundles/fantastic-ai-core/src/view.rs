//! THE VIEW ARRAY — the agent's STATE: the ordered ids it sees, each at a
//! resolution. Kernel-held, written by every projection and by `context_recall`,
//! so the next turn (and the next PROCESS) rebuilds the same bytes rather than a
//! freshly-guessed prefix.
//!
//! Mirrors python `ai_core/core.py` `_save_view` / `_load_view` / `_res_of` /
//! `_apply_res`.

use crate::helpers::now_secs;
use crate::history_view::reply_blocks;
use crate::strategies::strip::stub;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Persist what the model sees, and why.
pub fn save_view(self_id: &AgentId, kernel: &Arc<Kernel>, ids: &[Value], budget: i64, why: &str) {
    let mut doc = Map::new();
    doc.insert("v".to_string(), json!(1));
    doc.insert("ts".to_string(), json!(now_secs()));
    doc.insert("budget".to_string(), json!(budget));
    doc.insert("why".to_string(), json!(why));
    doc.insert("ids".to_string(), json!(ids));
    if let Err(e) = kernel.rows.view_put(self_id, doc) {
        eprintln!("  [view] {self_id}: {e}");
    }
}

/// The current view doc, when one has been written and carries an `ids` list.
#[must_use]
pub fn load_view(self_id: &AgentId, kernel: &Arc<Kernel>) -> Option<Map<String, Value>> {
    let doc = kernel.rows.view_get(self_id)?;
    if doc.get("ids").and_then(Value::as_array).is_some() {
        Some(doc)
    } else {
        None
    }
}

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
