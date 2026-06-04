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

/// Persist a client's chat (pretty JSON).
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
