//! Verb routing — `Kernel::send`, `Kernel::emit`, watch fanout, the
//! `_current_sender` task-local that tags state events with the
//! originator.
//!
//! Routing model: the kernel's `agents` map is the only routing table.
//! `send(target_id, payload)` resolves `target_id` directly there;
//! parent-child structure doesn't enter routing. An inbox exists only where a
//! READER registered one (`Kernel::ensure_inbox`) — agents and synthetic browser
//! client ids alike.
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
//! Every dispatch — text or binary — runs [`Kernel::fanout`] BEFORE its handler:
//! the raw payload onto the target's OWN inbox and each watcher's, plus the state
//! event `{agent_id, kind:"send", backlog, sender, summary, payload}` for the
//! target and for every watcher that is a real agent. `emit` does the same with
//! `kind:"emit"`; once the handler returns, `{agent_id, kind:"drain", backlog}`.

use crate::agent::{Agent, AgentId, RESERVED_RECORD_KEYS};
use crate::kernel::Kernel;
use crate::outcome::{Item, Outcome, Status};
use crate::tasks::{
    current_killed, current_task, TaskHandle, TaskId, CURRENT_KILLED, CURRENT_TASK,
};
use fantastic_bundle::{py_str, py_truthy};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::task_local;

/// Whether a handler's reply is a REFUSAL — a map carrying a truthy `error`.
/// The same test every consumer used to make for itself off the result body.
fn error_shaped(v: &Value) -> bool {
    match v.get("error") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

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

tokio::task_local! {
    /// THE KERNEL'S OWN SHUTDOWN TRAFFIC. `shutdown_agents` sets the drain flag
    /// FIRST (so nothing new enters while it works) and only then sends every
    /// agent its `shutdown` verb — which the drain gate would otherwise refuse
    /// as new work. Python `_in_shutdown` / `shutdown_context()`.
    pub static IN_SHUTDOWN: bool;
}

/// Is this send part of the kernel's own shutdown sequence?
#[must_use]
pub fn in_shutdown() -> bool {
    IN_SHUTDOWN.try_with(|v| *v).unwrap_or(false)
}

/// Run `fut` as the kernel's own shutdown traffic — exempt from the drain gate.
pub async fn with_shutdown<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    IN_SHUTDOWN.scope(true, fut).await
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

/// One-line, bytes-shrunk summary for telemetry — python's `_summarize_payload`.
///
/// `json.dumps(shrunk, default=str, ensure_ascii=False)`, so `, ` / `: `
/// separators and raw non-ASCII, not rust's compact `serde_json::to_string`. A
/// bytes value becomes `<bytes:N>` — the rust carrier keeps the bytes beside the
/// payload (see [`crate::inbox::InboxItem`]), so the marker is written back at
/// the body's own path.
///
/// NOT TRUNCATED — mirrors python. A summary that silently drops the end of a
/// payload lies about what was sent, and the reader cannot tell a short payload
/// from a cut one. Elide visibly at render time instead.
fn summarize_payload(payload: &Value, body: Option<&(String, Vec<u8>)>) -> String {
    match body {
        None => fantastic_bundle::py_json_dumps(payload, false, None),
        Some((path, bytes)) => {
            let mut shrunk = payload.clone();
            set_at_path(&mut shrunk, path, json!(format!("<bytes:{}>", bytes.len())));
            fantastic_bundle::py_json_dumps(&shrunk, false, None)
        }
    }
}

/// Write `value` at a dotted `path` (python's `set_path`: object keys, list
/// indices). Silently no-ops on a path that does not resolve — the only caller
/// is the summary above, and a telemetry line is not worth a panic.
fn set_at_path(obj: &mut Value, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut cur = obj;
    for p in &parts[..parts.len() - 1] {
        cur = match cur {
            Value::Array(a) => match p.parse::<usize>() {
                Ok(i) if i < a.len() => &mut a[i],
                _ => return,
            },
            Value::Object(m) => match m.get_mut(*p) {
                Some(v) => v,
                None => return,
            },
            _ => return,
        };
    }
    let last = parts[parts.len() - 1];
    match cur {
        Value::Array(a) => {
            if let Ok(i) = last.parse::<usize>() {
                if i < a.len() {
                    a[i] = value;
                }
            }
        }
        Value::Object(m) => {
            m.insert(last.to_string(), value);
        }
        _ => {}
    }
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
    timeout: Option<f64>,
    /// The execution's id — the kernel counter's number, and the ledger key.
    exec_id: i64,
    /// The task-table handle. It IS `exec_id`: one number names the execution in
    /// `ps`, in `cancel`, in the ledger and in the reply. A retry re-registers
    /// under the same handle.
    task_id: TaskId,
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
        // A NAME MAY NOT BE EMPTY. Everything else about the caller's id is the
        // ledger's business: a name it has seen JOINS that execution, a name it
        // has not opens a new one, and there is no namespace to trespass in
        // because the caller never supplies an id at all — only a name.
        if item.id.as_deref() == Some("") {
            return Launched::Settled(Outcome::refused(crate::refusal::bad(
                "send",
                "id",
                "a non-empty string",
            )));
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

        // WHILE DRAINING, NO NEW ROOTS. A send from inside live work (a parent
        // task in the table — including an agent's own `shutdown` turn using its
        // tools) passes; everything else is a new root and is refused BEFORE it
        // costs an id. The root's own control surface stays open: kernel verbs
        // (`shutdown_agents` again, `shutdown_kernel`, `ps`, `messages`,
        // `ledger`) and `reflect` are how an operator watches and finishes the
        // stop. Python `Agent._send` (`kernel/_agent.py`).
        if self.draining() && !in_shutdown() {
            let to_root = item.target.as_str() == "kernel"
                || self.root().is_some_and(|r| r.id == item.target);
            let control = to_root && (is_kernel_verb(&verb) || verb == "reflect");
            let inside_live_work = current_task().is_some_and(|t| self.tasks.contains(&t));
            if !control && !inside_live_work {
                return Launched::Settled(Outcome::refused(crate::refusal::refusal(
                    crate::refusal::DRAINING,
                    "send: the kernel is draining for shutdown — no new work is \
                     accepted; in-flight work may finish",
                )));
            }
        }

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

        let origin = current_sender();

        // ONE ID SPACE, AND THE KERNEL OWNS IT. A caller supplies a NAME — a
        // handle it chose, "ensure this ran" — and the kernel supplies the id
        // from its single counter. Re-sending the same name JOINS the execution
        // that name already opened; it never starts a second one and never mints
        // into the kernel's space. The mint stays SYNCHRONOUS: the pid exists
        // before the first await, which is what makes a whole batch visible in
        // `ps` at once.
        let name = item.id.clone();
        let joined = name.as_deref().and_then(|n| self.ledger.resolve_name(n));
        let exec_id = joined.unwrap_or_else(|| self.rows.mint_id());
        let task_id = TaskId(exec_id);
        let retries = item.retries.unwrap_or(0);

        match self.ledger.open(
            exec_id,
            &target.id,
            &verb,
            retries,
            name.as_deref(),
            origin.clone(),
        ) {
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
            crate::ledger::Opened::Fresh => {
                // EVERY SEND IS A MESSAGE. The row names the call — who was
                // asked what, by which message — and takes the outcome's status
                // at settle; the reply body stays with the reply. `parent` is
                // the message that made the call. Written under the id the
                // ledger just opened, so the call, the task and the outcome are
                // all one number. Python `Agent._ledger_open`.
                let mut row = Map::new();
                row.insert("id".to_string(), Value::from(exec_id));
                row.insert("kind".to_string(), Value::from("call"));
                row.insert(
                    "verb".to_string(),
                    item.payload.get("type").cloned().unwrap_or(Value::Null),
                );
                row.insert(
                    "parent".to_string(),
                    Value::from(current_task().map(TaskId::get)),
                );
                row.insert(
                    "origin".to_string(),
                    Value::from(origin.as_ref().map(|o| o.0.clone())),
                );
                if let Some(n) = name.as_deref() {
                    row.insert("name".to_string(), Value::from(n));
                }
                let _ = self.rows.put(&target.id, row);
            }
        }

        Launched::Dispatched(Box::new(Dispatch {
            target,
            payload: item.payload,
            verb,
            timeout: item.timeout,
            exec_id,
            task_id,
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
            timeout,
            exec_id,
            task_id,
            binary,
        } = d;

        let mut next_pid = task_id;
        // The pid of the attempt that actually SETTLED — a retry re-dispatches
        // under a fresh row, so the last one is what the outcome names.
        let (pid, reply, status) = loop {
            self.ledger.attempt(exec_id);
            let mut this_pid: Option<TaskId> = None;
            let (reply, status) = self
                .run_dispatch(
                    &target,
                    &payload,
                    &verb,
                    Some(&mut this_pid),
                    timeout,
                    next_pid,
                    binary.clone(),
                )
                .await;
            if !self.ledger.should_retry(exec_id, status) {
                break (this_pid, reply, status);
            }
            // A RETRY IS THE SAME EXECUTION, so it re-registers under the SAME
            // id — named or not. It used to mint a fresh pid for an unnamed
            // retry, so `cancel <pid>` on attempt two missed the work it was
            // aimed at. Cancelling the id kills the current attempt AND this
            // loop: a cancel is absorbing.
            next_pid = TaskId(exec_id);
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

        // NO FANOUT HERE. It ran in `run_dispatch`, BEFORE the handler — which is
        // where python puts it and what a watcher needs: mirroring after the
        // reply meant a watcher saw the request only once the work was already
        // done, so its echo landed after the handler's own progress events and
        // the measured order inverted (`>start job_started start`).
        let out = self
            .ledger
            .settle(exec_id, Outcome::new(pid, status, reply));
        // A SETTLE IS A ROW. Without one the message log records only
        // BEGINNINGS: a `call` row is appended at launch and nothing is ever
        // appended for its ending, so a cold read cannot tell "never settled"
        // from "settled". (Python had the same hole from the other side — it
        // mutated the launch row in place and the pump's debounce usually missed
        // the stamp.) A row is append-only by contract, so the ending has to be
        // an append.
        //
        // ON THE ROOT, not on the settled call's agent: an agent's row list is
        // read as its CONVERSATION — by the model's own history, by a fork's
        // row copy — and each of those readers filters bookkeeping by an
        // enumerated set of kinds it was told about. A new kind on an agent's
        // list is conversation to whichever reader was not. `settled` names the
        // agent as a FIELD, so the ending still says what it ended.
        self.stamp_call(pid, &out);
        // The DURABLE half, if a loader installed one. The kernel does not know
        // what this writes or where.
        self.ledger_settled();
        out
    }

    /// Settle the call: stamp the live row AND append a `settle` row.
    ///
    /// The stamp is python's, not the ledger's `Outcome`: `{status, error?,
    /// reason?}` — the error is `str(result.error)` cut to 200 CODE POINTS, and
    /// both optional keys follow python truthiness. The full outcome (pid,
    /// result, attempts) belongs to the ledger, not to the message log: writing
    /// it here put every reply body — binary ones included — into
    /// `messages.xml`.
    ///
    /// Nothing is written when the call row is absent (a refusal that never
    /// opened one, a hydrated id): python returns early on the same test, and
    /// the settle row exists to END a call that was written down.
    fn stamp_call(self: &Arc<Self>, pid: Option<TaskId>, out: &Outcome) {
        let Some(pid) = pid else { return };
        let Some(row) = self.rows.get_one(pid.get()) else {
            return;
        };
        let mut stamp = Map::new();
        stamp.insert("status".to_string(), Value::from(out.status.as_str()));
        if let Some(res) = out.result.as_object() {
            if let Some(err) = res.get("error").filter(|v| py_truthy(v)) {
                stamp.insert(
                    "error".to_string(),
                    Value::from(py_str(err).chars().take(200).collect::<String>()),
                );
                if let Some(reason) = res.get("reason").filter(|v| py_truthy(v)) {
                    stamp.insert("reason".to_string(), reason.clone());
                }
            }
        }
        let mut onto = Map::new();
        onto.insert("outcome".to_string(), Value::Object(stamp.clone()));
        self.rows.stamp(pid.get(), &onto);
        if let Some(root) = self.root() {
            let mut settle = Map::new();
            settle.insert("kind".to_string(), Value::from("settle"));
            settle.insert("parent".to_string(), Value::from(pid.get()));
            settle.insert(
                "settled".to_string(),
                row.get("agent").cloned().unwrap_or(Value::Null),
            );
            settle.insert("outcome".to_string(), Value::Object(stamp));
            let _ = self.rows.put(&root.id, settle);
        }
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
        // The pid was minted in the SYNCHRONOUS launch phase and IS the ledger
        // key — minting a second one here would split the two apart, so the row
        // and the reply would name different things and a pid could not
        // re-fetch its own outcome.
        //
        // Register BEFORE spawning: the row (and its stop handle) must exist
        // the instant the handler can run, or a nested `send` could claim a
        // parent that isn't in the table yet.
        if let Some(slot) = task_out {
            *slot = Some(task_id);
        }
        let handle = TaskHandle::new();
        self.tasks.register(
            &task_id,
            &target.id,
            verb,
            current_task(), // the dispatch we are nested inside, if any
            handle.clone(),
        );

        // BEFORE THE HANDLER, once per attempt — python's `_dispatch` bumps
        // `_in_flight`, fans out, and only then resolves the verb. Here, too, is
        // the last point at which `CURRENT_SENDER` is still the CALLER: the spawn
        // below re-points it at the target, so a fanout inside it would attribute
        // every event to its own recipient. The guard publishes the matching
        // `drain` on every exit path, timeout included.
        let _in_flight = InFlightGuard::enter(self, target);
        let item = match binary.clone() {
            // The BINARY channel carries the payload's bytes at `bytes` — the key
            // python's own payload dict holds them under — so a watcher receives
            // the body, not a header with a hole where it was.
            Some(blob) => {
                let mut head = payload.clone();
                if let Some(m) = head.as_object_mut() {
                    m.insert("bytes".to_string(), Value::Null);
                }
                crate::inbox::InboxItem::binary(head, "bytes", blob)
            }
            None => crate::inbox::InboxItem::text(payload.clone()),
        };
        self.fanout(target, "send", &item).await;

        let join = {
            let kernel = Arc::clone(self);
            let tgt = Arc::clone(target);
            let payload = payload.clone();
            let sender = target.id.clone();
            // The CALLER, read here in the caller's own context — inside the
            // spawn `CURRENT_SENDER` becomes the target, so this is the last
            // place the origin is still visible.
            let origin = current_sender();
            let tid = task_id;
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
            id: task_id,
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
                        let tid = task_id;
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
            // runs out.
            Ok(Some(v)) if v.get("raised").and_then(Value::as_bool) == Some(true) => {
                (v, Status::Failed)
            }
            // FAILURE IS A STATUS. A handler that RAN and said no — a missing
            // path, a patch that did not match, a refusal it composed itself —
            // settles `errored`, not `completed`. The reply is kept verbatim
            // either way; what changes is that a reader learns the outcome from
            // `status` instead of parsing `result`. Retries are unchanged:
            // `is_retryable` covers the transport classes only, so a semantic
            // no is never re-sent.
            Ok(Some(v)) if error_shaped(&v) => (v, Status::Errored),
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
    /// ONE DISPATCH PATH, text or binary: the fanout below is the same
    /// `kind:"send"` event and the same inbox put the text channel makes, run
    /// BEFORE the handler and carrying the raw body — there is no `send_binary`
    /// event kind, and a watcher is not told which channel a payload rode.
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

        let verb = verb_of(&header).to_string();

        // Call gate (see `Kernel::send`) — the binary path carries the same
        // agent-originated write_stream/read_stream sends, gated two-sided.
        // Only an operator (`current_sender()` is None) skips it.
        if let Some(orig) = current_sender() {
            if let Some(denial) = self.gate(&orig, &verb, &target, &header) {
                return (denial, Vec::new());
            }
        }

        // BEFORE the handler, with the body: this is the frame python puts on a
        // watcher's inbox for a `write_stream`, and the only reason a watcher can
        // ever see the bytes at all.
        let _in_flight = InFlightGuard::enter(self, &target);
        let mut head = header.clone();
        if let Some(m) = head.as_object_mut() {
            m.insert("bytes".to_string(), Value::Null);
        }
        self.fanout(
            &target,
            "send",
            &crate::inbox::InboxItem::binary(head, "bytes", blob.clone()),
        )
        .await;

        // Run dispatch under a fresh CURRENT_SENDER scope so nested
        // sends attribute to this target (matches Python contextvars).
        let kernel = Arc::clone(self);
        let target_for_dispatch = Arc::clone(&target);
        let header_clone = header.clone();
        let (reply, body) = with_sender(target.id.clone(), async move {
            dispatch_binary(&kernel, target_for_dispatch, header_clone, blob).await
        })
        .await;

        (reply, body)
    }

    /// Emit a payload to a target's inbox without dispatching. Returns
    /// immediately. Publishes a `kind:"emit"` state event and mirrors to
    /// watchers — the same fanout a dispatch runs, minus the in-flight bump.
    ///
    /// AN EMIT TO A NON-AGENT ID IS A NO-OP. The target agent is resolved FIRST:
    /// no agent, no inbox put, no state event. Rust used to put into any
    /// registered inbox and publish regardless, so a synthetic WS client id
    /// (`_ws_<hex>`) produced telemetry for a sprite that does not exist — python
    /// drops it silently (`_proxy.py:196-200`).
    pub async fn emit(self: &Arc<Self>, target_id: &AgentId, payload: Value) {
        self.emit_item(target_id, crate::inbox::InboxItem::text(payload))
            .await;
    }

    /// [`Self::emit`] for a payload carrying raw bytes — a `read_stream` chunk a
    /// bridge re-emits from a remote watch, which must reach a local watcher
    /// WHOLE. Same no-op rule for a non-agent id.
    pub async fn emit_item(self: &Arc<Self>, target_id: &AgentId, item: crate::inbox::InboxItem) {
        let Some(target) = self.agents.get(target_id).map(|e| Arc::clone(&e)) else {
            return;
        };
        self.fanout(&target, "emit", &item).await;
    }

    /// Register `watcher_id` as an observer of `src_id`'s inbox.
    /// Every subsequent `send`/`emit` targeting `src_id` mirrors the
    /// payload into `watcher_id`'s inbox via [`Self::fanout`].
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
        // A WATCHER MUST ALREADY BE A READER. This used to lazy-create the
        // watcher's inbox — a sender whose receiver was dropped on the same
        // line, so the mirror went nowhere and the caller had no way to know.
        // Registering a consumer is what `watch` requires, not what it does.
        if !self.inboxes.contains_key(&watcher_id) {
            let who = fantastic_bundle::py_repr(&watcher_id.0);
            let mut r = crate::refusal::refusal_hint(
                crate::refusal::NO_CONSUMER,
                format!(
                    "watch: {who} has no registered inbox, so nothing would read \
                     what is mirrored there"
                ),
                format!("register the reader first — ctx.ensure_inbox({who}) — then watch"),
            );
            crate::refusal::with(&mut r, "watcher", json!(watcher_id.0));
            crate::refusal::with(&mut r, "src", json!(src_id.0));
            return Some(r);
        }
        {
            src.watcher_ids.write().await.insert(watcher_id.clone());
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

    /// Python's `Agent._fanout` — the ONE place a payload reaches an inbox.
    ///
    /// The target's OWN inbox first (a reader that registered one sees what was
    /// sent TO it, not only what its handler replied), then the state event for
    /// the target, then each watcher's inbox plus a state event per watcher that
    /// is a REAL agent. A synthetic watcher (a browser client id) gets the mail
    /// and no telemetry ray — publishing one would mint a phantom sprite.
    ///
    /// The whole `item` travels, bytes included: a watcher on a `write_stream`
    /// receives the body, which is the binary frame python sends it.
    pub(crate) async fn fanout(
        &self,
        target: &Arc<Agent>,
        kind: &str,
        item: &crate::inbox::InboxItem,
    ) {
        // THE CALLER, not the target. Read before any dispatch scope re-points
        // `CURRENT_SENDER` at the target; an operator send carries JSON null.
        let sender = current_sender();
        let summary = summarize_payload(&item.payload, item.body.as_ref());
        if let Some(tx) = self.inboxes.get(&target.id) {
            // Never blocks the emitter, and at the bound the OLDEST payload is
            // evicted — python's `_put_drop_oldest` and swift's
            // `.bufferingNewest` rule, which rust used to invert.
            tx.send(item.clone());
        }
        self.publish_state_with_body(
            &traffic_event(
                &target.id,
                kind,
                target.in_flight(),
                sender.as_ref(),
                &summary,
                item,
            ),
            item.body.as_ref(),
        );
        let watchers: Vec<AgentId> = target.watcher_ids.read().await.iter().cloned().collect();
        for w in watchers {
            let Some(tx) = self.inboxes.get(&w).map(|e| e.clone()) else {
                continue;
            };
            tx.send(item.clone());
            if let Some(wa) = self.agents.get(&w).map(|e| Arc::clone(&e)) {
                self.publish_state_with_body(
                    &traffic_event(&w, kind, wa.in_flight(), sender.as_ref(), &summary, item),
                    item.body.as_ref(),
                );
            }
        }
    }
}

/// One traffic state event, in python's key order:
/// `{agent_id, kind, backlog, sender, summary, payload}` (`ts` is stamped by
/// `publish_state`). `sender` is the originator or JSON null — never the target's
/// own id, which is what rust used to substitute for an operator send.
fn traffic_event(
    agent_id: &AgentId,
    kind: &str,
    backlog: i64,
    sender: Option<&AgentId>,
    summary: &str,
    item: &crate::inbox::InboxItem,
) -> Value {
    json!({
        "agent_id": agent_id.0,
        "kind": kind,
        "backlog": backlog,
        "sender": sender.map(|s| s.0.clone()),
        "summary": summary,
        "payload": item.payload,
    })
}

/// The `{agent_id, kind:"drain", backlog}` event every dispatch publishes once
/// its handler has returned — python's `_dispatch` `finally`.
fn drain_event(agent_id: &AgentId, backlog: i64) -> Value {
    json!({ "agent_id": agent_id.0, "kind": "drain", "backlog": backlog })
}

/// Bump `target`'s in-flight count for the life of one dispatch, then drop it and
/// publish the `drain` event — python's `try/finally` around the handler, which a
/// rust early return (a timeout) must honour just the same.
pub(crate) struct InFlightGuard {
    kernel: Arc<Kernel>,
    target: Arc<Agent>,
}

impl InFlightGuard {
    pub(crate) fn enter(kernel: &Arc<Kernel>, target: &Arc<Agent>) -> Self {
        target.in_flight_enter();
        Self {
            kernel: Arc::clone(kernel),
            target: Arc::clone(target),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.target.in_flight_leave();
        self.kernel
            .publish_state(&drain_event(&self.target.id, self.target.in_flight()));
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
                            "agent {} has no handler_module; cannot answer verb {}",
                            fantastic_bundle::py_repr(&target.id.0),
                            fantastic_bundle::py_repr(verb)
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
