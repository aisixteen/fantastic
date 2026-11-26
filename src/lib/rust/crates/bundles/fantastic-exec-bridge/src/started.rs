//! A CALLER'S VIEW OF `start` (#795): the call SETTLES WHEN ITS JOB EXITS, so an
//! operator that needs the job while it runs (a terminal pane, a test) learns it from
//! the agent's `job_started` announcement — read off a PRIVATE watcher's inbox, since the
//! agent's own inbox is its readers' — never from a reply that waits.

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

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
    let (k, item) = (Arc::clone(kernel), Item::new(agent.clone(), payload));
    let mut settle = tokio::spawn(async move { k.operator_send(vec![item]).await.remove(0).result });
    let r = loop {
        tokio::select! {
            r = &mut settle => break Err(r.unwrap_or_else(|e| json!({"error": e.to_string()}))),
            ev = rx.recv() => match ev {
                Some(v) if v.payload.get("type").and_then(Value::as_str) == Some("job_started") => {
                    break Ok(v.payload);
                }
                Some(_) => {}
                None => break Err(json!({"error": format!("{agent}: the watch closed before the job started")})),
            },
        }
    };
    kernel.unwatch(agent, &watcher).await;
    kernel.remove_inbox(&watcher);
    r
}
