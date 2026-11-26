//! SETTLE DELIVERY — a call's result is a MESSAGE to its `reply_to`.
//!
//! A call's settle is first a row (the root's `settle` row), then a delivery:
//! - to the CONTINUATION its caller registered ([`Kernel::call`]): `then(outcome)`
//!   runs as a message to that caller (its sends are the caller's), and no task
//!   waits for it;
//! - else to the receiver's `reply` verb, when its bundle declares one: the kernel
//!   runs that handler with `{type: reply, re, target, verb, status, result}` (and
//!   the reply body under `result`) as kernel bookkeeping — no call row and no ack
//!   of its own, so a delivery never makes another delivery;
//! - else nowhere: the settle row is the whole record.
//!
//! ROWS ARE APPEND-ONLY: the settle row is never touched after it is written.
//! Once an owed delivery ran, the kernel APPENDS a `delivered` row naming it
//! (`parent`: the execution, `settle`: the settle row's id). A settle owed to a
//! receiver that has no `delivered` row — the process stopped in between — is
//! delivered ONCE at the next `kernel_start`, which then appends its row
//! ([`Kernel::redeliver`]): at least once across a stop, the receiver dedups by
//! `re`. Continuations live in RAM only: a
//! restart loses them, and their settles arrive as plain `reply` messages.
//!
//! A DEFERRED SETTLE: a handler whose answer comes from another agent's later
//! settle calls [`Kernel::defer`] and returns; its own task ends there. The
//! execution stays open (its `ps` row, its deadline, `cancel`) until
//! [`Kernel::settle_deferred`] gives the outcome. One that comes after the
//! execution already ended (its deadline, a cancel) is late and wakes nothing.

use super::{current_sender, origin_scope, Launched, CURRENT_ORIGIN, CURRENT_SENDER};
use crate::agent::AgentId;
use crate::kernel::Kernel;
use crate::outcome::{Item, Outcome, Status};
use crate::rows::Row;
use crate::tasks::{current_task, Inherited, TaskId, CURRENT_TASK};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A registered continuation: what runs with a call's settle.
pub(crate) type Then = Box<
    dyn FnOnce(Outcome) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send,
>;

/// What a settled execution still owes once its row is written.
pub(crate) struct Owed {
    /// The execution id: the reply's `re`.
    pub(crate) pid: i64,
    /// The agent the call went to.
    pub(crate) target: AgentId,
    /// The call's verb.
    pub(crate) verb: String,
    /// The receiver whose `reply` verb takes the settle (`None`: a continuation
    /// takes it, or nobody does).
    pub(crate) recipient: Option<AgentId>,
    /// The settle row a `delivered` row is appended for once the delivery ran
    /// (`None`: nothing is owed, or no row).
    pub(crate) settle_row: Option<i64>,
}

/// A handler's promise to settle later — [`Kernel::defer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deferred {
    pid: TaskId,
}

impl Deferred {
    /// The execution that settles later: hand it to [`Kernel::settle_deferred`].
    #[must_use]
    pub fn pid(&self) -> TaskId {
        self.pid
    }
}

/// WHO ASKED for the verb a dispatch runs: its `current_origin()` (the operator when
/// that is `None`), captured in that dispatch so a later step of the verb can act for
/// its asker ([`Kernel::call_for`]). Only the kernel's origin scope makes one: a verb can
/// act for its own asker, never name another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asker(Option<AgentId>);

impl Asker {
    /// The asker of the dispatch running here; `None` where no origin scope is set at
    /// all (a bare `tokio::spawn`).
    #[must_use]
    pub fn current() -> Option<Self> {
        origin_scope().map(Self)
    }

    /// The asking agent; `None` for the operator.
    #[must_use]
    pub fn id(&self) -> Option<&AgentId> {
        self.0.as_ref()
    }
}

struct Slot {
    tx: Option<tokio::sync::oneshot::Sender<Outcome>>,
    rx: Option<tokio::sync::oneshot::Receiver<Outcome>>,
}

/// The executions whose handlers deferred their settle, by execution id.
#[derive(Default)]
pub(crate) struct DeferTable {
    inner: Mutex<HashMap<i64, Slot>>,
}

impl DeferTable {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Slot>> {
        self.inner.lock().expect("defer table poisoned")
    }

