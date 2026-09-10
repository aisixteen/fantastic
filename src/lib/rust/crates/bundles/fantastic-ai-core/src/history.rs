//! The agent's conversation — rows in the KERNEL's message store.
//!
//! AGENT ID IS CONVERSATION ID. A second conversation is a second agent, so
//! there is no client dimension here: `history`, `recall` and the model view all
//! read the same list whoever asks. Python's law, ported 2026-09-10 — until then
//! this was a per-client JSONL sidecar at `agents/<self>/chat_<client>.jsonl` on
//! the AI's own file leg, and two clients on one agent kept two threads that
//! could not see each other.
//!
//! What is stored is the CONVERSATION: a turn, a delivered reply, a fold mark, a
//! recall decision. The kernel's own bookkeeping — a `call` row per batch item,
//! a `task` row per fork, a `body` row per dehydrated reply, a `view` row per
//! projection — lives in the same table and is filtered out here.
//!
//! LIST INDEX IS STORE INDEX, and it is load-bearing: `recall.before` pages
//! backward by it and `projection::derive_reaction` marks a compaction with it.
//! Both are positions in THIS list, so both hold as long as one list is what
//! every reader sees.
//!
//! Mirrors python `ai_core.core._load_history` / `_save_history` (python is the
//! immutable reference).

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// The kernel's own bookkeeping kinds. In the same table, never the conversation.
const BOOKKEEPING: [&str; 4] = ["call", "task", "body", "view"];

/// The agent's conversation, oldest first.
///
/// Nothing is read from disk here: persistence of the store is the kernel's
/// concern (a loader hydrates it, a persister tails it), never this bundle's.
pub async fn load_history(self_id: &AgentId, kernel: &Arc<Kernel>, _client_id: &str) -> Vec<Value> {
    kernel
        .rows
        .for_agent(self_id)
        .into_iter()
        .filter(|r| {
            !r.get("kind")
                .and_then(Value::as_str)
                .is_some_and(|k| BOOKKEEPING.contains(&k))
        })
        .map(Value::Object)
        .collect()
}

/// Append this pass's turns to the conversation.
///
/// A turn is `{role, content}` plus whatever the caller carries; the store adds
/// the id, the agent and the timestamp. A pre-minted `id` is accepted, which is
/// what lets a turn name itself in the reply tags it sends before its own row
/// exists.
pub async fn append_history(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    _client_id: &str,
    new_turns: &[Value],
) {
    for turn in new_turns {
        let row: Map<String, Value> = match turn {
            Value::Object(m) => m.clone(),
            other => {
                let mut m = Map::new();
                m.insert("content".to_string(), json!(other.to_string()));
                m
            }
        };
        // A refused row is the hydrator's mistake, not the model's — say it and
        // keep the rest of the turn, rather than dropping a conversation.
        if let Err(e) = kernel.rows.put(self_id, row) {
            eprintln!("  [history] {self_id}: {e}");
        }
    }
}
