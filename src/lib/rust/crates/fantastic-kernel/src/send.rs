//! Verb routing — `Kernel::send`, `Kernel::emit`, watch fanout, the
//! `_current_sender` task-local that tags state events with the
//! originator.
//!
//! Routing model: the kernel's `agents` map is the only routing table.
//! `send(target_id, payload)` resolves `target_id` directly there;
//! parent-child structure doesn't enter routing. Inboxes are auto-
//! vivified for synthetic ids (browser clients).
//!
//! ### System verbs vs bundle verbs
//!
//! Substrate-native verbs (`create_agent`, `delete_agent`,
//! `update_agent`, `list_agents`, `reflect`, `boot`, `shutdown`) are
//! resolved on the target agent itself without consulting its bundle.
//! Everything else routes to the bundle whose `handler_module` matches
//! the agent's record. Unknown bundle → `{"error": "no bundle for
//! handler_module"}`. Missing target → `{"error": "no agent <id>"}`.
//!
//! ### Watcher fanout
//!
//! Every successful dispatch publishes a state event
//! `{"type":"send", "sender":<originator>, "target":<id>, "verb":<v>,
//! "summary": <stringified-payload>}` and fans the raw payload out to
//! every watcher's inbox. Emits (no reply) do the same with
//! `"type":"emit"`. Errors emit `{"type":"error", ...}`.

use crate::agent::{Agent, AgentId, RESERVED_RECORD_KEYS};
use crate::kernel::Kernel;
use crate::outcome::{Item, Outcome, Status};
use crate::tasks::{
    current_killed, current_task, TaskHandle, TaskId, CURRENT_KILLED, CURRENT_TASK,
};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::task_local;

task_local! {
    /// The originator of the send currently dispatching. `Some(id)` inside an
    /// agent's handler (a nested send is attributed to that agent — the CALL
    /// GATE's sender). `None` = OPERATOR: outside any dispatch (CLI/bootstrap/
    /// test), or explicitly under [`with_operator`] (a verb completing its own
    /// side effect). Also tags telemetry state events with the originator.
    pub static CURRENT_SENDER: Option<AgentId>;
}

/// Get the current sender — `Some(id)` for an agent-originated send, `None` for
/// an operator send (outside any dispatch, or under [`with_operator`]).
pub fn current_sender() -> Option<AgentId> {
    CURRENT_SENDER.try_with(|s| s.clone()).ok().flatten()
}

task_local! {
    /// The id that ORIGINATED the dispatch running right now — the CALLER, as
    /// the gate saw it, captured before [`CURRENT_SENDER`] is rewritten to the
    /// target's own id. `None` = an operator send.
    ///
    /// This exists because `CURRENT_SENDER` is the CALLEE mid-dispatch: inside
    /// a handler it reads as that handler's own agent, so a bundle could not
    /// tell who actually called it. That gap let an agent write ANOTHER agent's
    /// memory through a blob verb — the payload named the namespace and nothing
    /// could check it against the caller.
    pub static CURRENT_ORIGIN: Option<AgentId>;
}

/// The id that ORIGINATED this dispatch, or `None` for an operator send.
///
/// Read it when a verb's payload NAMES a resource the caller might not own (a
/// blob namespace, another agent's ledger rows). Do NOT use it for
/// `(sender, target, verb)` decisions — that is the gate's job, it runs first,
/// and duplicating it here would be a second truth.
pub fn current_origin() -> Option<AgentId> {
    CURRENT_ORIGIN.try_with(|s| s.clone()).ok().flatten()
}

/// Run `fut` with `sender` set as `CURRENT_SENDER` for its duration.
/// External transports (web_ws, web_rest) call this with the webapp's
/// own id so telemetry events attribute correctly AND so inbound calls are
/// GATED as that surface agent.
pub async fn with_sender<F, T>(sender: AgentId, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CURRENT_SENDER.scope(Some(sender), fut).await
}

/// Run `fut` with OPERATOR authority (`CURRENT_SENDER = None` → the call gate
/// skips it). NOT a privilege escalation for agent code: the substrate uses it
/// only to complete the side effects of an already-authorized verb — e.g.
/// `create_agent` booting the child it just made, or the persistence layer
/// writing a record to its own store. The kernel doing its bookkeeping acts as
/// the operator, not as the agent that asked.
pub async fn with_operator<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CURRENT_SENDER.scope(None, fut).await
}

/// One-line, bytes-stripped, max-160-char summary for telemetry.
/// Mirrors what Python emits as `summary` on state events.
fn summarize_payload(payload: &Value) -> String {
    let s = match serde_json::to_string(payload) {
        Ok(s) => s,
        Err(_) => format!("{payload:?}"),
    };
    // NOT TRUNCATED — mirrors python. A summary that silently drops the end
    // of a payload lies about what was sent, and the reader cannot tell a
    // short payload from a cut one. Elide visibly at render time instead.
    s
}

/// Extract `payload["type"]` as `&str`, or `""` if missing/wrong type.
fn verb_of(payload: &Value) -> &str {
    payload.get("type").and_then(Value::as_str).unwrap_or("")
}

