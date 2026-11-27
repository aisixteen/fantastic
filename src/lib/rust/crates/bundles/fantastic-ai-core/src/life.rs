//! THE AGENT'S LIFE (#795): an AI agent is a machine woken by messages.
//!
//! - An ARRIVAL (a `send`, a reply to a call its answer made, an inquiry, a harness
//!   notice) is stored as a row of its memory first, then put on the agent's `unread`
//!   list ([`arrive`]). A `send` is acked once its row is stored.
//! - A PASS starts when the agent is idle and something is unread ([`wake`]): it takes
//!   every unread message, reads its history, sends its request to the model and its
//!   step ENDS ([`crate::agent_loop::start`]). The request's settle is the next step: a
//!   re-ask, or the answer stored and routed. The pass ends once its answer row is
//!   written ([`end`]); messages that arrived meanwhile are read by exactly ONE next
//!   pass.
//! - Nothing waits: no lock is held across a call, no sender waits for a pass, and no
//!   pass waits for another agent. A pass is a record handed from step to step, never
//!   a task that sleeps.
//!
//! The model decides what to answer, when and to whom: a peer's question is answered by
//! the model's own `send` back (`re` set), never by the harness.

use std::sync::Arc;

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

use crate::helpers::{mint_send_id, now_secs};
use crate::memory::MemResult;
use crate::state::{state_for, Arrival};

/// A message to store and wake on.
pub(crate) struct Incoming {
    /// Its text, as the model reads it (with its `[from …]` header).
    pub(crate) text: String,
    /// The sanitised client id it came with.
    pub(crate) client_id: String,
    /// The sender (the calling agent, else the client id).
    pub(crate) origin: String,
    /// This layer's own text to its own agent: stored as `kind: "notice"`.
    pub(crate) notice: bool,
    /// The images it attached.
    pub(crate) attached: Option<crate::images::Attached>,
}

/// THE ARRIVAL, as a step: `m` stored as a row of `self_id`'s memory, then — once the
/// store took it — put on the unread list and, when `wakes`, a pass started if none
/// runs. The next step gets the row's id, or the store's refusal (nothing was put on
/// the list).
pub(crate) async fn arrive<R, F, Fut>(
    kernel: &Arc<Kernel>,
    self_id: &AgentId,
    m: Incoming,
    wakes: bool,
    rec: R,
    next: F,
) where
    R: Send + 'static,
    F: FnOnce(R, MemResult<i64>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    // THE ROW ID IS MINTED AT THE APPEND: stamped now (its `ts` and UTC offset), its
    // images and its sender on it
    let id = kernel.mint_id();
    let mut row = crate::images::stamped(
        crate::agent_loop::incoming_row(
            &m.text,
            id,
            crate::agent_loop::incoming_parent(&m.text),
            m.notice,
        ),
        m.attached.as_ref(),
    );
    crate::row_attrs::Stamp::now().apply(&mut row);
    let row = crate::row_attrs::with_source(row, &m.origin);
    let (k, me) = (Arc::clone(kernel), self_id.clone());
    // owed while its row is out: the agent is not idle with a message in flight
    owe(self_id, 1);
    crate::steps::append(self_id, kernel, &[row], (rec, m), move |(rec, m), r| async move {
        let r = r.map(|_| id);
        if r.is_ok() && wakes {
            let state = state_for(&me);
            state
                .life
                .lock()
                .expect("life poisoned")
                .unread
                .push(Arrival {
                    row_id: id,
                    client_id: m.client_id,
                    origin: m.origin,
                    text: m.text,
                    send_id: mint_send_id(),
                    at: now_secs(),
                    notice: m.notice,
                });
            state.changed();
            wake(&k, &me);
        }
        owe(&me, -1);
        next(rec, r).await;
    })
    .await;
}

/// TESTS: [`arrive`] driven to its end — the row's id, or the store's refusal.
#[cfg(test)]
pub(crate) async fn arrived(kernel: &Arc<Kernel>, self_id: &AgentId, m: Incoming, wakes: bool) -> MemResult<i64> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    arrive(kernel, self_id, m, wakes, (), move |(), r| async move {
        let _ = tx.send(r);
    })
    .await;
    rx.await
        .unwrap_or_else(|_| Err(crate::memory::MemoryStoreError("the arrival was dropped".into())))
}

