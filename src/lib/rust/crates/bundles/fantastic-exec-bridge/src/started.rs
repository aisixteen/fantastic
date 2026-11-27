//! A CALLER'S VIEW OF `start` (#795): the call SETTLES WHEN ITS JOB EXITS, so an
//! operator that needs the job while it runs (a terminal pane, a test) learns it from
//! the agent's `job_started` announcement — read off a PRIVATE watcher's inbox, since the
//! agent's own inbox is its readers' — never from a reply that waits.

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// The start's own settle, as the private watcher reads it.
const SETTLED: &str = "start_settled";

/// `start` `payload` on `agent` AS THE OPERATOR: the `job_started` payload that named
/// its job (`{type, job_id, command, args}`); the start's own settle (the job's end)
/// comes later and is let go. A start that settles first — refused, or its job already
/// ended — is the `Err`, that settle's result.
///
/// # Errors
/// The watch was refused, or the start settled before any announcement.
pub async fn start_announced(kernel: &Arc<Kernel>, agent: &AgentId, payload: Value) -> Result<Value, Value> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let watcher = AgentId::from(format!("{agent}/started-{n}"));
    let mut rx = kernel.ensure_inbox(&watcher);
    if let Some(denied) = fantastic_kernel::send::as_operator(kernel.watch(agent, watcher.clone())).await {
        kernel.remove_inbox(&watcher);
        return Err(denied);
    }
    // THE START IS A CALL (#795): its settle comes as one more message on the private
    // inbox (`{type: settled, result}`), so one stream of messages says which came first
    let (k, w, item) = (Arc::clone(kernel), watcher.clone(), Item::new(agent.clone(), payload));
    fantastic_kernel::send::as_operator(kernel.call_then(item, move |out| async move {
        if let Some(q) = k.inboxes.get(&w).map(|e| e.clone()) {
            q.send(fantastic_kernel::inbox::InboxItem::text(json!({"type": SETTLED, "result": out.result})));
        }
    }))
    .await;
    let r = loop {
        match rx.recv().await {
            Some(v) if v.payload.get("type").and_then(Value::as_str) == Some("job_started") => {
                break Ok(v.payload);
            }
            Some(v) if v.payload.get("type").and_then(Value::as_str) == Some(SETTLED) => {
                break Err(v.payload.get("result").cloned().unwrap_or(Value::Null));
            }
            Some(_) => {}
            None => break Err(json!({"error": format!("{agent}: the watch closed before the job started")})),
        }
    };
    kernel.unwatch(agent, &watcher).await;
    kernel.remove_inbox(&watcher);
    r
}
