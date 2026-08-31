//! Per-client chat history — an append-only JSONL sidecar on the AI's own leg.
//!
//! One file per client at `agents/<self>/chat_<safe_client>.jsonl`, ONE JSON
//! line per turn, written through the `file_bridge` the AI names in its
//! `file_bridge_id`. The encoding is ai_core's PRIVATE choice as a consumer
//! (this code is its only reader; models get turns through `history`/`recall`)
//! — the substrate imposes no format on anyone.
//!
//! This replaced the `<self>_chat` durable_memory child when durable_memory was
//! deleted (memory is plain FILES now). Line index == store index — load-bearing
//! for `recall.before` and `derive_reaction`, exactly as in python.
//!
//! Mirrors python `ai_core.core._chat_path` / `_load_history` / `_save_history`
//! (python is the immutable reference).

use crate::helpers::{file_bridge_id, safe_client};
use fantastic_kernel::send::with_operator;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// read chunk size on the stream verbs.
const CHUNK: usize = 65536;

/// The chat sidecar's path on the agent's OWN file leg.
fn chat_path(self_id: &AgentId, client_id: &str) -> String {
    format!("agents/{}/chat_{}.jsonl", self_id.0, safe_client(client_id))
}

/// The client's conversation, parsed line-by-line off the sidecar. Empty on a
/// missing leg, a missing file, or unreadable content. A torn tail line (a
/// crash mid-append) is skipped rather than poisoning the whole history.
///
/// BOOKKEEPING, PROCESSED FROM ABOVE — the same standing as the loader's
/// persist_blob and the old chat-store create: the agent does not choose to
/// have a conversation log. Confused-deputy-safe by construction: the target
/// is the record's OWN `file_bridge_id` (operator-set), the verb is literal,
/// and the path is self-scoped (`agents/<self>/…`, client sanitized).
pub async fn load_history(self_id: &AgentId, kernel: &Arc<Kernel>, client_id: &str) -> Vec<Value> {
    let Some(fb) = file_bridge_id(self_id, kernel) else {
        return Vec::new();
    };
    let leg = AgentId::from(fb.as_str());
    let path = chat_path(self_id, client_id);
    let mut raw: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    loop {
        let (r, body) = with_operator(kernel.send_with_binary(
            &leg,
            json!({"type": "read_stream", "path": path, "offset": offset, "length": CHUNK}),
            Vec::new(),
        ))
        .await;
        if !r.is_object() || r.get("error").is_some() {
            break;
        }
        if !body.is_empty() {
            raw.extend_from_slice(&body);
        }
        let eof = r.get("eof").and_then(Value::as_bool).unwrap_or(true);
        if eof || body.is_empty() {
            break;
        }
        offset = r
            .get("next_offset")
            .and_then(Value::as_u64)
            .unwrap_or(offset + body.len() as u64);
    }
    if raw.is_empty() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&raw);
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(Value::is_object)
        .collect()
}

/// APPEND this turn's new messages — one `write_stream` (absent offset =
/// append-at-end, creates the file) per turn, one JSON line each.
///
/// The durable record is never trimmed — the projection only shapes what the
/// model is shown, so compaction and `recall` need the FULL conversation on
/// the next turn. PROCESSED FROM ABOVE — see `load_history`: own leg, literal
/// verb, self-scoped path. Without this every fixture would need a
/// can_call/callable_by pair just so an AI can keep its own history.
pub async fn append_history(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
    new_turns: &[Value],
) {
    if new_turns.is_empty() {
        return;
    }
    let Some(fb) = file_bridge_id(self_id, kernel) else {
        return;
    };
    let leg = AgentId::from(fb.as_str());
    let path = chat_path(self_id, client_id);
    for turn in new_turns {
        let mut line = match serde_json::to_string(turn) {
            Ok(s) => s,
            Err(_) => continue,
        };
        line.push('\n');
        let _ = with_operator(kernel.send_with_binary(
            &leg,
            json!({"type": "write_stream", "path": path}),
            line.into_bytes(),
        ))
        .await;
    }
}