/// Harness text to the agent itself (a restart's dead letters, an undelivered inquiry
/// answer): stored as a notice and woken on. A store that refuses it is said.
pub(crate) async fn notice(kernel: &Arc<Kernel>, self_id: &AgentId, text: &str, client_id: &str) {
    let m = Incoming {
        text: text.to_string(),
        client_id: client_id.to_string(),
        origin: self_id.as_str().to_string(),
        notice: true,
        attached: None,
    };
    let me = self_id.clone();
    arrive(kernel, self_id, m, true, (), move |(), r| async move {
        if let Err(e) = r {
            eprintln!("  [ai] {me}: a notice could not be stored: {}", e.0);
        }
    })
    .await;
}

/// WAKE: start ONE pass over everything unread, when no pass runs and the agent is
/// not paused. Its first step runs as a task of the agent's own (a `ps` row while it
/// assembles); it ends when the request is sent.
pub(crate) fn wake(kernel: &Arc<Kernel>, self_id: &AgentId) {
    let state = state_for(self_id);
    let next = {
        let mut life = state.life.lock().expect("life poisoned");
        if life.running || life.paused || life.booting || life.unread.is_empty() {
            None
        } else {
            life.running = true;
            life.epoch += 1;
            Some((life.epoch, std::mem::take(&mut life.unread)))
        }
    };
    let Some((epoch, tail)) = next else { return };
    state.changed();
    let (k, me) = (Arc::clone(kernel), self_id.clone());
    kernel.spawn_task(self_id, "pass", async move {
        crate::agent_loop::start(k, me, epoch, tail).await;
    });
}

/// Whether `epoch` is still the pass in flight.
pub(crate) fn is_current(self_id: &AgentId, epoch: u64) -> bool {
    let state = state_for(self_id);
    let life = state.life.lock().expect("life poisoned");
    life.running && life.epoch == epoch
}

/// THE PASS ENDED (its answer row written, or it failed or was stopped): the in-flight
/// entry goes, and what arrived meanwhile wakes exactly one next pass. A pass a `stop`
/// already ended (an older `epoch`) changes nothing.
pub(crate) fn end(kernel: &Arc<Kernel>, self_id: &AgentId, epoch: u64) {
    let state = state_for(self_id);
    {
        let mut life = state.life.lock().expect("life poisoned");
        if !(life.running && life.epoch == epoch) {
            return;
        }
        life.running = false;
    }
    *state.current_meta.lock().expect("current poisoned") = None;
    state.changed();
    wake(kernel, self_id);
}

/// END THE PASS IN FLIGHT NOW (a `stop`, a `pause`): its request's settle will act on
/// nothing, and what is unread stays stored but wakes nothing. `true` when one ran.
pub(crate) fn abandon(self_id: &AgentId) -> bool {
    let state = state_for(self_id);
    let ran = {
        let mut life = state.life.lock().expect("life poisoned");
        let ran = life.running;
        life.running = false;
        life.epoch += 1;
        life.stops += 1;
        life.unread.clear();
        ran
    };
    state.changed();
    ran
}

/// How many times the agent's work was stopped: a routed reply reads it when the routing
/// decided, and one that lands after a later stop is `late`.
pub(crate) fn stops(self_id: &AgentId) -> u64 {
    let state = state_for(self_id);
    let n = state.life.lock().expect("life poisoned").stops;
    n
}

/// A call an answer sent is in flight (`+1`) or its reply was taken (`-1`).
pub(crate) fn owe(self_id: &AgentId, delta: isize) {
    let state = state_for(self_id);
    {
        let mut life = state.life.lock().expect("life poisoned");
        life.open = life.open.saturating_add_signed(delta);
    }
    state.changed();
}

/// The boot's reads are out (`true`) or have all run: while booting, arrivals are
/// stored and no pass starts.
pub(crate) fn set_booting(self_id: &AgentId, booting: bool) {
    let state = state_for(self_id);
    state.life.lock().expect("life poisoned").booting = booting;
    state.changed();
}