    /// The deferred settle of `pid` to wait for, once its handler returned:
    /// `None` when the handler did not defer.
    pub(crate) fn waiting(&self, pid: TaskId) -> Option<tokio::sync::oneshot::Receiver<Outcome>> {
        self.lock().get_mut(&pid.0).and_then(|s| s.rx.take())
    }

    /// The execution ended: a settle for it from now on is late.
    pub(crate) fn forget(&self, pid: TaskId) {
        self.lock().remove(&pid.0);
    }
}

impl Kernel {
    /// THE ASYNC CALL: launch `item` and return its ACK at once — `{pid, status:
    /// accepted}` once the pid is minted and the call row written, or the launch's
    /// refusal (a gate denial, a bad item, an unknown target), for which `then`
    /// never runs. The handler runs as its own task; when the call settles,
    /// `then(outcome)` runs as a message to the caller, in place of its `reply`
    /// verb. No task waits for it and no lock is held.
    ///
    /// A named item that joins an execution in flight acks that execution's pid,
    /// and `then` gets its settle; one naming an execution already settled acks
    /// its pid and `then` gets the recorded outcome. Deadlines and retries are the
    /// item's, as for any call. The continuation lives in RAM: a restart loses it.
    pub async fn call<F, Fut>(self: &Arc<Self>, item: Item, then: F) -> Outcome
    where
        F: FnOnce(Outcome) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        if origin_scope().is_none() {
            return Outcome::refused(crate::refusal::unknown_agent(item.target.as_str()));
        }
        let caller = current_sender();
        if self.store_id().is_some() && item.id.is_some() {
            if let Err(e) = self.recall_executions(std::slice::from_ref(&item)).await {
                return Outcome::refused(crate::executions::raised("RuntimeError", &e));
            }
        }
        let launched = self.launch(item, caller.clone());
        let pid = match &launched {
            Launched::Settled(o) => return o.clone(),
            Launched::Recorded(o) => o.pid,
            Launched::Dispatched(d) => Some(d.task_id),
            Launched::Joining(j) => Some(TaskId(j.execution_id)),
            Launched::KernelReflectFast(_) => None,
        };
        let then: Then = Box::new(move |o| Box::pin(then(o)));
        let kernel = Arc::clone(self);
        // the handler is a child of the dispatch that called, as for any send
        let parent = current_task();
        tokio::spawn(Inherited::capture().scope(async move {
            let run = kernel.settle_core(launched, true);
            let (out, owed) = match parent {
                Some(p) => CURRENT_TASK.scope(p, run).await,
                None => run.await,
            };
            kernel.finish(owed, &out, Some(then), caller).await;
        }));
        Outcome::accepted(pid)
    }

    /// THE ASKER'S CALL: [`Self::call`] sent for `asker` — with sender and origin both
    /// the agent that asked for the verb whose dispatch captured it ([`Asker::current`]),
    /// as [`Self::send_as_caller`] sends — so a later step of that verb still acts for
    /// its asker, checked as it. The continuation runs as the agent that makes this
    /// call, as for [`Self::call`].
    pub async fn call_for<F, Fut>(self: &Arc<Self>, asker: &Asker, item: Item, then: F) -> Outcome
    where
        F: FnOnce(Outcome) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let me = current_sender();
        let then = move |out| CURRENT_SENDER.scope(me, then(out));
        let a = asker.0.clone();
        CURRENT_ORIGIN
            .scope(a.clone(), CURRENT_SENDER.scope(a, self.call(item, then)))
            .await
    }

    /// DEFER this dispatch's settle: called by a handler whose answer comes from
    /// another agent's later settle. The handler returns as soon as it has made
    /// that call (its reply is then not the settle); the execution stays open
    /// until [`Self::settle_deferred`] gives the outcome, or its deadline or a
    /// `cancel` ends it. Refused outside a dispatch.
    ///
    /// # Errors
    /// No dispatch is running here (a bookkeeping run included), or this one already
    /// deferred.
    pub fn defer(&self) -> Result<Deferred, Value> {
        if super::IN_BOOKKEEPING.try_with(|b| *b).unwrap_or(false) {
            return Err(crate::refusal::refusal(
                crate::refusal::BAD_ARG,
                "defer: a kernel bookkeeping run answers no call, so it has no settle to defer",
            ));
        }
        let Some(pid) = current_task() else {
            return Err(crate::refusal::refusal(
                crate::refusal::BAD_ARG,
                "defer: only a running dispatch can defer its settle",
            ));
        };
        let mut slots = self.defers.lock();
        if slots.contains_key(&pid.0) {
            return Err(crate::refusal::refusal(
                crate::refusal::BAD_ARG,
                format!("defer: execution {pid} already deferred its settle"),
            ));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        slots.insert(
            pid.0,
            Slot {
                tx: Some(tx),
                rx: Some(rx),
            },
        );
        Ok(Deferred { pid })
    }

    /// SETTLE A DEFERRED EXECUTION with `outcome` (its status and result as given;
    /// the pid is the execution's). `false` when nothing waits for it any more —
    /// it never deferred, it already settled, or its deadline or a cancel ended it:
    /// a late settle wakes nothing.
    pub fn settle_deferred(&self, pid: TaskId, outcome: Outcome) -> bool {
        let tx = self.defers.lock().get_mut(&pid.0).and_then(|s| s.tx.take());
        tx.is_some_and(|tx| tx.send(outcome).is_ok())
    }

    /// Does `id`'s bundle declare the `reply` verb — is it a receiver of settles?
    pub(crate) fn declares_reply(&self, id: &AgentId) -> bool {
        let Some(agent) = self.agents.get(id).map(|e| Arc::clone(&e)) else {
            return false;
        };
        self.local_bundle(&agent).is_some_and(|b| {
            b.verbs().iter().any(|v| v.name == "reply")
                || b.dynamic_verbs(&agent).iter().any(|v| v.name == "reply")
        })
    }

    /// DELIVER a settle: to the continuation, else to the receiver's `reply`
    /// verb; then append the `delivered` row. `caller`: who registered the
    /// continuation (its sends are that agent's).
    pub(crate) async fn finish(
        self: &Arc<Self>,
        owed: Option<Owed>,
        out: &Outcome,
        then: Option<Then>,
        caller: Option<AgentId>,
    ) {
        if let Some(then) = then {
            run_then(then, out.clone(), caller).await;
        } else if let Some(o) = &owed {
            if let Some(r) = &o.recipient {
                self.deliver_reply(r, o.pid, &o.target, &o.verb, out).await;
            }
        }
        if let Some(o) = owed {
            if let Some(row) = o.settle_row {
                self.append_delivered(o.pid, row);
            }
        }
    }

    /// Run `recipient`'s `reply` handler with one settle, as kernel bookkeeping.
    async fn deliver_reply(
        self: &Arc<Self>,
        recipient: &AgentId,
        pid: i64,
        target: &AgentId,
        verb: &str,
        out: &Outcome,
    ) {
        let Some(agent) = self.agents.get(recipient).map(|e| Arc::clone(&e)) else {
            return;
        };
        let payload = json!({
            "type": "reply",
            "re": pid,
            "target": target.as_str(),
            "verb": verb,
            "status": out.status.as_str(),
            "result": out.result,
        });
        let body = out
            .body
            .as_ref()
            .map(|b| crate::outcome::Blob::new(b.nested_in("result"), b.bytes.clone()));
        let r = self.bookkeeping(&agent, payload, body).await;
        if r.status != Status::Completed {
            eprintln!(
                "  [kernel] the settle of {pid} was not taken by {}: {} {}",
                recipient,
                r.status.as_str(),
                r.result
            );
        }
    }

    /// Append the `delivered` row for settle row `settle` of execution `pid`.
    fn append_delivered(&self, pid: i64, settle: i64) {
        let Some(root) = self.root() else { return };
        let mut row = Map::new();
        row.insert("kind".to_string(), Value::from("delivered"));
        row.insert("parent".to_string(), Value::from(pid));
        row.insert("settle".to_string(), Value::from(settle));
        if let Err(e) = self.rows_put(&root.id, row) {
            eprintln!("  [kernel] the delivery of {pid} could not be recorded: {e}");
        }
    }

    /// RE-DELIVERY AT START: every settle owed to a receiver's `reply` verb that
    /// has no `delivered` row — its process stopped before delivering it, or this
    /// start's `settle_cut` just wrote it — is delivered once, and its `delivered`
    /// row appended. Nothing already written is touched.
    pub(crate) async fn redeliver(self: &Arc<Self>) {
        let mut rows: Vec<Row> = Vec::new();
        if self.store_id().is_some() {
            let mut q = Map::new();
            q.insert("kinds".to_string(), json!(["settle", "delivered"]));
            match self.store_read(q).await {
                Ok(r) => rows = r,
                Err(e) => {
                    eprintln!("  [kernel] the undelivered settles could not be read: {e}");
                    return;
                }
            }
        }
        rows.extend(self.rows.all().into_iter().filter(|r| {
            matches!(r.get("kind").and_then(Value::as_str), Some("settle" | "delivered"))
        }));
        let kind = |r: &Row| r.get("kind").and_then(Value::as_str).map(str::to_string);
        let done: std::collections::HashSet<i64> = rows
            .iter()
            .filter(|r| kind(r).as_deref() == Some("delivered"))
            .filter_map(|r| r.get("settle").and_then(Value::as_i64))
            .collect();
        let mut seen = std::collections::HashSet::new();
        let pending: Vec<Row> = rows
            .into_iter()
            .filter(|r| kind(r).as_deref() == Some("settle"))
            .filter(|r| {
                r.get("id")
                    .and_then(Value::as_i64)
                    .is_some_and(|id| !done.contains(&id) && seen.insert(id))
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        // the calls they settle: who receives each, and its verb
        let parents: Vec<i64> = pending
            .iter()
            .filter_map(|r| r.get("parent").and_then(Value::as_i64))
            .collect();
        let mut calls: HashMap<i64, Row> = HashMap::new();
        for p in &parents {
            if let Some(c) = self.rows.get_one(*p) {
                calls.insert(*p, c);
            }
        }
        let missing: Vec<i64> = parents.iter().copied().filter(|p| !calls.contains_key(p)).collect();
        if !missing.is_empty() && self.store_id().is_some() {
            let mut q = Map::new();
            q.insert("kinds".to_string(), json!(["call"]));
            q.insert("ids".to_string(), json!(missing));
            if let Ok(found) = self.store_read(q).await {
                for c in found {
                    if let Some(id) = c.get("id").and_then(Value::as_i64) {
                        calls.insert(id, c);
                    }
                }
            }
        }
        for row in pending {
            let (Some(sid), Some(pid)) = (
                row.get("id").and_then(Value::as_i64),
                row.get("parent").and_then(Value::as_i64),
            ) else {
                continue;
            };
            let Some(call) = calls.get(&pid) else { continue };
            let Some(recipient) = call
                .get("reply_to")
                .and_then(Value::as_str)
                .map(AgentId::from)
                .filter(|r| self.declares_reply(r))
            else {
                continue;
            };
            let target = AgentId::from(
                row.get("settled")
                    .and_then(Value::as_str)
                    .or_else(|| call.get("agent").and_then(Value::as_str))
                    .unwrap_or(""),
            );
            let verb = call.get("verb").and_then(Value::as_str).unwrap_or("").to_string();
            let out = recorded_outcome(pid, &row);
            self.deliver_reply(&recipient, pid, &target, &verb, &out).await;
            self.append_delivered(pid, sid);
        }
    }
}

/// A settle as its row records it ([`crate::ledger::stored_outcome`]): a named
/// call's whole result, else what the kernel keeps of it (a dead letter's error,
/// a size).
fn recorded_outcome(pid: i64, row: &Row) -> Outcome {
    let mut kept = row
        .get("outcome")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let status = kept.remove("status");
    let result = kept.remove("result").unwrap_or(Value::Object(kept));
    let status = match status.as_ref().and_then(Value::as_str) {
        Some("completed") => Status::Completed,
        Some("errored") => Status::Errored,
        Some("cancelled") => Status::Cancelled,
        Some("timed_out") => Status::TimedOut,
        Some("accepted") => Status::Accepted,
        _ => Status::Failed,
    };
    let mut out = Outcome::new(Some(TaskId(pid)), status, result);
    out.attempts = row
        .get("attempts")
        .and_then(Value::as_u64)
        .and_then(|a| u32::try_from(a).ok())
        .unwrap_or(1);
    out
}

/// Run a continuation as a message to `caller`: its sends are the caller's, and
/// the kernel (origin none) delivered it. A panic in it is said, never spread.
async fn run_then(then: Then, out: Outcome, caller: Option<AgentId>) {
    use futures_util::FutureExt;
    let run = CURRENT_ORIGIN.scope(None, CURRENT_SENDER.scope(caller, then(out)));
    if let Err(p) = std::panic::AssertUnwindSafe(run).catch_unwind().await {
        eprintln!(
            "  [kernel] a continuation panicked: {}",
            super::panic_message(&p)
        );
    }
}