/// A launched batch item: either it settled before ever reaching the task table
/// (a refusal), or it is a live dispatch to await.
enum Launched {
    /// Nothing entered the task table — `pid` stays `None`.
    Settled(Outcome),
    /// Boxed because a `Dispatch` is much larger than an `Outcome`, and a fat
    /// enum variant would be paid for on every refusal too.
    Dispatched(Box<Dispatch>),
    /// Already in flight under this id: wait for ITS outcome, run nothing.
    Joining(Box<Joiner>),
}

/// One item that passed the gate and is on its way to a handler.
struct Dispatch {
    target: Arc<Agent>,
    payload: Value,
    verb: String,
    sender: AgentId,
    timeout: Option<f64>,
    /// The ledger key: the caller's id, or the minted pid for an unnamed item.
    exec_id: String,
    /// The task-table handle. For a NAMED execution this IS `exec_id`, so `ps`
    /// and `cancel` speak the caller's own id; an unnamed one carries its minted
    /// pid. A retry re-registers under the same handle either way.
    task_id: TaskId,
    /// Whether the caller named this execution (its id is the handle).
    named: bool,
    /// A RAW request body: present ⇒ dispatch through the BINARY channel.
    binary: Option<Vec<u8>>,
}

/// An item that is JOINING an execution someone else is running, or that is
/// reading one already settled. Neither runs a handler.
struct Joiner {
    rx: tokio::sync::watch::Receiver<Option<Outcome>>,
    timeout: Option<f64>,
}

impl Kernel {
    /// ARRAY-ONLY batch send: `[Item] -> [Outcome]`.
    ///
    /// There is deliberately NO single-call form — one shape prevents
    /// single/batch drift. A single call is
    /// `kernel.send(vec![Item::new(t.clone(), p)]).await.remove(0).result`.
    ///
    /// Dispatch is CONCURRENT; the returned vec preserves input order. Every
    /// dispatched item is a REGISTERED task (one `ps` row, `cancel`-able by the
    /// pid the outcome carries). Pids are minted in a SYNCHRONOUS launch phase,
    /// so the whole batch is visible in `ps` before the first await.
    ///
    /// `timeout` is per-item seconds and the kernel NEVER defaults it:
    /// absent = wait forever. On expiry the item is killed through the FRONT
    /// DOOR (`cancel_task`), so every bundle's `on_cancel` hook runs before the
    /// abort — a bare task drop would orphan the OS resources those hooks free.
    ///
    /// Nothing propagates out of `send`: a handler panic settles as `failed`
    /// with `raised: true`, a cancel settles as `cancelled`.
    ///
    /// Why a batch at all: a model turn that fans out N calls used to get its
    /// answers spread over the next N turns and re-asked in between (measured
    /// 2026-08-25: 7 calls → replies `1 of 7`…`7 of 7`, `list_agents` re-asked
    /// five times, nothing built). The industry shape is the same — all results
    /// of one turn return together.
    pub async fn send(self: &Arc<Self>, items: Vec<Item>) -> Vec<Outcome> {
        // SYNCHRONOUS launch: resolve, gate and register EVERY item before the
        // first await, so `ps` shows the whole batch rather than it appearing
        // one row at a time.
        let mut launched: Vec<Launched> = Vec::with_capacity(items.len());
        for item in items {
            launched.push(self.launch(item));
        }

        // Concurrent settle. `join_all` over non-spawned futures keeps the
        // task-locals (`CURRENT_SENDER`/`CURRENT_TASK`/`CURRENT_ORIGIN`) — the
        // spawn happens INSIDE `run_dispatch`, which re-establishes them by
        // hand. A `tokio::spawn` here would silently drop all three.
        let mut out: Vec<Outcome> = Vec::with_capacity(launched.len());
        let settled =
            futures_util::future::join_all(launched.into_iter().map(|l| self.settle(l))).await;
        out.extend(settled);
        out
    }

