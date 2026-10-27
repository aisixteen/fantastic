//! Pause / resume (#219). A paused AI agent stops its in-flight generation and
//! HOLDS every incoming message, in order, in its memory store — nothing is run and
//! nothing is dropped. The pause state and the held queue are rows in the agent's
//! own store, so a restart reads them back from the head: a paused agent boots
//! paused with its queue intact.
//!
//! Rows (all in this agent's store head):
//!   `{kind:"pause", paused_since}`   — the agent is paused from here on
//!   `{kind:"held", payload}`         — an incoming message held while paused
//!   `{kind:"resume", role:"user"}`   — released: the held messages after it run

use std::sync::Arc;

use fantastic_kernel::bundle::BundleError;
use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};

use crate::helpers::now_secs;
use crate::memory::{head, save_history, store_of, MemResult};

const PAUSE: &str = "pause";
const RESUME: &str = "resume";
const HELD: &str = "held";

/// The pause state as the store head shows it.
pub struct PauseView {
    /// Paused right now.
    pub paused: bool,
    /// When the current pause began (`paused_since`), if paused.
    pub since: Option<f64>,
    /// Messages held since the last resume, in arrival order.
    pub held: Vec<Value>,
}

/// Read the pause state off the store head. The last `pause`/`resume` row decides
/// whether the agent is paused; the `held` rows after the last `resume` are the queue.
pub async fn view(agent_id: &AgentId, kernel: &Arc<Kernel>) -> MemResult<PauseView> {
    let rows = head(agent_id, kernel).await?;
    let mut out = PauseView {
        paused: false,
        since: None,
        held: Vec::new(),
    };
    for r in &rows {
        match r.get("kind").and_then(Value::as_str) {
            Some(PAUSE) => {
                out.paused = true;
                out.since = r.get("paused_since").and_then(Value::as_f64);
            }
            Some(RESUME) => {
                out.paused = false;
                out.since = None;
                out.held.clear();
            }
            Some(HELD) => {
                if let Some(p) = r.get("payload") {
                    out.held.push(p.clone());
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// THE HOLD CHECK at the top of message intake. Returns `Some(reply)` when the agent
/// is paused: the message is appended to the held queue and not run. `None` means
/// run it. A record with no store is left to the send's own refusal.
pub async fn hold_if_paused(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> Result<Option<Value>, BundleError> {
    if store_of(agent_id, kernel).is_err() {
        return Ok(None);
    }
    let v = view(agent_id, kernel).await?;
    if !v.paused {
        return Ok(None);
    }
    save_history(
        agent_id,
        kernel,
        &[json!({"kind": HELD, "payload": payload})],
    )
    .await?;
    Ok(Some(json!({"held": v.held.len() + 1, "paused": true})))
}

/// `pause` — stop the in-flight generation and hold incoming messages. Already
/// paused: a no-op with the same reply.
pub async fn pause(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> Result<Value, BundleError> {
    let v = view(agent_id, kernel).await?;
    if v.paused {
        return Ok(json!({"paused": true, "held": v.held.len()}));
    }
    // The stop writes the `[stopped by …]` marker and closes the open parts.
    let _ = crate::verbs::stop(agent_id, &json!({}), kernel).await;
    save_history(
        agent_id,
        kernel,
        &[json!({
            "kind": PAUSE,
            "paused_since": now_secs(),
            "reason": payload.get("reason").cloned().unwrap_or(Value::Null),
        })],
    )
    .await?;
    Ok(json!({"paused": true, "held": 0}))
}

/// `resume` — write one resumed row (with the note), then release the held messages
/// in order as if they had just arrived. Not paused: a no-op.
pub async fn resume(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> Result<Value, BundleError> {
    let v = view(agent_id, kernel).await?;
    if !v.paused {
        return Ok(json!({"resumed": false, "released": 0}));
    }
    let since = v.since.unwrap_or_else(now_secs);
    let secs = (now_secs() - since).max(0.0);
    let mut content = format!("[resumed after a pause of {secs:.0}s]");
    if let Some(note) = payload.get("note").and_then(Value::as_str) {
        content.push_str(&format!(" {note}"));
    }
    save_history(
        agent_id,
        kernel,
        &[json!({"kind": RESUME, "role": "user", "content": content})],
    )
    .await?;
    let released = v.held.len();
    let held = v.held;
    // Released as their own kernel task, one after another, so each held turn runs
    // after the resumed row and in arrival order, without holding this dispatch.
    let aid = agent_id.clone();
    let k = Arc::clone(kernel);
    kernel.spawn_task(agent_id, "resume_release", async move {
        for p in held {
            let _ = k.send(vec![Item::new(aid.clone(), p)]).await;
        }
    });
    Ok(json!({"resumed": true, "released": released}))
}
