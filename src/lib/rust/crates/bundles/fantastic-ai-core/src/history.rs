//! Per-client chat history, held in the AI's own `<self>_chat` store.
//!
//! The store is a `durable_memory` CHILD of the AI agent — one key per
//! `client_id`, holding that client's message list. It persists through the
//! SAME `file_bridge` leg the AI names in its `file_bridge_id`, so the AI
//! writes no disk itself and nothing here depends on the loader.
//!
//! This replaced the loader BLOB channel (`persist_blob`/`load_blob` on a
//! `chat_<client>.json` sidecar). That path routed every chat write through
//! whatever persistence provider the loader had DISCOVERED, so on a kernel
//! with no `.fantastic`-rooted store (`persistence.provider = null`) each write
//! vanished silently and `history` stayed empty forever while `send` reported
//! success. The blob channel itself stays — it is still the loader's own — but
//! chat no longer rides it. Mirrors python `ai_core.core._ensure_chat_agent` /
//! `_load_history` / `_save_history`.

use crate::helpers::{file_bridge_id, safe_client};
use fantastic_kernel::send::with_operator;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// `handler_module` the chat store runs. The durable_memory bundle must be
/// registered for the store to answer — an unregistered bundle still gets a
/// record, it just never dispatches.
const CHAT_HANDLER: &str = "durable_memory.tools";

/// The AI's chat-store id (`<self>_chat`), keyed by agent id. python's
/// module-level `_chat_agent` dict — a pure lookup cache, always re-validated
/// against the live tree before use.
static CHAT_AGENT: OnceLock<Mutex<HashMap<AgentId, String>>> = OnceLock::new();

fn cache() -> std::sync::MutexGuard<'static, HashMap<AgentId, String>> {
    CHAT_AGENT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("CHAT_AGENT poisoned")
}

/// The AI's chat store — a `durable_memory` child holding its durable
/// conversation log. Discovered by id among this agent's children, or created
/// (idempotent + cached). `None` only if creation fails.
///
/// The id is DETERMINISTIC (`<self>_chat`), so finding it back is a lookup, not
/// a scan for a handler_module/mode pair.
pub async fn ensure_chat_agent(self_id: &AgentId, kernel: &Arc<Kernel>) -> Option<String> {
    let cached = cache().get(self_id).cloned();
    if let Some(cached) = cached {
        if kernel.agents.contains_key(&AgentId::from(cached.as_str())) {
            return Some(cached);
        }
        cache().remove(self_id);
    }
    let want = format!("{}_chat", self_id.0);
    if kernel.agents.contains_key(&AgentId::from(want.as_str())) {
        cache().insert(self_id.clone(), want.clone());
        return Some(want);
    }
    // BOOKKEEPING, PROCESSED FROM ABOVE — the same standing the blob channel
    // had. This store is the AI's own conversation state, not an operator
    // composition decision, and the agent does not choose to have one. Without
    // operator authority every AI agent would need an explicit `create_agent`
    // grant on the root just to keep its history.
    let r = with_operator(kernel.send(vec![Item::new(
        AgentId::from("kernel"),
        json!({
            "type": "create_agent",
            // The store is MINE — a child of this AI agent, reached through the
            // `$children` baseline with no grant. `create_agent` is kernel-owned,
            // so the placement is STATED rather than implied by the address.
            "parent_id": self_id.0,
            "id": want,
            "handler_module": CHAT_HANDLER,
            // The store persists through the SAME file leg as its owner —
            // durable_memory has no fs of its own. Absent here means absent
            // there: the store is still created, and its writes failfast
            // exactly as durable_memory's do. No fallback is invented.
            "file_bridge_id": file_bridge_id(self_id, kernel),
            // SAY WHAT IT IS. A type-derived id advertises "key/value store"
            // loudly, and a model asked to use "this kernel's key/value store"
            // will pick it (measured in python 2026-08-24). An internal agent
            // that looks general-purpose is a discoverability defect.
            "description": format!(
                "Internal conversation store for the AI agent `{}` — one key per \
                 client_id, holding that client's message history. NOT \
                 general-purpose memory: do not read or write it to store facts, \
                 results or notes. It is written by the agent's own turn loop.",
                self_id.0
            ),
        }),
    )]))
    .await
    .remove(0)
    .result;
    let cid = r.get("id").and_then(Value::as_str).map(str::to_string)?;
    cache().insert(self_id.clone(), cid.clone());
    Some(cid)
}

/// The client's conversation, read from the chat store (key-per-client).
/// Empty on a missing store, a missing key, or a non-list value.
pub async fn load_history(self_id: &AgentId, kernel: &Arc<Kernel>, client_id: &str) -> Vec<Value> {
    let Some(cid) = ensure_chat_agent(self_id, kernel).await else {
        return Vec::new();
    };
    let r = kernel
        .send(vec![Item::new(
            AgentId::from(cid.as_str()),
            json!({"type": "read", "key": safe_client(client_id)}),
        )])
        .await
        .remove(0)
        .result;
    match r.get("value") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    }
}

/// APPEND this turn's new messages to the chat store (key-per-client).
///
/// The durable record is never trimmed — the projection only shapes what the
/// model is shown, so compaction and `recall` need the FULL conversation on the
/// next turn.
///
/// ONE `append` PER TURN: `append` adds its value as ONE element, so handing it
/// the whole list nests the turns (`[[user, assistant]]`) and history reads back
/// empty of messages. Python carries the same note for the same reason.
pub async fn append_history(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    client_id: &str,
    new_turns: &[Value],
) {
    if new_turns.is_empty() {
        return;
    }
    let Some(cid) = ensure_chat_agent(self_id, kernel).await else {
        return;
    };
    let key = safe_client(client_id);
    for turn in new_turns {
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from(cid.as_str()),
                json!({"type": "append", "key": key, "value": turn}),
            )])
            .await;
    }
}