    /// One item's SYNCHRONOUS half: resolve the target, run the gate, register
    /// the dispatch. Returns either an immediate outcome (nothing ever reached
    /// the task table) or a live dispatch to await.
    fn launch(self: &Arc<Self>, item: Item) -> Launched {
        if let Some(t) = item.timeout {
            if !(t.is_finite() && t > 0.0) {
                return Launched::Settled(Outcome::refused(crate::refusal::bad(
                    "send",
                    "timeout",
                    "a positive number of seconds",
                )));
            }
        }

        // The item's OWN fields are validated before anything is resolved, gated
        // or minted (python order). `type` is the verb; there is no send without
        // one — a typeless payload is a coded refusal for EVERY sender, never an
        // `unhandled` from the target (measured 2026-09-02).
        if !item.payload.is_object() {
            return Launched::Settled(Outcome::refused(crate::refusal::missing("send", "payload")));
        }
        let Some(type_v) = item.payload.get("type") else {
            return Launched::Settled(Outcome::refused(crate::refusal::missing(
                "send",
                "payload.type",
            )));
        };
        let verb = match type_v.as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return Launched::Settled(Outcome::refused(crate::refusal::bad(
                    "send",
                    "payload.type",
                    "a non-empty string",
                )))
            }
        };
        if let Some(id) = &item.id {
            // ONE ID SPACE: a caller id may not be empty and may not sit in the
            // kernel-minted `<agent>:<hex>` namespace unless it NAMES an existing
            // execution (re-fetch / join by pid) — minted ids are referenced,
            // never created.
            if id.is_empty() {
                return Launched::Settled(Outcome::refused(crate::refusal::bad(
                    "send",
                    "id",
                    "a non-empty string",
                )));
            }
            if id.contains(':') && !self.ledger.has(id) {
                let mut r = crate::refusal::refusal(
                    crate::refusal::BAD_ARG,
                    format!(
                        "send: id {id:?} is in the kernel-minted pid namespace (':') and \
                         names no known execution — minted ids can be referenced, never \
                         created"
                    ),
                );
                crate::refusal::with(&mut r, "arg", json!("id"));
                return Launched::Settled(Outcome::refused(r));
            }
        }

        // Resolve target. Special id `"kernel"` aliases to the root.
        let resolved: Option<Arc<Agent>> = if item.target.as_str() == "kernel" {
            self.root()
        } else {
            self.agents.get(&item.target).map(|e| Arc::clone(&e))
        };
        let Some(target) = resolved else {
            return Launched::Settled(Outcome::refused(crate::refusal::unknown_agent(
                item.target.as_str(),
                self.agents.iter().map(|e| e.key().0.clone()).collect(),
            )));
        };

        // ── CALL GATE — fully-gated, two-sided, from-above ──────────────
        // Trust boundary = who ORIGINATED the send. An operator send
        // (`current_sender()` is None: CLI/bootstrap/loader-at-boot/test, or a
        // verb completing its own side effect under `with_operator`) is the origin
        // root of trust → ungated. Every AGENT send — ANY verb, `reflect`
        // included, nothing exempt — needs BOTH the sender's `can_call` (egress)
        // AND the target's `callable_by` (ingress) to grant the edge.
        if let Some(orig) = current_sender() {
            if let Some(denial) = self.gate(&orig, &verb, &target, &item.payload) {
                return Launched::Settled(Outcome::refused(denial));
            }
        }

        let sender = current_sender().unwrap_or_else(|| target.id.clone());
        let origin = current_sender();

        // ONE ID SPACE, ONE HANDLE. A NAMED execution registers in the task
        // table under the caller's own id, so `ps` shows that id and `cancel
        // task_id=<id>` kills it — the id the caller already holds IS the
        // handle. Minting a second, private pid for a named item meant the
        // caller could not cancel or find its own execution (measured
        // 2026-09-02). An UNNAMED item still mints, and the mint stays
        // SYNCHRONOUS: the pid exists before the first await, which is what
        // makes a whole batch visible in `ps` at once.
        let named = item.id.is_some();
        let task_id = match &item.id {
            Some(id) => TaskId(id.clone()),
            None => self.tasks.mint(&target.id),
        };
        let exec_id = item.id.unwrap_or_else(|| task_id.0.clone());
        let retries = item.retries.unwrap_or(0);

        match self
            .ledger
            .open(&exec_id, &target.id, &verb, retries, named, origin.clone())
        {
            crate::ledger::Opened::Refused(r) => {
                self.tasks.forget(&task_id);
                return Launched::Settled(Outcome::refused(*r));
            }
            // ALREADY SETTLED — the recorded outcome IS the answer, and the
            // handler must not run a second time. This is idempotency and
            // once-only execution in one field.
            crate::ledger::Opened::Settled(o, orig_verb) => {
                self.tasks.forget(&task_id);
                // AUTHORIZED AGAINST THE EXECUTION IT JOINS. The recorded
                // outcome belongs to whoever ran it, so handing it back is
                // equivalent to having made THAT call — the gate must be asked
                // that question, not about the verb this caller happened to
                // send. Measured in python 2026-08-28: a caller granted only a
                // harmless verb reused another's id and got its secret verbatim.
                if let Some(d) = self.join_denial(&origin, &orig_verb, &target) {
                    return Launched::Settled(Outcome::refused(d));
                }
                return Launched::Settled(*o);
            }
            // IN FLIGHT — join it. A joiner's own `timeout` bounds only ITS
            // wait: it does not own the execution and must not kill it.
            crate::ledger::Opened::Join(rx, orig_verb) => {
                self.tasks.forget(&task_id);
                if let Some(d) = self.join_denial(&origin, &orig_verb, &target) {
                    return Launched::Settled(Outcome::refused(d));
                }
                return Launched::Joining(Box::new(Joiner {
                    rx,
                    timeout: item.timeout,
                }));
            }
            crate::ledger::Opened::Fresh => {}
        }

        Launched::Dispatched(Box::new(Dispatch {
            target,
            payload: item.payload,
            verb,
            sender,
            timeout: item.timeout,
            exec_id,
            task_id,
            named,
            binary: item.binary,
        }))
    }

    /// One item's ASYNC half.
    ///
    /// Runs the dispatch, enforces the item's own deadline, and — this is the
    /// RETRY ENGINE — re-dispatches a terminal `failed`/`timed_out` under the
    /// SAME id while the budget lasts. Only the FINAL outcome settles the
    /// ledger, so a joiner or a re-sender never sees an attempt that was later
    /// retried away.
    async fn settle(self: &Arc<Self>, launched: Launched) -> Outcome {
        let d = match launched {
            Launched::Settled(o) => return o,
            Launched::Joining(j) => return self.join_execution(*j).await,
            Launched::Dispatched(d) => *d,
        };
        let Dispatch {
            target,
            payload,
            verb,
            sender,
            timeout,
            exec_id,
            task_id,
            named,
            binary,
        } = d;

        let mut next_pid = task_id;
        // The pid of the attempt that actually SETTLED — a retry re-dispatches
        // under a fresh row, so the last one is what the outcome names.
        let (pid, reply, status) = loop {
            self.ledger.attempt(&exec_id);
            let mut this_pid: Option<TaskId> = None;
            let (reply, status) = self
                .run_dispatch(
                    &target,
                    &payload,
                    &verb,
                    Some(&mut this_pid),
                    timeout,
                    next_pid.clone(),
                    binary.clone(),
                )
                .await;
            if !self.ledger.should_retry(&exec_id, status) {
                break (this_pid, reply, status);
            }
            // Same exec id across attempts; a fresh task row per attempt, because
            // the previous one is already forgotten. Cancelling the execution
            // kills the current attempt AND this loop — a cancel is absorbing.
            // A retry is the SAME execution, so it re-registers under the SAME
            // id: cancelling that id kills the current attempt AND the loop.
            next_pid = if named {
                TaskId(exec_id.clone())
            } else {
                self.tasks.mint(&target.id)
            };
        };

        let mut reply = reply;
        // Universal post-process: compose every reflect reply with the
        // tree/bundles/readme flags (mirrors Python's
        // `Agent._apply_reflect_flags`). Bundle reflects and bare-agent
        // reflects alike get the tree (default all) + the optional
        // bundles catalog + the optional readme. Errors / Null replies
        // pass through unchanged.
        if verb == "reflect" && status == Status::Completed {
            crate::reflect::apply_reflect_flags(self, &target, &payload, &mut reply);
        }

        // State event + watcher fanout (synchronous; cheap subscribers).
        let event = json!({
            "type": "send",
            "sender": sender.0,
            "target": target.id.0,
            "verb": verb,
            "summary": summarize_payload(&payload),
        });
        self.publish_state(&event);
        // Watchers see the RAW request payload (Python parity) — they're
        // listening for verb dispatches, not event-metadata envelopes.
        // State subscribers above already got the metadata envelope.
        self.fanout_to_watchers(&target, &payload).await;

        let out = self
            .ledger
            .settle(&exec_id, Outcome::new(pid, status, reply));
        // The DURABLE half, if a loader installed one. The kernel does not know
        // what this writes or where.
        self.ledger_settled();
        out
    }

    /// Wait for an execution someone else owns.
    ///
    /// The joiner's `timeout` bounds only its OWN wait — on expiry it settles
    /// `timed_out` and the execution KEEPS RUNNING, because a joiner does not
    /// own it and killing another caller's work would be a surprise.
    async fn join_execution(self: &Arc<Self>, mut j: Joiner) -> Outcome {
        let wait = async {
            loop {
                if let Some(o) = j.rx.borrow_and_update().clone() {
                    return o;
                }
                if j.rx.changed().await.is_err() {
                    // The owner's row was evicted mid-flight — nothing to join.
                    return Outcome::refused(crate::refusal::refusal(
                        crate::refusal::TRANSPORT_ERROR,
                        "the execution being joined went away",
                    ));
                }
            }
        };
        match j.timeout {
            None => wait.await,
            Some(secs) => {
                let dur = std::time::Duration::from_secs_f64(secs);
                match tokio::time::timeout(dur, wait).await {
                    Ok(o) => o,
                    Err(_) => Outcome::new(
                        None,
                        Status::TimedOut,
                        crate::refusal::refusal(
                            crate::refusal::TIMEOUT,
                            format!("joined execution did not settle within {secs}s"),
                        ),
                    ),
                }
            }
        }
    }

    /// Run ONE dispatch as a registered, killable task and await it — the unit
    /// of the task table.
    ///
    /// **Parent propagation.** `tokio::spawn` MOVES the future onto a fresh
    /// task, and task-locals do NOT inherit across that move — so the scopes
    /// are re-established INSIDE the spawned task: `CURRENT_SENDER` (the target,
    /// as before), `CURRENT_TASK` (this dispatch's own id, so any `send` the
    /// handler issues registers as a CHILD of it — the table is a tree), and
    /// `CURRENT_KILLED` (its own dying flag). The PARENT link is read here, in
    /// the caller's task, before the move.
    ///
    /// **The cancellation contract, and why it is not ambiguous.** Both cases
    /// surface at this one await as `Ok(None)` (the dispatch's `select!` took
    /// the stop branch):
    ///   - THE DISPATCH was killed (someone sent `cancel`) → the awaiting caller
    ///     is NOT being killed; it gets the reply `{"cancelled": true}`.
    ///     Cancellation never raises out of `send`.
    ///   - WE were killed (our own task is inside the killed subtree) →
    ///     `current_killed()` is true, so we PARK and let our own stop signal
    ///     land instead of resuming. Getting this backwards would silently
    ///     resurrect a task the kernel just killed: the kill loop is
    ///     synchronous, but on a multi-thread runtime a parent woken by its
    ///     child's death can be polled on another worker before its own stop
    ///     lands — the flag (set for the WHOLE subtree before anything is
    ///     stopped) closes that window.
    // One attempt needs the target, the payload, the verb, the pid-out slot, the
    // deadline, the task id and (since the binary channel joined the batch) the
    // blob. Bundling them into a struct would only move the same seven fields.
    #[allow(clippy::too_many_arguments)]
    async fn run_dispatch(
        self: &Arc<Self>,
        target: &Arc<Agent>,
        payload: &Value,
        verb: &str,
        task_out: Option<&mut Option<TaskId>>,
        timeout: Option<f64>,
        task_id: TaskId,
        binary: Option<Vec<u8>>,
    ) -> (Value, Status) {
        // The pid was minted in the SYNCHRONOUS launch phase and is already the
        // ledger key for an unnamed execution — minting a second one here would
        // split the two apart, so the row and the reply would name different
        // things and a pid could not re-fetch its own outcome.
        //
        // Register BEFORE spawning: the row (and its stop handle) must exist
        // the instant the handler can run, or a nested `send` could claim a
        // parent that isn't in the table yet.
        if let Some(slot) = task_out {
            *slot = Some(task_id.clone());
        }
        let handle = TaskHandle::new();
        self.tasks.register(
            &task_id,
            &target.id,
            verb,
            current_task(), // the dispatch we are nested inside, if any
            handle.clone(),
        );

        let join = {
            let kernel = Arc::clone(self);
            let tgt = Arc::clone(target);
            let payload = payload.clone();
            let sender = target.id.clone();
            // The CALLER, read here in the caller's own context — inside the
            // spawn `CURRENT_SENDER` becomes the target, so this is the last
            // place the origin is still visible.
            let origin = current_sender();
            let tid = task_id.clone();
            let cancel = handle.cancel_signal();
            let killed = handle.killed_flag();
            tokio::spawn(async move {
                CURRENT_ORIGIN
                    .scope(
                        origin,
                        CURRENT_SENDER.scope(
                            Some(sender),
                            CURRENT_TASK.scope(
                                tid,
                                CURRENT_KILLED.scope(killed, async move {
                                    tokio::select! {
                                        biased;
                                        // Stopped: DROP the dispatch future — the
                                        // abort half of `cancel`.
                                        _ = cancel.notified() => None,
                                        // A blob rides the BINARY channel under
                                        // this same row/ledger/timeout. The
                                        // reply body is dropped here exactly as
                                        // python does: `write_stream` answers
                                        // JSON, and a `read_stream` through the
                                        // batch is not the carrier for bytes.
                                        v = async {
                                            match binary {
                                                Some(b) => {
                                                    dispatch_binary(&kernel, tgt, payload, b)
                                                        .await
                                                        .0
                                                }
                                                None => dispatch(kernel, tgt, payload).await,
                                            }
                                        } => Some(v),
                                    }
                                }),
                            ),
                        ),
                    )
                    .await
            })
        };

        // Structured concurrency: if the CALLER goes away (its own future is
        // dropped — a `select!` loser, a timeout, its own cancellation), the
        // dispatch it spawned goes with it, exactly as when dispatch ran inline.
        // Also the row's self-cleaner: dropped on every exit path.
        let _guard = DispatchGuard {
            kernel: Arc::clone(self),
            id: task_id.clone(),
            handle,
        };

        // THE ITEM'S OWN DEADLINE. Expiry kills through the FRONT DOOR
        // (`cancel_task`) so every bundle's `on_cancel` hook runs BEFORE the
        // abort — dropping the future would strand the OS resources those hooks
        // release (a subprocess, a PTY, a socket). The hook's own sends run
        // under OPERATOR authority: it is the kernel closing out work it was
        // told to kill, not the batch caller reaching out, so a gated caller
        // never makes an `on_cancel` fail.
        //
        // The race is reported honestly: a handler that finishes during the
        // grace window COMPLETES — `cancel_task` no-ops on a finished id and
        // the join below yields its real reply.
        let outcome = match timeout {
            None => join.await,
            Some(secs) => {
                let dur = std::time::Duration::from_secs_f64(secs);
                match tokio::time::timeout(dur, &mut Box::pin(&mut { join })).await {
                    Ok(j) => j,
                    Err(_) => {
                        let k = Arc::clone(self);
                        let tid = task_id.clone();
                        with_operator(async move { k.cancel_task(&tid).await }).await;
                        return (
                            crate::refusal::refusal(
                                crate::refusal::TIMEOUT,
                                format!("{verb} on {} timed out after {secs}s", target.id),
                            ),
                            Status::TimedOut,
                        );
                    }
                }
            }
        };

        match outcome {
            // `raised` marks a terminal handler failure (a bundle `Err`, or a
            // panic below) — retryable, and a dead-letter row when the budget
            // runs out. A plain error-shaped reply is a completed dispatch.
            Ok(Some(v)) if v.get("raised").and_then(Value::as_bool) == Some(true) => {
                (v, Status::Failed)
            }
            Ok(Some(v)) => (v, Status::Completed),
            Ok(None) => {
                if current_killed() {
                    // We are in the killed subtree — park; our own stop signal
                    // lands at this yield and drops us. Never resume.
                    std::future::pending::<()>().await;
                    unreachable!("a killed task is dropped at its next yield");
                }
                (json!({ "cancelled": true }), Status::Cancelled)
            }
            // A handler PANIC is a result, not a crash of the caller: it settles
            // as `failed` with `raised: true`. `send` propagates nothing — every
            // net a caller wrapped around it is now dead code.
            Err(e) if e.is_panic() => {
                let why = panic_message(&e.into_panic());
                (
                    json!({ "error": format!("panic: {why}"), "raised": true }),
                    Status::Failed,
                )
            }
            Err(_) => (json!({ "cancelled": true }), Status::Cancelled),
        }
    }

    /// Send a binary-framed verb. Mirrors [`Self::send`] except the
    /// payload travels as `(header, blob)` instead of a single JSON
    /// `Value`. Dispatches through the target bundle's
    /// [`crate::bundle::Bundle::handle_binary`] (whose default impl
    /// dispatches the JSON header through `handle` and drops the raw
    /// blob — never base64; a bundle that wants the bytes overrides it).
    ///
    /// State events: emits `{"type":"send_binary", sender, target,
    /// verb, summary}` for telemetry parity with text dispatch. Watcher
    /// fanout uses the same event payload.
    pub async fn send_with_binary(
        self: &Arc<Self>,
        target_id: &AgentId,
        header: Value,
        blob: Vec<u8>,
    ) -> (Value, Vec<u8>) {
        // Resolve target. Special id `"kernel"` aliases to the root.
        let resolved: Option<Arc<Agent>> = if target_id.as_str() == "kernel" {
            self.root()
        } else {
            self.agents.get(target_id).map(|e| Arc::clone(&e))
        };
        let Some(target) = resolved else {
            // The SAME refusal the text channel gives — coded, and carrying the
            // live id index so the caller can see what it could have meant.
            return (
                crate::refusal::unknown_agent(
                    target_id.as_str(),
                    self.agents.iter().map(|e| e.key().0.clone()).collect(),
                ),
                Vec::new(),
            );
        };

        let sender = current_sender().unwrap_or_else(|| target.id.clone());
        let verb = verb_of(&header).to_string();
        let blob_len = blob.len();
        let summary = summarize_payload(&header);

        // Call gate (see `Kernel::send`) — the binary path carries the same
        // agent-originated write_stream/read_stream sends, gated two-sided.
        // Only an operator (`current_sender()` is None) skips it.
        if let Some(orig) = current_sender() {
            if let Some(denial) = self.gate(&orig, &verb, &target, &header) {
                return (denial, Vec::new());
            }
        }

        // Run dispatch under a fresh CURRENT_SENDER scope so nested
        // sends attribute to this target (matches Python contextvars).
        let kernel = Arc::clone(self);
        let target_for_dispatch = Arc::clone(&target);
        let header_clone = header.clone();
        let (reply, body) = with_sender(target.id.clone(), async move {
            dispatch_binary(&kernel, target_for_dispatch, header_clone, blob).await
        })
        .await;

        // State event + watcher fanout. `bytes` counts the larger of the
        // request/reply body so telemetry reflects either direction.
        let event = json!({
            "type": "send_binary",
            "sender": sender.0,
            "target": target.id.0,
            "verb": verb,
            "summary": summary,
            "bytes": blob_len.max(body.len()),
        });
        self.publish_state(&event);
        // Watchers get the raw header (request envelope without the
        // blob). Mirrors Python's payload-to-watchers semantics.
        self.fanout_to_watchers(&target, &header).await;

        (reply, body)
    }

    /// Emit a payload to a target's inbox without dispatching. Returns
    /// immediately. Fans the event out to watchers; publishes a state
    /// event of type `"emit"`.
    ///
    /// The target's inbox is auto-vivified if missing (synthetic
    /// browser-client ids land here).
    pub async fn emit(self: &Arc<Self>, target_id: &AgentId, payload: Value) {
        // Auto-vivify the inbox for synthetic ids (any id not in
        // `agents` is treated as a watch-only listener).
        if !self.inboxes.contains_key(target_id) {
            let (tx, _rx) = tokio::sync::mpsc::channel(self.inbox_bound);
            self.inboxes.insert(target_id.clone(), tx);
        }
        if let Some(tx) = self.inboxes.get(target_id) {
            // try_send so a slow consumer doesn't deadlock the emitter.
            // Drops on full match Python's bounded-deque behaviour.
            let _ = tx.try_send(payload.clone());
        }

        let sender = current_sender().unwrap_or_else(|| target_id.clone());
        let verb = verb_of(&payload).to_string();
        let event = json!({
            "type": "emit",
            "sender": sender.0,
            "target": target_id.0,
            "verb": verb,
            "summary": summarize_payload(&payload),
        });
        self.publish_state(&event);
        // Best-effort watcher fanout — emits without a registered
        // Agent (synthetic targets) have no watcher set; skip.
        // Watchers see the RAW payload (Python parity) — they
        // subscribed to learn what's flowing through the target's
        // inbox, not metadata about it.
        if let Some(target) = self.agents.get(target_id).map(|e| Arc::clone(&e)) {
            self.fanout_to_watchers(&target, &payload).await;
        }
    }

    /// Register `watcher_id` as an observer of `src_id`'s inbox.
    /// Every subsequent `send`/`emit` targeting `src_id` mirrors the
    /// payload into `watcher_id`'s inbox via [`Self::fanout_to_watchers`].
    /// Mirror `src_id`'s inbound traffic into `watcher_id`'s inbox. Returns a
    /// refusal when the gate denies it, else `None`.
    ///
    /// GATED LIKE A SEND, because it IS a read of the target. A watcher receives
    /// src's raw inbound payloads VERBATIM — the model prompt, the document
    /// body, whatever was sent — so an ungated `watch` is a read side-channel
    /// around `callable_by`: an agent that may not CALL an agent could still
    /// read everything sent TO it.
    ///
    /// Found 2026-08-27 in a dataroom audit: a browser leg granted exactly
    /// `assistant:[send]` could mirror ANY agent's inbox in its session and read
    /// the packed confidential context out of the model's prompt. The `call`
    /// path went through the two-sided gate; this one never did.
    ///
    /// An OPERATOR watch (`current_sender()` is `None`) is the origin root of
    /// trust and stays ungated, as with send.
    pub async fn watch(self: &Arc<Self>, src_id: &AgentId, watcher_id: AgentId) -> Option<Value> {
        let Some(src) = self.agents.get(src_id).map(|e| Arc::clone(&e)) else {
            return None; // nothing to mirror; not a refusal
        };
        if let Some(orig) = current_sender() {
            if let Some(denial) = self.gate(&orig, "watch", &src, &Value::Null) {
                return Some(denial);
            }
        }
        {
            src.watcher_ids.write().await.insert(watcher_id.clone());
        }
        // Auto-vivify the watcher's inbox so fanout has a destination
        // even if `watcher_id` is a synthetic browser-client id.
        if !self.inboxes.contains_key(&watcher_id) {
            let (tx, _rx) = tokio::sync::mpsc::channel(self.inbox_bound);
            self.inboxes.insert(watcher_id, tx);
        }
        None
    }

    /// May `watcher_id` tap the kernel-wide STATE STREAM? Refusal, or `None`.
    ///
    /// The stream carries every send's sender, target, verb and payload summary
    /// for every agent in the tree, so subscribing is the broadest read the
    /// kernel offers. It is gated on the ROOT, and `state_subscribe` is in
    /// [`crate::authz::PRIVILEGED_VERBS`] so a wildcard never covers it: the
    /// edge must be named.
    ///
    /// An ingress rule answers "may this client reach this leg"; it says nothing
    /// about what the client may then READ.
    pub fn authorize_state_subscribe(self: &Arc<Self>, watcher_id: &AgentId) -> Option<Value> {
        // An OPERATOR tap is the origin root of trust, as with send and watch.
        current_sender()?;
        let root = self.root()?;
        self.gate(watcher_id, "state_subscribe", &root, &Value::Null)
    }

    /// Detach a previously-registered watcher.
    pub async fn unwatch(&self, src_id: &AgentId, watcher_id: &AgentId) {
        if let Some(src) = self.agents.get(src_id) {
            src.watcher_ids.write().await.remove(watcher_id);
        }
    }

    /// Push `payload` to every watcher of `target`'s inbox.
    pub(crate) async fn fanout_to_watchers(&self, target: &Agent, payload: &Value) {
        let watchers: Vec<AgentId> = target.watcher_ids.read().await.iter().cloned().collect();
        for w in watchers {
            if let Some(tx) = self.inboxes.get(&w) {
                let _ = tx.try_send(payload.clone());
            }
        }
    }
}

