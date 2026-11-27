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
use crate::memory::MemResult;
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

/// The pause state off the store head's `rows`: the last `pause`/`resume` row decides
/// whether the agent is paused.
fn view_of(rows: &[Value]) -> PauseView {
    let mut out = PauseView {
        paused: false,
        since: None,
    };
    for r in rows {
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
    out
}

/// The pause state, as a step (#795): the store head read ([`crate::steps::head`]),
/// then [`view_of`].
async fn view<R, F, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<PauseView>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    crate::steps::head(agent_id, kernel, rec, move |rec, rows| next(rec, rows.map(|r| view_of(&r)))).await;
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

/// AT BOOT, as a step (#795): a chat whose store says it is paused says `paused`. Reads
/// only the last `pause`/`resume` row since the last clear, straight from the store —
/// the head is not loaded, so the boot stays lazy for the conversation. The next step
/// gets whether the store says paused; the boot sets it once its reads ran.
pub(crate) async fn announce_at_boot<R, F, Fut>(agent_id: &AgentId, kernel: &Arc<Kernel>, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<bool>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let ask = json!({
        "type": "read",
        "from_last_kind": "clear",
        "kinds": [PAUSE, RESUME],
        "order": "desc",
        "limit": 1,
    });
    let (me, k) = (agent_id.clone(), Arc::clone(kernel));
    crate::steps::rows(agent_id, kernel, ask, rec, move |rec, last| async move {
        let last = match last {
            Ok(l) => l,
            Err(e) => return next(rec, Err(e)).await,
        };
        let paused = last
            .first()
            .and_then(|r| r.get("kind"))
            .and_then(Value::as_str)
            == Some(PAUSE);
        if paused {
            announce(&me, &k, "paused").await;
        }
        next(rec, Ok(paused)).await;
    })
    .await;
}

/// `pause` — stop the in-flight generation and hold incoming messages. Already
/// paused: a no-op with the same reply. The head read, the stop's rows and the pause
/// row are steps (#795); `settle` answers the call.
pub(crate) async fn pause(
    agent_id: &AgentId,
    reason: Option<String>,
    kernel: &Arc<Kernel>,
    settle: crate::steps::Settle,
) {
    let (me, k) = (agent_id.clone(), Arc::clone(kernel));
    let who = crate::verbs::stopper();
    view(agent_id, kernel, (reason, settle), move |(reason, settle), v| async move {
        let v = match v {
            Ok(v) => v,
            Err(e) => return settle.with(Err::<Value, _>(e)),
        };
        if v.paused {
            return settle.with(Ok::<_, BundleError>(
                json!({"paused": true, "held": crate::life::unread_count(&me)}),
            ));
        }
        // The stop writes the `[stopped by …]` marker and closes the open parts.
        let (me2, k2) = (me.clone(), Arc::clone(&k));
        crate::verbs::stop(&me, None, who, &k, (reason, settle), move |(reason, settle), _| async move {
            crate::life::set_paused(&me2, true);
            let row = json!({
                "kind": PAUSE,
                "paused_since": now_secs(),
                "reason": reason,
            });
            let (me3, k3) = (me2.clone(), Arc::clone(&k2));
            crate::steps::append(&me2, &k2, &[row], settle, move |settle, r| async move {
                if let Err(e) = r {
                    return settle.with(Err::<Value, _>(e));
                }
                announce(&me3, &k3, "paused").await;
                settle.with(Ok::<_, BundleError>(json!({"paused": true, "held": 0})));
            })
            .await;
        })
        .await;
    })
    .await;
}

/// `resume` — write one resumed row (with the note), then ONE pass reads every message
/// held (#795): nothing is re-sent. Not paused: a no-op. The head read and the row are
/// steps; `settle` answers the call.
pub(crate) async fn resume(
    agent_id: &AgentId,
    note: Option<String>,
    kernel: &Arc<Kernel>,
    settle: crate::steps::Settle,
) {
    let (me, k) = (agent_id.clone(), Arc::clone(kernel));
    view(agent_id, kernel, (note, settle), move |(note, settle), v| async move {
        let v = match v {
            Ok(v) => v,
            Err(e) => return settle.with(Err::<Value, _>(e)),
        };
        if !v.paused {
            return settle.with(Ok::<_, BundleError>(json!({"resumed": false, "released": 0})));
        }
        let since = v.since.unwrap_or_else(now_secs);
        let secs = (now_secs() - since).max(0.0);
        let mut content = format!("[resumed after a pause of {secs:.0}s]");
        if let Some(note) = note {
            content.push_str(&format!(" {note}"));
        }
        let row = json!({"kind": RESUME, "role": "user", "content": content});
        let (me2, k2) = (me.clone(), Arc::clone(&k));
        crate::steps::append(&me, &k, &[row], settle, move |settle, r| async move {
            if let Err(e) = r {
                return settle.with(Err::<Value, _>(e));
            }
            crate::life::set_paused(&me2, false);
            let released = crate::life::unread_count(&me2);
            if released == 0 {
                announce(&me2, &k2, "idle").await;
            } else {
                crate::life::wake(&k2, &me2);
            }
            settle.with(Ok::<_, BundleError>(json!({"resumed": true, "released": released})));
        })
        .await;
    })
    .await;
}
