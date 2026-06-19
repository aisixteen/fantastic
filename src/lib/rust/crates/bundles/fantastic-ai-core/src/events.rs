//! Event routing + the status phase machine.
//!
//! Per the LLM contract, stream events go to the originating caller
//! ONLY. The two shipped backends differ on HOW the `cli` caller is
//! reached — that difference is captured by [`CallerRoute`] so a single
//! shared path serves both byte-identically to each backend's contract.

use crate::helpers::now_secs;
use crate::state::BackendState;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// How streaming events reach the originating caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallerRoute {
    /// `client_id == "cli"` round-trips through the `cli` agent
    /// (`kernel.send("cli", ev)`); everyone else gets
    /// `kernel.emit(self_id, ev)` tagged with `client_id`. Matches the
    /// Python reference + the ollama backend.
    CliRoundTrip,
    /// Every event is emitted on `client_id`'s own inbox
    /// (`kernel.emit(client_id, ev)`). Used by the NIM backend, whose
    /// tests drain a per-`client_id` inbox.
    PerClientInbox,
}

/// Route a streaming event to the originating caller's inbox.
pub async fn to_caller(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    client_id: &str,
    route: CallerRoute,
    mut ev: Value,
) {
    if let Value::Object(ref mut obj) = ev {
        obj.insert("client_id".to_string(), json!(client_id));
        obj.entry("source")
            .or_insert_with(|| Value::String(self_id.as_str().to_string()));
    }
    match route {
        CallerRoute::CliRoundTrip => {
            if client_id == crate::helpers::DEFAULT_CLIENT_ID {
                let _ = kernel.send(&AgentId::from("cli"), ev).await;
            } else {
                kernel.emit(self_id, ev).await;
            }
        }
        CallerRoute::PerClientInbox => {
            kernel.emit(&AgentId::from(client_id), ev).await;
        }
    }
}

/// Broadcast a structured `status` event AND update the current entry's
/// phase so the on-demand `status` verb stays in sync.
pub async fn emit_status(
    kernel: &Arc<Kernel>,
    state: &Arc<BackendState>,
    self_id: &AgentId,
    client_id: &str,
    route: CallerRoute,
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
    to_caller(kernel, self_id, client_id, route, ev).await;
}

/// Emit the back-compat `{type:"done"}` end marker.
pub async fn emit_done(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    client_id: &str,
    route: CallerRoute,
) {
    let ev = json!({"type": "done", "source": self_id.as_str()});
    to_caller(kernel, self_id, client_id, route, ev).await;
}
