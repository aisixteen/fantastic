//! Shared low-level helpers: meta access, client sanitisation, time + id
//! minting.
//!
//! The tool-call id minter that used to live here (`mint_tool_call_id`, no
//! callers) moved to [`crate::tool_parse::mint_call_id`]: the parser is the only
//! thing that mints one, and python keeps `_mint_id` inside `tool_parse.py` for
//! the same reason.
//!
//! The blob-channel file I/O that used to live here (`chat_path` / `file_read` /
//! `file_write` / `file_delete`, all four riding the LOADER's `persist_blob` /
//! `load_blob`) is GONE. Chat was its only caller, and chat now lives in an
//! append-only JSONL sidecar (`agents/<self>/chat_<client>.jsonl`) written
//! through the AI's OWN `file_bridge_id` (see [`crate::history`]) —
//! python-canonical, and independent of whether the loader discovered a
//! persistence provider.

use fantastic_kernel::{AgentId, Kernel};
use serde_json::Value;
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

/// Current unix time in fractional seconds.
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The mixer both minters below share: clock nanos ^ a stack address ^ the pid ^
/// a monotonic counter. No `rand` dependency, and the counter is what makes two
/// mints in the SAME nanosecond differ.
fn mix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    nanos ^ stack_ptr ^ std::process::id() as u64 ^ seq
}

/// Opaque id for a single user submission.
pub fn mint_send_id() -> String {
    format!("snd_{:08x}", mix() as u32)
}

/// A FRESH 4-hex token for ONE delivered turn's batch — python's
/// `secrets.token_hex(2)`.
///
/// Per TURN and not derived from the batch's content: two turns that made the
/// same calls used to collide (rust/swift hashed `(dest, count, body lengths)`),
/// which is the exact confusion the token exists to prevent — a model that reads
/// the same file twice in a row saw `1 of 2 in batch 7f3a` both times and could
/// not tell the batches apart (measured 2026-08-24, the re-read loop).
pub fn mint_batch_token() -> String {
    format!("{:04x}", mix() as u16)
}