/// Paused (`true`) or resumed: a paused agent stores its arrivals and runs no pass.
pub(crate) fn set_paused(self_id: &AgentId, paused: bool) {
    let state = state_for(self_id);
    state.life.lock().expect("life poisoned").paused = paused;
    state.changed();
}

/// The unread messages among `read` (the rows a pass's history read held) are read
/// by that pass: they leave the unread list.
pub(crate) fn read_by_pass(self_id: &AgentId, read: &[i64]) {
    let state = state_for(self_id);
    let gone = {
        let mut life = state.life.lock().expect("life poisoned");
        let before = life.unread.len();
        let read: std::collections::HashSet<i64> = read.iter().copied().collect();
        life.unread.retain(|a| !read.contains(&a.row_id));
        before != life.unread.len()
    };
    if gone {
        state.changed();
    }
}

/// How many messages are stored and unread.
pub(crate) fn unread_count(self_id: &AgentId) -> usize {
    let state = state_for(self_id);
    let n = state.life.lock().expect("life poisoned").unread.len();
    n
}

/// The kinds of a stored user row that are not a message to read: a late reply, the
/// harness's own records of a pass that ended, the pause rows, a fold's summary and a
/// recall's rebuilt context (the agent's own memory work).
const NOT_ARRIVALS: [&str; 7] = ["late", "withdrawn", "held", "pause", "resume", "fold", "recall"];
/// The kinds of a user row that close a pass: the pass that read the rows before it
/// ended there.
const PASS_ENDS: [&str; 2] = ["error", "stopped"];

/// AT BOOT (#795), as a step: the messages the store holds after the last answer —
/// stored, acked, never read by a pass because the process stopped first — go back on
/// the unread list, so the next wake reads them. Read from the store's tail; the
/// conversation is not loaded. The next step gets how many came back, or the store's
/// refusal of the read.
pub(crate) async fn restore_unread<R, F, Fut>(kernel: &Arc<Kernel>, self_id: &AgentId, rec: R, next: F)
where
    R: Send + 'static,
    F: FnOnce(R, MemResult<usize>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let ask = json!({"type": "read", "from_last_kind": "clear", "order": "desc", "limit": 64});
    let me = self_id.clone();
    crate::steps::rows(self_id, kernel, ask, rec, move |rec, rows| async move {
        next(rec, rows.map(|rows| unread_back(&me, &rows))).await;
    })
    .await;
}

/// The messages among the store's tail `rows` (newest first) that no pass read, put
/// back on the unread list; how many.
fn unread_back(self_id: &AgentId, rows: &[Value]) -> usize {
    let mut back: Vec<Arrival> = Vec::new();
    for r in rows {
        let kind = r.get("kind").and_then(Value::as_str);
        let role = r.get("role").and_then(Value::as_str);
        if role == Some("assistant") || kind.is_some_and(|k| PASS_ENDS.contains(&k)) {
            break;
        }
        let text = r.get("content").and_then(Value::as_str).unwrap_or_default();
        // an ack of a message sent to an AI is stored, never woken on (Q2)
        if role != Some("user")
            || kind.is_some_and(|k| NOT_ARRIVALS.contains(&k))
            || crate::route::is_ack_text(text)
        {
            continue;
        }
        let Some(row_id) = r.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let origin = r
            .pointer("/attributes/source")
            .and_then(Value::as_str)
            .unwrap_or(self_id.as_str());
        back.push(Arrival {
            row_id,
            client_id: fantastic_kernel::inquiry::default_client_id(),
            origin: origin.to_string(),
            text: text.to_string(),
            send_id: mint_send_id(),
            at: now_secs(),
            notice: kind == Some("notice"),
        });
    }
    back.reverse();
    let n = back.len();
    let state = state_for(self_id);
    {
        let mut life = state.life.lock().expect("life poisoned");
        let known: std::collections::HashSet<i64> = life.unread.iter().map(|a| a.row_id).collect();
        let mut merged: Vec<Arrival> = back.into_iter().filter(|a| !known.contains(&a.row_id)).collect();
        merged.append(&mut life.unread);
        life.unread = merged;
    }
    state.changed();
    n
}
