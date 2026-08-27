//! Shared low-level helpers: meta access, client sanitisation, chat
//! path, file-agent-routed I/O, time + id minting.
//!
//! The tool-call id minter that used to live here (`mint_tool_call_id`, no
//! callers) moved to [`crate::tool_parse::mint_call_id`]: the parser is the only
//! thing that mints one, and python keeps `_mint_id` inside `tool_parse.py` for
//! the same reason.

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Headless / REPL caller default. Matches Python's `DEFAULT_CLIENT_ID`.
pub const DEFAULT_CLIENT_ID: &str = "cli";

/// Read a string meta field off the agent record.
pub fn meta_string(agent_id: &AgentId, kernel: &Kernel, key: &str) -> Option<String> {
    let agent = kernel.agents.get(agent_id).map(|e| Arc::clone(&e))?;
    let meta = agent.meta.read().expect("meta poisoned");
    meta.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Read a string meta field, falling back to `default`.
pub fn meta_string_or(agent_id: &AgentId, kernel: &Kernel, key: &str, default: &str) -> String {
    meta_string(agent_id, kernel, key).unwrap_or_else(|| default.to_string())
}

/// The bound file agent id, if any.
pub fn file_bridge_id(self_id: &AgentId, kernel: &Kernel) -> Option<String> {
    meta_string(self_id, kernel, "file_bridge_id")
}

/// A snapshot of the agent record's meta map (the per-agent config). Empty
/// if the agent is gone. Used by the context-budget functions, mirroring
/// the Python reference's `rec` dict.
pub fn agent_meta(agent_id: &AgentId, kernel: &Kernel) -> serde_json::Map<String, Value> {
    match kernel.agents.get(agent_id) {
        Some(e) => e.meta.read().expect("meta poisoned").clone(),
        None => serde_json::Map::new(),
    }
}

/// Trim + sanitise a client id so it's safe as a filename suffix.
/// Spaces / slashes / weirdness collapse to underscores; capped at 64.
pub fn safe_client(client_id: &str) -> String {
    let trimmed = client_id.trim();
    let raw = if trimmed.is_empty() {
        DEFAULT_CLIENT_ID
    } else {
        trimmed
    };
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.len() > 64 {
        out.truncate(64);
    }
    if out.is_empty() {
        DEFAULT_CLIENT_ID.to_string()
    } else {
        out
    }
}

/// Per-client chat thread FILENAME.
///
/// A name, not a path: the LOADER owns where sidecars land, so this bundle no
/// longer computes `agents/<id>/…` or names a provider of its own.
pub fn chat_path(_self_id: &AgentId, client_id: &str) -> String {
    format!("chat_{}.json", safe_client(client_id))
}

/// Read a file via the bound file agent. `None` if no file agent or the
/// reply carries no `content`.
pub async fn file_read(self_id: &AgentId, kernel: &Arc<Kernel>, path: &str) -> Option<String> {
    // Operator authority: reading our OWN sidecar is bookkeeping processed from
    // above, not this agent reaching out.
    let reply = fantastic_kernel::send::with_operator(kernel.send(vec![Item::new(
        AgentId::from("kernel"),
        json!({"type": "load_blob", "agent_id": self_id.0, "name": path}),
    )]))
    .await
    .remove(0)
    .result;
    reply
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Write a file via the bound file agent.
pub async fn file_write(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    path: &str,
    content: &str,
) -> Result<(), String> {
    let reply = fantastic_kernel::send::with_operator(kernel.send(vec![Item::new(
        AgentId::from("kernel"),
        json!({
            "type": "persist_blob",
            "agent_id": self_id.0,
            "name": path,
            "content": content,
        }),
    )]))
    .await
    .remove(0)
    .result;
    if let Some(err) = reply.get("error").and_then(Value::as_str) {
        return Err(err.to_string());
    }
    Ok(())
}

/// CLEAR one of this agent's own sidecars. Returns whether the write landed.
///
/// The blob channel is a dumb BYTE authority with no delete verb — python has
/// none either — so "cleared" means written EMPTY. For the one caller that
/// matters (dropping a stored api key) that is the property wanted: the secret
/// is gone, and a subsequent read sees nothing.
pub async fn file_delete(self_id: &AgentId, kernel: &Arc<Kernel>, path: &str) -> bool {
    let reply = fantastic_kernel::send::with_operator(kernel.send(vec![Item::new(
        AgentId::from("kernel"),
        json!({
            "type": "persist_blob",
            "agent_id": self_id.0,
            "name": path,
            "content": "",
        }),
    )]))
    .await
    .remove(0)
    .result;
    reply.get("ok").and_then(Value::as_bool).unwrap_or(false)
}

/// Current unix time in fractional seconds.
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Opaque id for a single user submission.
pub fn mint_send_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let mix = nanos ^ stack_ptr ^ std::process::id() as u64;
    format!("snd_{:08x}", mix as u32)
}