/// Caller-side guard for one dispatch: on ANY exit path (reply, kill, or the
/// caller's own future being dropped) it stops the dispatch task and drops its
/// row. Stopping a task that already finished is a no-op; forgetting a row that
/// `cancel` already removed is a no-op — both are idempotent by design.
/// Best-effort human text out of a panic payload — the two shapes
/// `panic!` produces plus a fallback, so a handler that panicked with a
/// non-string never reads as "panic: <nothing>".
fn panic_message(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "handler panicked".to_string()
    }
}

struct DispatchGuard {
    kernel: Arc<Kernel>,
    id: TaskId,
    handle: TaskHandle,
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        self.handle.stop();
        self.kernel.tasks.forget(&self.id);
    }
}

/// Resolve verb → system handler or bundle handler.
///
/// BOXED, not `async fn`, and deliberately so: the dispatch path recurses
/// (`send` → dispatch → `create_agent` → `send` → …), and since `send` now
/// SPAWNS its dispatch, that cycle would force the compiler to prove
/// `Send`-ness of a future in terms of itself — auto-trait inference gives up
/// and reports "not Send". Declaring one concrete `dyn Future + Send` at the
/// recursion point cuts the cycle.
fn dispatch(
    kernel: Arc<Kernel>,
    target: Arc<Agent>,
    payload: Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>> {
    Box::pin(async move {
        let verb = verb_of(&payload);
        if is_kernel_verb(verb) {
            // ROOT ONLY. These verbs act on the TREE and the kernel-wide task
            // table, so the agent named is the OPERAND, not the actor. Answered
            // everywhere, "send create_agent to X" could mean X performs it, X
            // is created under something, or X is the parent — only the last was
            // ever true, and a model asked "which target?" for 16 turns and
            // finished with nothing created.
            //
            // Refused HERE, in the handler — not at the call gate — so probes
            // asserting a denial `side` for a non-root privileged send stay valid.
            if target.parent_id.is_some() {
                return crate::refusal::not_here(verb, target.id.as_str());
            }
            return handle_kernel_verb(&kernel, &target, verb, &payload).await;
        }
        // No handler_module → only system verbs answerable.
        let Some(hm) = target.handler_module.as_deref() else {
            // Universal answers for bare agents.
            return match verb {
                "boot" | "shutdown" => Value::Null,
                "reflect" => crate::reflect::reflect_identity(&target),
                // CODED, not a bare error. A reason-less `{error}` is decorated
                // by `apply_reflect_flags` into something that reads like a
                // healthy reflect (kernel_verbs, tree, more) with a stray error
                // field — the caller cannot tell a refusal from an answer
                // (measured 2026-09-02).
                _ => {
                    let mut r = crate::refusal::refusal(
                        crate::refusal::UNHANDLED,
                        format!(
                            "agent {:?} has no handler_module; cannot answer verb {:?}",
                            target.id.0, verb
                        ),
                    );
                    crate::refusal::with(&mut r, "verb", json!(verb));
                    r
                }
            };
        };
        let Some(bundle) = kernel.bundles.get(hm) else {
            // NOT a create-time refusal: this kernel legitimately holds records
            // whose bundle belongs to ANOTHER runtime (the browser's `*.ts`
            // agents persist here), or is installed later. The call simply
            // cannot be answered HERE, and says so with a code.
            let mut r = crate::refusal::refusal(
                crate::refusal::UNHANDLED,
                format!(
                    "agent {:?}: handler_module {hm:?} is not installed in this runtime",
                    target.id.0
                ),
            );
            crate::refusal::with(&mut r, "verb", json!(verb));
            return r;
        };
        match bundle.handle(&target.id, &payload, &kernel).await {
            Ok(Some(v)) => v,
            Ok(None) => Value::Null,
            // A BUNDLE ERROR IS A TERMINAL FAILURE, marked `raised` exactly as
            // a panic is — python settles a handler raise as `failed`
            // (`_agent.py`: "a handler raise settles as `failed`"). Without the
            // marker this returned a perfectly successful reply that merely
            // CONTAINED an error, so `status` was `Completed`,
            // `Status::is_retryable` was false, and the retry engine exited
            // after one attempt: `retries: N` was silently inert for every
            // bundle failure in rust, and no execution could ever dead-letter.
            // An `Ok(Some({"error": ...}))` — an unknown verb, a bad argument —
            // is NOT this: it stays `Completed`, because the handler chose to
            // answer rather than fail.
            Err(e) => json!({ "error": e.to_string(), "raised": true }),
        }
    })
}

/// Binary-frame dispatch: route to the target bundle's `handle_binary`.
/// Bare agents (no handler_module) have no binary verb surface — return
/// an error matching the text-dispatch shape so callers get a uniform
/// error envelope across both channels.
async fn dispatch_binary(
    kernel: &Arc<Kernel>,
    target: Arc<Agent>,
    header: Value,
    blob: Vec<u8>,
) -> (Value, Vec<u8>) {
    let Some(hm) = target.handler_module.as_deref() else {
        return (
            json!({
                "error": format!(
                    "agent {:?} has no handler_module; cannot answer binary frame",
                    target.id.0
                ),
            }),
            Vec::new(),
        );
    };
    let Some(bundle) = kernel.bundles.get(hm) else {
        return (
            json!({ "error": format!("no bundle for handler_module {hm:?}") }),
            Vec::new(),
        );
    };
    match bundle.handle_binary(&target.id, header, blob, kernel).await {
        Ok((Some(v), body)) => (v, body),
        Ok((None, body)) => (Value::Null, body),
        Err(e) => (json!({ "error": e.to_string() }), Vec::new()),
    }
}

/// Substrate verbs answered natively on every agent — explicitly
/// `list_agents`' pointer back to the briefing — py-canonical `_LIST_HINT`.
const LIST_HINT: &str = "Raw records — how agents are WIRED, not what they are FOR, \
and no manual. {type:'reflect'} on the root is the briefing: each agent's purpose, \
plus `self_extension` (how to author your own durable tools in THIS kernel — which \
executor, and the fields without which the agent is created and never runs) and \
`readmes` (the manual index). Reflect the root before building anything.";

mod gate;
mod kernel_verbs;
use kernel_verbs::{handle_kernel_verb, is_kernel_verb};
pub use kernel_verbs::{kernel_verb_docs, kernel_verbs_view, KERNEL_VERBS_PREAMBLE};

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "send/batch_tests.rs"]
mod batch_tests;

#[cfg(test)]
#[path = "send/ledger_tests.rs"]
mod ledger_tests;
