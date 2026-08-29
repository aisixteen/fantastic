//! Per-client chat history load/save (`chat_{client}.json`), routed
//! through the bound file agent.

use crate::helpers::{chat_path, file_read, file_write};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::Value;
use std::sync::Arc;

/// Load a client's persisted chat. Empty vec on missing / unparseable.
pub async fn load_history(self_id: &AgentId, kernel: &Arc<Kernel>, client_id: &str) -> Vec<Value> {
    let path = chat_path(self_id, client_id);
    let Some(raw) = file_read(self_id, kernel, &path).await else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<Value>>(&raw).unwrap_or_default()
}

/// APPEND turns to a client's chat, preserving everything already stored.
///
/// The durable record is never trimmed — the projection shapes only what the
/// model is shown, so compaction and `recall` need the FULL conversation on the
/// next turn. This exists because the call site used to hand `save_history` a
/// freshly-built two-element vec holding just the current user+assistant pair,
/// which OVERWROTE the file: every rust agent came back to a history exactly
/// two messages long, lost its context after one exchange, and abandoned any
/// task needing more than a single pass. Python appends through the `chat`
/// yaml_state's `append` verb; this is the same semantics for the file store.
pub async fn append_history(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
    new_turns: &[Value],
) -> Result<(), String> {
    if new_turns.is_empty() {
        return Ok(());
    }
    let mut all = load_history(self_id, kernel, client_id).await;
    all.extend_from_slice(new_turns);
    save_history(self_id, kernel, client_id, &all).await
}

/// Persist a client's chat WHOLESALE (pretty JSON) — replaces what is stored.
/// Callers adding turns want [`append_history`]; this is for a caller that has
/// already computed the complete list (e.g. a compaction rewrite).
pub async fn save_history(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
    messages: &[Value],
) -> Result<(), String> {
    let path = chat_path(self_id, client_id);
    let body = serde_json::to_string_pretty(messages).map_err(|e| format!("serialize: {e}"))?;
    file_write(self_id, kernel, &path, &body).await
}
