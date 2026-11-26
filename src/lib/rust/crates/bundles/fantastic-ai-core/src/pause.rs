//! Pause / resume (#219). A paused AI agent stops its pass in flight and HOLDS every
//! incoming message: each is stored as it arrives, as any message is (#795), and no
//! pass starts. The pause state is a row in the agent's own store, so a restart reads
//! it back: a paused agent boots paused, and the messages it holds are the ones stored
//! after its last answer.
//!
//! Rows (in this agent's store head):
//!   `{kind:"pause", paused_since}`   — the agent is paused from here on
//!   `{kind:"resume", role:"user"}`   — released: ONE pass reads every message held
//!
//! The chat says so on its own inbox, as a `status`: `paused` when the pause verb
//! pauses it and when it boots with its store saying paused; `idle` when a resume
//! has nothing held (held messages, released, move it through a pass's own phases).

use std::sync::Arc;

use fantastic_kernel::bundle::BundleError;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

use crate::events::emit_status;
use crate::helpers::now_secs;
use crate::memory::{head, save_history, store_call, MemResult};
use crate::state::state_for;

const PAUSE: &str = "pause";
const RESUME: &str = "resume";

/// The pause state as the store head shows it.
pub struct PauseView {
    /// Paused right now.
    pub paused: bool,
    /// When the current pause began (`paused_since`), if paused.
    pub since: Option<f64>,
}

/// Read the pause state off the store head: the last `pause`/`resume` row decides
/// whether the agent is paused.
pub async fn view(agent_id: &AgentId, kernel: &Arc<Kernel>) -> MemResult<PauseView> {
    let rows = head(agent_id, kernel).await?;
    let mut out = PauseView {
        paused: false,
        since: None,
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
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The chat's `status` outside a call — `paused` or `idle` — on its own inbox, for
/// the terminal caller (no call of anyone's is running).
async fn announce(agent_id: &AgentId, kernel: &Arc<Kernel>, phase: &str) {
    let client_id = fantastic_kernel::inquiry::default_client_id();
    emit_status(
        kernel,
        &state_for(agent_id),
        agent_id,
        &client_id,
        phase,
        serde_json::Map::new(),
    )
    .await;
}

/// AT BOOT: a chat whose store says it is paused says `paused`. Reads only the last
/// `pause`/`resume` row since the last clear, straight from the store — the head is
/// not loaded, so the boot stays lazy for the conversation.
pub async fn announce_at_boot(agent_id: &AgentId, kernel: &Arc<Kernel>) -> MemResult<()> {
    let res = store_call(
        agent_id,
        kernel,
        json!({
            "type": "read",
            "from_last_kind": "clear",
            "kinds": [PAUSE, RESUME],
            "order": "desc",
            "limit": 1,
        }),
    )
    .await?;
    let last = crate::memory::rows_of(&res, agent_id);
    let paused = last
        .first()
        .and_then(|r| r.get("kind"))
        .and_then(Value::as_str)
        == Some(PAUSE);
    crate::life::set_paused(agent_id, paused);
    if paused {
        announce(agent_id, kernel, "paused").await;
    }
    Ok(())
}

/// `pause` — stop the in-flight generation and hold incoming messages. Already
/// paused: a no-op with the same reply.
pub async fn pause(
    agent_id: &AgentId,
    reason: Option<&str>,
    kernel: &Arc<Kernel>,
) -> Result<Value, BundleError> {
    let v = view(agent_id, kernel).await?;
    if v.paused {
        return Ok(json!({"paused": true, "held": crate::life::unread_count(agent_id)}));
    }
    // The stop writes the `[stopped by …]` marker and closes the open parts.
    let _ = crate::verbs::stop(agent_id, None, kernel).await;
    crate::life::set_paused(agent_id, true);
    save_history(
        agent_id,
        kernel,
        &[json!({
            "kind": PAUSE,
            "paused_since": now_secs(),
            "reason": reason,
        })],
    )
    .await?;
    announce(agent_id, kernel, "paused").await;
    Ok(json!({"paused": true, "held": 0}))
}

/// `resume` — write one resumed row (with the note), then ONE pass reads every message
/// held (#795): nothing is re-sent. Not paused: a no-op.
pub async fn resume(
    agent_id: &AgentId,
    note: Option<&str>,
    kernel: &Arc<Kernel>,
) -> Result<Value, BundleError> {
    let v = view(agent_id, kernel).await?;
    if !v.paused {
        return Ok(json!({"resumed": false, "released": 0}));
    }
    let since = v.since.unwrap_or_else(now_secs);
    let secs = (now_secs() - since).max(0.0);
    let mut content = format!("[resumed after a pause of {secs:.0}s]");
    if let Some(note) = note {
        content.push_str(&format!(" {note}"));
    }
    save_history(
        agent_id,
        kernel,
        &[json!({"kind": RESUME, "role": "user", "content": content})],
    )
    .await?;
    crate::life::set_paused(agent_id, false);
    let released = crate::life::unread_count(agent_id);
    if released == 0 {
        announce(agent_id, kernel, "idle").await;
    } else {
        crate::life::wake(kernel, agent_id);
    }
    Ok(json!({"resumed": true, "released": released}))
}
