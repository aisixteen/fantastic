//! Event routing + the status phase machine.
//!
//! Per the LLM contract, stream events go to the originating caller ONLY. There
//! is ONE rule for how (python `_to_caller`): the `cli` caller round-trips
//! through the `cli` agent so its handler prints to stdout, and every other
//! caller gets the event on the BACKEND's inbox tagged with its `client_id` —
//! the browser's WS subscription mirrors that inbox and filters to its own id.
//! No fan-out: cli does not see browser tokens, and vice versa.

use crate::helpers::now_secs;
use crate::state::BackendState;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Route a streaming event to the originating caller's inbox — python
/// `_to_caller`, the ONE rule.
///
/// This used to be selected per backend by a `CallerRoute` knob: ollama
/// round-tripped `cli` and the other three always emitted on the agent's inbox.
/// Python has no such knob, so a `cli` caller on three of the four backends
/// never reached the `cli` handler and printed nothing.
pub async fn to_caller(kernel: &Arc<Kernel>, self_id: &AgentId, client_id: &str, mut ev: Value) {
    if let Value::Object(ref mut obj) = ev {
        obj.insert("client_id".to_string(), json!(client_id));
        obj.entry("source")
            .or_insert_with(|| Value::String(self_id.as_str().to_string()));
    }
    if client_id == crate::helpers::DEFAULT_CLIENT_ID {
        let _ = kernel
            .send(vec![Item::new(AgentId::from("cli"), ev)])
            .await
            .remove(0)
            .result;
    } else {
        // ON THE AGENT'S INBOX, tagged with `client_id` — not on an inbox named
        // after the client. A synthetic client id has no Agent, so it has no
        // watcher set, and a watcher that subscribed to the AGENT (which is what
        // a `watch` frame names) never saw these events at all: measured
        // 2026-09-10, `compaction:rust` fired its projection and pushed nothing
        // the harness could observe.
        kernel.emit(self_id, ev).await;
    }
}

/// Broadcast a structured `status` event AND update the current entry's
/// phase so the on-demand `status` verb stays in sync.
pub async fn emit_status(
    kernel: &Arc<Kernel>,
    state: &Arc<BackendState>,
    self_id: &AgentId,
    client_id: &str,
    phase: &str,
    extra_detail: Map<String, Value>,
) {
    let (send_id, started_at) = {
        let mut cur = state.current_meta.lock().expect("current poisoned");
        if let Some(c) = cur.as_mut() {
            c.phase = phase.to_string();
            (Some(c.send_id.clone()), Some(c.started_at))
        } else {
            (None, None)
        }
    };
    let queue_depth = state.queue.lock().expect("queue poisoned").len();
    let mut detail = extra_detail;
    if let Some(sid) = send_id {
        detail.entry("send_id").or_insert(json!(sid));
    }
    if let Some(t) = started_at {
        detail.entry("started_at").or_insert(json!(t));
    }
    detail
        .entry("queue_depth")
        .or_insert(json!(queue_depth as u64));
    let ev = json!({
        "type": "status",
        "source": self_id.as_str(),
        "phase": phase,
        "detail": Value::Object(detail),
        "ts": now_secs(),
    });
    to_caller(kernel, self_id, client_id, ev).await;
}
