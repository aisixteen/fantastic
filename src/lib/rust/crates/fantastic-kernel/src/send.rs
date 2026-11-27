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
//! `update_agent`, `reflect`) are
//! resolved on the target agent itself without consulting its bundle.
//! Everything else routes to the bundle whose `handler_module` matches
//! the agent's record. Unknown bundle → `{"error": "no bundle for
//! handler_module"}`. Missing target → `{"error": "no agent <id>"}`.
//!
//! ### Watcher fanout
//!
//! Every dispatch — with a request body or without — runs [`Kernel::fanout`]
//! BEFORE its handler:
//! the raw payload onto the target's OWN inbox and each watcher's, plus the state
//! event `{agent_id, kind:"send", backlog, sender, summary, payload}` for the
//! target and for every watcher that is a real agent. `emit` does the same with
//! `kind:"emit"`; once the handler returns, `{agent_id, kind:"drain", backlog}`.

use crate::agent::{Agent, AgentId};
use crate::kernel::Kernel;
use crate::outcome::{Item, Outcome, Status};
use crate::tasks::{
    current_killed, current_task, Inherited, TaskHandle, TaskId, CURRENT_KILLED, CURRENT_TASK,
};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::task_local;

/// Whether a handler's reply is a REFUSAL — a map carrying an `error` (not null,
/// not empty text). The one test, so no consumer makes its own off the result body.
fn error_shaped(v: &Value) -> bool {
    crate::refusal::is_refused(v)
}

task_local! {
    /// The agent whose handler is running right now. `Some(id)` inside that
    /// agent's dispatch and every task it spawns: a send made there is THAT
    /// agent's, and the CALL GATE judges it as that agent. `None` = OPERATOR —
    /// outside any dispatch (the CLI, the cold bootstrap, a test driver). Only
    /// the kernel sets it, when it runs a handler: identity comes from dispatch,
    /// and nothing switches it. Also tags telemetry state events with the
    /// originator.
    pub(crate) static CURRENT_SENDER: Option<AgentId>;
}

/// Get the current sender — `Some(id)` inside an agent's dispatch (or a task it
/// spawned), `None` for an operator send (outside any dispatch).
pub fn current_sender() -> Option<AgentId> {
    CURRENT_SENDER.try_with(|s| s.clone()).ok().flatten()
}

/// THE DISPATCH IDENTITY for one of an agent's own HOOKS (`on_cancel`,
/// `on_delete`): the kernel running that agent's code, so every send made inside
/// is the agent's — python `Agent._as_self`. The kernel sets an identity only when
/// it runs an agent's own code; a bundle that creates an agent as bookkeeping
/// (`fork`) and must write that agent's first rows under ITS key uses it for that
/// one step.
pub(crate) async fn as_agent<F, T>(agent: &AgentId, hook: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CURRENT_SENDER.scope(Some(agent.clone()), hook).await
}

task_local! {
    /// The id that ORIGINATED the dispatch running right now — the CALLER, as
    /// the gate saw it, captured before [`CURRENT_SENDER`] is set to the
    /// target's own id. `None` = an operator send, or the kernel's own call.
    ///
    /// This exists because `CURRENT_SENDER` is the CALLEE mid-dispatch: inside
    /// a handler it reads as that handler's own agent, so a bundle could not
    /// tell who actually called it — an inquiry's asker, `stop`'s stopper, an
    /// operator send told from an agent's.
    pub(crate) static CURRENT_ORIGIN: Option<AgentId>;
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

/// The origin scope as it stands: `Some(origin)` inside a dispatch, an operator
/// call ([`Kernel::lifecycle_call`]), the shell's [`as_operator`] or kernel
/// bookkeeping (with `Some(None)` for the operator), and `None` when no scope is
/// set at all (a bare `tokio::spawn`). The operator is origin `None` set
/// explicitly, never the absence of an origin: code that acts for "whoever asked"
/// reads this, and refuses on `None`.
pub fn origin_scope() -> Option<Option<AgentId>> {
    CURRENT_ORIGIN.try_with(Clone::clone).ok()
}

/// THE IN-PROCESS SHELL'S SCOPE: `work` runs with origin none set explicitly, so
/// what it does outside any dispatch (a send, an emit, a watch) is the
/// operator's. Only the in-process shell, and a test acting as the operator, call
/// it; [`Kernel::lifecycle_call`] is this around one call. The sender is still the
/// running dispatch's when there is one, so code running inside an agent stays
/// that agent's.
pub async fn as_operator<F: std::future::Future>(work: F) -> F::Output {
    CURRENT_ORIGIN.scope(None, work).await
}

tokio::task_local! {
    /// A HANDLER RUN AS KERNEL BOOKKEEPING ([`Kernel::bookkeeping`]): it answers no
    /// call of its own, so it has no execution to [`Kernel::defer`] — the running task
    /// is its caller's, whose execution is not its to settle.
    pub(crate) static IN_BOOKKEEPING: bool;
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

tokio::task_local! {
    /// THE LEDGER FLUSH IS SILENT on the state stream. The kernel's own flush
    /// `send` — and every send, emit and state event inside its execution (the
    /// store's dispatch, its drain) — publishes nothing: a subscriber woken by
    /// the flush (a long-poll `events` waiter) would make a call, the call rows,
    /// the rows the next flush, forever, filling the store. The rows stay
    /// recorded by the flush row. Carried into the dispatch and the forks the
    /// flush starts. Python `_state_quiet`.
    pub(crate) static STATE_QUIET: bool;
}

/// Is this inside the kernel's own ledger flush?
pub(crate) fn state_quiet() -> bool {
    STATE_QUIET.try_with(|v| *v).unwrap_or(false)
}

/// One-line, bytes-shrunk summary for telemetry — python's `_summarize_payload`.
///
/// `json.dumps(shrunk, default=str, ensure_ascii=False)`, so `, ` / `: `
/// separators and raw non-ASCII, not rust's compact `serde_json::to_string`. A
/// bytes value becomes `<bytes:N>` — the rust carrier keeps the bytes beside the
/// payload (see [`crate::inbox::InboxItem`]), so the marker is written back at
/// the body's own slot: a literal key `a.b` is the key `"a.b"`, as in python's
/// dict.
///
/// NOT TRUNCATED — mirrors python. A summary that silently drops the end of a
/// payload lies about what was sent, and the reader cannot tell a short payload
/// from a cut one. Elide visibly at render time instead.
fn summarize_payload(payload: &Value, body: Option<&crate::outcome::Blob>) -> String {
    match body {
        None => fantastic_bundle::py_json_dumps(payload, false, None),
        Some(b) => {
            let mut shrunk = payload.clone();
            // A telemetry line is not worth a refusal: an unplaceable path
            // (never one the kernel admitted) leaves the payload as it is.
            let _ = b.place(&mut shrunk, json!(format!("<bytes:{}>", b.bytes.len())));
            fantastic_bundle::py_json_dumps(&shrunk, false, None)
        }
    }
}

/// The typed arguments of a `reflect` the KERNEL answers itself (the `kernel` address
/// fast path): the kernel is the agent of its own verbs and parses them through the
/// shared entry of the common verbs; `Err` is its refusal.
fn kernel_reflect_args(
    target: &Agent,
    payload: &Value,
) -> Result<crate::declared::common::Reflect, Value> {
    use crate::declared::common::Call;
    let Call::Reflect(args) = Call::parse(&target.id.0, payload)?;
    Ok(args)
}

/// Extract `payload["type"]` as `&str`, or `""` if missing/wrong type.
fn verb_of(payload: &Value) -> &str {
    payload.get("type").and_then(Value::as_str).unwrap_or("")
}

/// `agent <id!r> has no handler_module; cannot answer verb <verb!r>` — the
/// one dispatch's refusal, with or without a request body.
fn no_handler_module_refusal(id: &str, verb: &str) -> Value {
    let mut r = crate::refusal::refusal(
        crate::refusal::UNHANDLED,
        format!(
            "agent {} has no handler_module; cannot answer verb {}",
            fantastic_bundle::py_repr(id),
            fantastic_bundle::py_repr(verb)
        ),
    );
    crate::refusal::with(&mut r, "verb", json!(verb));
    r
}

/// python's `ModuleNotFoundError` text for a handler module no bundle here
/// answers: `No module named '<package>'`. Importing `a.tools` when no `a`
/// package exists fails at `a` — the TOP-LEVEL name, which is what an absent
/// bundle is (`No module named 'absent_bundle'` for `absent_bundle.tools`).
pub(crate) fn no_module_named(hm: &str) -> String {
    let top = hm.split('.').next().unwrap_or(hm);
    format!("No module named {}", fantastic_bundle::py_repr(top))
}

/// `agent <id!r>: handler_module <hm!r> cannot be imported in this runtime:
/// ModuleNotFoundError: <message>` — the one dispatch's refusal (C34/C24),
/// python's text verbatim.
fn not_installed_refusal(id: &str, hm: &str, verb: &str) -> Value {
    let mut r = crate::refusal::refusal(
        crate::refusal::UNHANDLED,
        format!(
            "agent {}: handler_module {} cannot be imported in this runtime: \
             ModuleNotFoundError: {}",
            fantastic_bundle::py_repr(id),
            fantastic_bundle::py_repr(hm),
            no_module_named(hm),
        ),
    );
    crate::refusal::with(&mut r, "verb", json!(verb));
    r
}

/// An item's validated deadline: the seconds the kernel waits, and the number
/// the caller GAVE. Python keeps `timeout` as whatever arrived — the call row
/// stores it and the timeout prose prints it (`f"{d.timeout}s"`: `5` → `5s`,
/// `5.0` → `5.0s`, `1e-07` → `1e-07s`). A typed rust `f64` is a python float.
#[derive(Clone)]
struct Deadline {
    secs: f64,
    given: Value,
}

impl Deadline {
    /// `{d.timeout}` — python's `str()` of the number the caller gave.
    fn shown(&self) -> String {
        fantastic_bundle::py_float_repr(self.secs)
    }
}

/// `timeout` validation: the typed `item.timeout`, a positive finite number of seconds.
fn validate_timeout(item: &Item) -> Result<Option<Deadline>, Value> {
    let Some(t) = item.timeout else {
        return Ok(None);
    };
    if !(t.is_finite() && t > 0.0) {
        return Err(crate::refusal::bad(
            "send",
            "timeout",
            &format!(
                "a positive number of seconds, got {}",
                fantastic_bundle::py_repr_value(&json!(t))
            ),
        ));
    }
    Ok(Some(Deadline {
        secs: t,
        given: Value::from(t),
    }))
}

/// python's `refusal(TIMEOUT, f"send: {d.target.id} {d.payload.get('type')!r}
/// exceeded its {d.timeout}s timeout; task cancelled", task_id=d.pid or "-")`.
fn own_deadline_refusal(target: &AgentId, verb: &str, d: &Deadline, task_id: Value) -> Value {
    let mut r = crate::refusal::refusal(
        crate::refusal::TIMEOUT,
        format!(
            "send: {} {} exceeded its {}s timeout; task cancelled",
            target,
            fantastic_bundle::py_repr(verb),
            d.shown(),
        ),
    );
    crate::refusal::with(&mut r, "task_id", task_id);
    r
}

/// A launched batch item: either it settled before ever reaching the task table
/// (a refusal), or it is accepted and its settle comes later.
enum Launched {
    /// Nothing entered the task table — `pid` stays `None`.
    Settled(Outcome),
    /// A named execution that ALREADY SETTLED: its recorded outcome is the answer
    /// and nothing runs (the dedup contract). It is delivered like any settle.
    Recorded(Outcome, Box<Joiner>),
    /// Boxed because a `Dispatch` is much larger than an `Outcome`, and a fat
    /// enum variant would be paid for on every refusal too.
    Dispatched(Box<Dispatch>),
    /// Already in flight under this id: the item is a JOINER of ITS settle, and
    /// runs nothing.
    Joining(Box<Joiner>),
    /// The UNNAMED `kernel` reflect fast path — python `_agent.py:846`.
    /// A pure identity read: no ledger entry, no call row, no task-table
    /// row, no watcher fanout. Still async (reflect flags may nest
    /// dispatches), so it rides the settle phase like a real dispatch — just
    /// with none of that bookkeeping, and its outcome's `pid` is `None`.
    KernelReflectFast(Box<FastReflect>),
}

/// The unnamed `kernel` reflect fast path's payload — see [`Launched::
/// KernelReflectFast`]. It keeps the item's deadline: python awaits even this
/// pid-less read under `asyncio.wait(timeout)`.
struct FastReflect {
    target: Arc<Agent>,
    payload: Value,
    timeout: Option<Deadline>,
    /// The caller, as the gate judged it: the reflect is cut to what it holds
    /// (`None`: the operator, who sees everything).
    caller: Option<AgentId>,
    /// Who receives the read's settle (the item's `reply_to`).
    recipient: Option<AgentId>,
}

/// One item that passed the gate and is on its way to a handler.
struct Dispatch {
    target: Arc<Agent>,
    payload: Value,
    verb: String,
    timeout: Option<Deadline>,
    /// The execution's id — the kernel counter's number, and the ledger key.
    execution_id: i64,
    /// The task-table handle. It IS `execution_id`: one number names the execution in
    /// `ps`, in `cancel`, in the ledger and in the reply. A retry re-registers
    /// under the same handle.
    task_id: TaskId,
    /// A RAW request body and the payload path it sits at (python's payload
    /// dict holding bytes there).
    binary: Option<crate::outcome::Blob>,
    /// The CALLER, as the gate judged it: the sending agent, or `None` for an
    /// operator send. Every attempt's mirrors and its handler's `current_origin`
    /// read it.
    caller: Option<AgentId>,
    /// The item's `secret` marks (`crate::secret`), carried to every attempt.
    marks: Arc<Vec<String>>,
    /// WHO RECEIVES THE SETTLE, resolved from the item's `reply_to` at launch:
    /// the caller (`None` for the operator), another agent, or `None` for the
    /// row only. Recorded on the call row.
    recipient: Option<AgentId>,
    /// A caller-NAMED `kernel` reflect — python's flags-done fast path: the
    /// FIRST attempt is a bare identity read with the flags applied (no task
    /// row, no fanout, no `_in_flight`, and no enforced deadline: `cancel_task`
    /// finds no task under the pid, so the read runs to completion). The ledger
    /// still applies. A retry is a real dispatch whose reply is not decorated
    /// again (`flags_done` stays set).
    named_fast_reflect: bool,
}

/// An item that is JOINING an execution someone else is running, or that is
/// reading one already settled. Neither runs a handler: the execution's settle is
/// delivered to it ([`Kernel::join`]).
struct Joiner {
    /// The joiner's own deadline: it bounds only the joiner's delivery, never the
    /// execution it joins.
    timeout: Option<Deadline>,
    /// The id of the execution being JOINED (python `d.pid`): the ack's pid.
    execution_id: i64,
    /// The joined call's target and verb, as its settle names them.
    target: AgentId,
    verb: String,
    /// Who receives the settle (the joiner's own `reply_to`).
    recipient: Option<AgentId>,
}

impl Kernel {
    /// Is this send the root's CONTROL SURFACE — a kernel verb or `reflect`
    /// on the root — directly, or carried by a wire leg's `inbound_call`? The
    /// drain gate keeps it open while the kernel stops. Python
    /// `Agent._is_control`.
    fn is_control(&self, target: &str, payload: &Value) -> bool {
        let carried;
        let (target, payload) = match serde_json::from_value::<crate::declared::HandIn>(payload.clone()) {
            Ok(crate::declared::HandIn::InboundCall { item }) => {
                carried = item;
                (carried.target.as_str(), &carried.payload)
            }
            Ok(crate::declared::HandIn::Other) | Err(_) => (target, payload),
        };
        let verb = verb_of(payload);
        let to_root = target == "kernel" || self.root().is_some_and(|r| r.id.as_str() == target);
        to_root && (is_kernel_verb(verb) || verb == "reflect")
    }

    /// ARRAY-ONLY batch send: `[Item] -> [Outcome]` — every outcome an ACK.
    ///
    /// EVERYTHING ASYNC (#795): `send` returns once every item is LAUNCHED. Each
    /// outcome is the item's ack — `{pid, status: accepted}` once its pid is minted
    /// and its call row written — or the launch's refusal (a gate denial, a bad
    /// item, an unknown target, a draining kernel). The handlers run as their own
    /// tasks; each settle is a row, then a MESSAGE to the item's `reply_to` (the
    /// caller by default): its `reply` verb, when its bundle declares one, else the
    /// row alone. A caller that needs the result as a step of its own uses
    /// [`Self::call`] (a continuation) instead. Nothing returns a settled result.
    ///
    /// There is deliberately NO single-call form — one shape prevents
    /// single/batch drift. Pids are minted and call rows written in a SYNCHRONOUS
    /// launch phase, in item order, so the whole batch is visible in `ps` before
    /// the first handler runs. Every dispatched item is a REGISTERED task (one `ps`
    /// row, `cancel`-able by the pid its ack carries).
    ///
    /// `timeout` is per-item seconds and the kernel NEVER defaults it: absent = no
    /// deadline. On expiry the item is killed through the FRONT DOOR
    /// (`cancel_task`), so every bundle's `on_cancel` hook runs before the abort,
    /// and its `timed_out` settle (the dead letter) is delivered like any.
    ///
    /// Nothing propagates out of `send`: a handler panic settles as `failed` with
    /// `raised: true`, a cancel settles as `cancelled`.
    pub async fn send(self: &Arc<Self>, items: Vec<Item>) -> Vec<Outcome> {
        self.launch_all(items.into_iter().map(|i| (i, None)).collect())
            .await
    }

    /// THE LAUNCH PHASE every call takes ([`Self::send`], [`Self::call`]): resolve,
    /// gate and register EVERY item before any handler runs, then start each one —
    /// its settle goes to its continuation (`Some`), else to its `reply_to`. The acks
    /// come back in item order.
    pub(crate) async fn launch_all(
        self: &Arc<Self>,
        items: Vec<(Item, Option<deliver::Then>)>,
    ) -> Vec<Outcome> {
        // NO ORIGIN SCOPE, NO SEND. Work spawned with a bare `tokio::spawn` carries
        // no identity at all, and is never taken for the operator: every item gets
        // the unknown answer for its target. The operator sends through
        // [`as_operator`]; spawned work keeps its identity through
        // `spawn_inherit`.
        if origin_scope().is_none() {
            return items
                .iter()
                .map(|(it, _)| Outcome::refused(crate::refusal::unknown_agent(it.target.as_str())))
                .collect();
        }
        // THE CALLER: the agent whose dispatch makes this send, or `None` — the
        // operator. Read once; every item is judged as it.
        let caller = current_sender();
        // NAMED EXECUTIONS OUTLIVE RAM. Once flushed, a settled execution is only in
        // the kernel's store; an item naming one is looked up there FIRST, so the
        // synchronous launch below finds it exactly as if it had never left RAM.
        //
        // A store read that fails RAISES in python (out of `send`, into the
        // caller's handler). `send` never raises here, so every item of the batch
        // is refused with that raise.
        //
        // TODO(parity, rare failure): python's raise
        // also reaches the WIRE: `web_rest` answers HTTP 500 for the whole request,
        // where rust's legs answer 200 with every item failed. Mirroring it needs
        // `send` to be fallible — either `send` returns
        // `Result<Vec<Outcome>, SendError>` (changes every caller, the CLI's
        // in-process `Arc<Kernel>` use included) or a separate `try_send` used only
        // by the wire legs (two entry points). Both outcomes are loud and carry the
        // store's error text, so only the response SHAPE differs.
        if self.store_id().is_some() && items.iter().any(|(it, _)| it.id.is_some()) {
            let named: Vec<Item> = items.iter().map(|(it, _)| it.clone()).collect();
            if let Err(e) = self.recall_executions(&named).await {
                let r = crate::executions::raised("RuntimeError", &e);
                return items.iter().map(|_| Outcome::refused(r.clone())).collect();
            }
        }
        // SYNCHRONOUS launch: resolve, gate and register EVERY item before the
        // first handler runs, so `ps` shows the whole batch rather than it
        // appearing one row at a time.
        let mut launched: Vec<(Launched, Option<deliver::Then>)> = Vec::with_capacity(items.len());
        for (item, then) in items {
            launched.push((self.launch(item, caller.clone()), then));
        }
        launched
            .into_iter()
            .map(|(l, then)| self.start_item(l, then, caller.clone()))
            .collect()
    }

    /// `id` resolution: a NAME opens-or-joins by name; a number JOINS an existing
    /// execution or refuses.
    ///
    /// Returns `(join_id, name, caller_id)`: `join_id` is `Some` when an
    /// existing execution must be joined (a number naming one, or a name the
    /// ledger already resolves); `name` is `Some` only for the name case
    /// (what a FRESH open binds); `caller_id` is the id as JSON
    /// (`Value::Null` when the item carried none), kept for the ledger's
    /// rebind text.
    fn resolve_execution_id(&self, item: &Item) -> Result<(Option<i64>, Option<String>, Value), Value> {
        use crate::declared::IdOrName;
        let Some(id) = &item.id else {
            return Ok((None, None, Value::Null));
        };
        let raw = id.to_value();
        match id {
            IdOrName::Id(n) => {
                if !self.ledger.has(*n) {
                    let mut r = crate::refusal::refusal(
                        crate::refusal::BAD_ARG,
                        format!(
                            "send: id {n} names no known execution — ids are \
                             minted by the kernel and can be joined, never \
                             created",
                        ),
                    );
                    crate::refusal::with(&mut r, "arg", json!("id"));
                    return Err(r);
                }
                Ok((Some(*n), None, raw))
            }
            IdOrName::Name(s) if !s.is_empty() => {
                let joined = self.ledger.resolve_name(s);
                Ok((joined, Some(s.clone()), raw))
            }
            IdOrName::Name(_) => Err(crate::refusal::bad("send", "id", "a non-empty name")),
        }
    }

    /// The bundle that answers `target` here — `None` for a remote agent, a bare
    /// agent, or a handler_module no registry holds.
    fn local_bundle(&self, target: &Agent) -> Option<Arc<dyn crate::bundle::Bundle>> {
        if target.is_remote() {
            return None;
        }
        target
            .handler_module
            .as_deref()
            .and_then(|hm| self.bundles.get(hm))
    }

    /// The item's marks plus the ones `target`'s bundle names for this payload
    /// (`Bundle::payload_secret`) — the well-formed ones (`crate::secret::check`'s
    /// rules: non-empty steps, never `type`), each once.
    fn with_bundle_marks(
        &self,
        target: &Agent,
        payload: &Value,
        marks: Arc<Vec<String>>,
    ) -> Arc<Vec<String>> {
        let Some(bundle) = self.local_bundle(target) else {
            return marks;
        };
        let mut all = marks.as_ref().clone();
        for m in bundle.payload_secret(payload) {
            if m != "type" && !m.split('.').any(str::is_empty) && !all.contains(&m) {
                all.push(m);
            }
        }
        if all.len() == marks.len() {
            marks
        } else {
            Arc::new(all)
        }
    }

    /// One item's SYNCHRONOUS half: validate, resolve, gate. Python
    /// `Agent._launch` (`kernel/_agent.py:601-732`) — the check order:
    /// target → payload dict → timeout → payload.type → id → retries → killed
    /// → draining → resolve → gate. `caller` is who sends: the dispatching
    /// agent, or `None` (the operator — ungated).
    fn launch(self: &Arc<Self>, item: Item, caller: Option<AgentId>) -> Launched {
        // 1. target — must be a non-empty string.
        if item.target.as_str().is_empty() {
            return Launched::Settled(Outcome::refused(crate::refusal::missing("send", "target")));
        }
        // 2. payload dict.
        if !item.payload.is_object() {
            return Launched::Settled(Outcome::refused(crate::refusal::missing("send", "payload")));
        }
        // 2b. A RAW BODY sits at a slot of its payload — where python's dict holds
        // the bytes. A path that names none could never be mirrored, recorded or
        // read by the handler; every wire leg places its body before it gets here,
        // so this only refuses an in-process caller's typo.
        if let Some(blob) = &item.binary {
            let mut probe = item.payload.clone();
            if blob.place(&mut probe, Value::Null).is_err() {
                return Launched::Settled(Outcome::refused(crate::refusal::bad(
                    "send",
                    "binary",
                    &format!(
                        "a body path naming a slot of the payload, got {}",
                        fantastic_bundle::py_repr_value(&fantastic_bundle::path::steps_value(
                            &blob.path
                        ))
                    ),
                )));
            }
        }
        // 3. timeout.
        let timeout = match validate_timeout(&item) {
            Ok(t) => t,
            Err(r) => return Launched::Settled(Outcome::refused(r)),
        };
        // 4. payload.type — `type` is the verb; there is no send without one.
        // Validated for EVERY send (operator included) BEFORE the gate, so a
        // typeless payload is a coded refusal, never an `unhandled` from the
        // target.
        // A RAW BODY at `type` is python's `bytes` value there.
        let body_at_type = item.binary.as_ref().is_some_and(|b| b.at("type").is_some());
        let type_v = match item.payload.get("type") {
            Some(v) => v,
            None if body_at_type => &Value::Null,
            None => {
                return Launched::Settled(Outcome::refused(crate::refusal::missing(
                    "send",
                    "payload.type",
                )));
            }
        };
        let verb = match type_v.as_str() {
            Some(s) if !s.is_empty() && !body_at_type => s.to_string(),
            _ => {
                // A non-string is named by its TYPE, never echoed: the octet REST
                // leg and a binary WS frame can put a whole body here — python's
                // `type(verb).__name__`.
                let got = if body_at_type {
                    "bytes".to_string()
                } else if type_v.is_string() {
                    fantastic_bundle::py_repr_value(type_v)
                } else {
                    fantastic_bundle::py_type_name(type_v).to_string()
                };
                let mut r = crate::refusal::refusal(
                    crate::refusal::BAD_ARG,
                    format!("send: payload.type must be a non-empty string, got {got}"),
                );
                crate::refusal::with(&mut r, "arg", json!("payload.type"));
                return Launched::Settled(Outcome::refused(r));
            }
        };
        // 5. id — a NAME (non-empty string) opens-or-joins by name; an int
        // JOINS an existing execution or refuses; anything else is `bad_arg`.
        let (join_id, name, caller_id) = match self.resolve_execution_id(&item) {
            Ok(v) => v,
            Err(r) => return Launched::Settled(Outcome::refused(r)),
        };
        // 6. retries — a non-negative int, never defaulted otherwise.
        let retries = item.retries.unwrap_or(0);
        // 6a. reply_to — who the settle is delivered to.
        let recipient = match &item.reply_to {
            crate::outcome::ReplyTo::Caller => caller.clone(),
            crate::outcome::ReplyTo::Agent(id) => Some(id.clone()),
            crate::outcome::ReplyTo::Row => None,
        };
        // 6b. secret marks — paths into the payload, shown as `[secret]`.
        let marks = match crate::secret::check(&item.secret) {
            Ok(m) => Arc::new(m),
            Err(r) => return Launched::Settled(Outcome::refused(r)),
        };
        // 7. killed — A DYING TASK REGISTERS NO NEW WORK. `cancel_task` marks
        // the whole subtree before it aborts anything, because `on_cancel`
        // hooks await — so a doomed handler gets scheduling passes on its way
        // out, and what it sends there must not start fresh dispatches that
        // outlive the cancel.
        if current_killed() {
            return Launched::Settled(Outcome::new(
                None,
                Status::Cancelled,
                json!({"cancelled": true}),
            ));
        }
        // 8. draining — WHILE DRAINING, NO NEW ROOTS, checked BEFORE the
        // target is even resolved. A send from
        // inside live work (a parent task in the table — including an
        // agent's own `shutdown` dispatch using its tools) passes; everything
        // else is a new root and is refused BEFORE it costs an id. The
        // root's own control surface stays open: kernel verbs
        // (`shutdown_agents` again, `kernel_stop`, `ps`, `ledger`) and
        // `reflect` are how an operator watches and finishes
        // the stop. Over a wire the operator's call arrives as a leg's
        // `inbound_call` carrying it — judged by the call it carries, or a
        // remote operator could never finish the stop it started. Python
        // `Agent._send` (`kernel/_agent.py`).
        if self.draining() && !in_shutdown() {
            let control = self.is_control(item.target.as_str(), &item.payload);
            // a live DISPATCH, or a keepalive fork; any other fork starts new
            // work — a ws call is refused exactly where the same REST call is.
            let inside_live_work = current_task().is_some_and(|t| self.tasks.drain_admits(&t));
            if !control && !inside_live_work {
                return Launched::Settled(Outcome::refused(crate::refusal::refusal(
                    crate::refusal::DRAINING,
                    "send: the kernel is draining for shutdown — no new work is \
                     accepted; in-flight work may finish",
                )));
            }
        }

        // 9. resolve — `"kernel"` names the root, else a literal id, else a
        // QUALIFIED `<guid>|<agent id>`: this kernel's own guid names a local
        // agent, a connected peer's guid an agent there (cross-kernel addressing).
        let Some(target) = self
            .resolve_target(item.target.as_str())
            .or_else(|| self.qualified_target(item.target.as_str()))
        else {
            return Launched::Settled(Outcome::refused(crate::refusal::unknown_agent(
                item.target.as_str(),
            )));
        };

        // 9b. THE TARGET'S OWN SECRETS — its bundle names paths of this payload
        // (`Bundle::payload_secret`: a credential store's `set` fields). They join the
        // item's marks before ANY refusal below, so a refused call row, the state
        // stream and every mirror show them as `[secret]` whoever sent the call.
        let marks = self.with_bundle_marks(&target, &item.payload, marks);

        // 10. ── CALL GATE — the caller's lines and the ancestor cap ──────
        // The operator (origin `None`) is ungated. Every AGENT send is judged on
        // the CALLER's side alone, by verb:
        // - a kernel verb sent to the root is not gated here: each checks itself
        //   (create/update/delete on the operand, `ps`/`cancel`/`ledger`/… own
        //   only, the process verbs operator only);
        // - a kernel verb sent to any other agent takes the gate like any verb;
        //   when it passes, dispatch answers `not_here`;
        // - `reflect` passes when the caller holds anything on the target;
        // - every other verb takes the gate.
        // A caller that holds nothing on the target gets the unknown answer,
        // byte-identical to a missing id; one that sees it is told what its lines
        // lack ([`Kernel::call_denial`]).
        if let Some(orig) = caller.as_ref() {
            let on_root = self.root().is_some_and(|r| r.id == target.id);
            let denial = if is_kernel_verb(&verb) && on_root {
                None
            } else if verb == "reflect" {
                (self.held(orig, &target.id) == gate::Held::Nothing)
                    .then(|| crate::refusal::unknown_agent(item.target.as_str()))
            } else if self.allowed(orig, &crate::authz::solver::Act::Call(verb.clone()), &target) {
                None
            } else {
                Some(self.call_denial(orig, &verb, &target, item.target.as_str()))
            };
            if let Some(denial) = denial {
                return Launched::Settled(Outcome::refused(denial));
            }
        }

        // The UNNAMED `kernel` reflect fast path. python checks the
        // LITERAL reserved address the caller sent (`target_id == "kernel"`,
        // not the resolved root id — a call spelling the root's real id
        // directly does NOT take this
        // path), applied AFTER the gate so it is authorized like any other
        // reflect (`_agent.py:846`). A NAMED or JOINING send (an `id` in the
        // payload) still opens the ledger (`_agent.py:816`) — this fast path
        // is unnamed-only: no caller-supplied id at all.
        let kernel_reflect = item.target.as_str() == "kernel" && verb == "reflect";
        if join_id.is_none() && name.is_none() && kernel_reflect {
            return Launched::KernelReflectFast(Box::new(FastReflect {
                target,
                payload: item.payload,
                timeout,
                caller,
                recipient,
            }));
        }

        // ONE ID SPACE, AND THE KERNEL OWNS IT. A caller supplies a NAME (or
        // an int JOIN) — the kernel supplies a FRESH id from its single
        // counter only when neither resolved to an existing execution.
        // Re-sending the same name (or an int naming one) JOINS the
        // execution that already opened; it never starts a second one and
        // never mints into the kernel's space. The mint stays SYNCHRONOUS:
        // the pid exists before the first await, which is what makes a whole
        // batch visible in `ps` at once.
        let execution_id = join_id.unwrap_or_else(|| self.mint_id());
        let task_id = TaskId(execution_id);

        match self.ledger.open(
            execution_id,
            &caller_id,
            &target.id,
            &item.payload,
            retries,
            name.as_deref(),
            caller.clone(),
        ) {
            crate::ledger::Opened::Refused(r) => {
                self.tasks.forget(&task_id);
                return Launched::Settled(Outcome::refused(*r));
            }
            // ALREADY SETTLED — the recorded outcome IS the answer, and the
            // handler must not run a second time. This is idempotency and
            // once-only execution in one field.
            crate::ledger::Opened::Settled(o, _) => {
                self.tasks.forget(&task_id);
                // ONLY ITS ORIGINATOR JOINS. The recorded outcome belongs to
                // whoever ran it, so handing it back is equivalent to having made
                // THAT call — a caller reusing another's id must not get its
                // secret verbatim.
                if let Some(d) = self.join_denial(&caller, execution_id, &target) {
                    return Launched::Settled(Outcome::refused(d));
                }
                return Launched::Recorded(
                    *o,
                    Box::new(Joiner {
                        timeout,
                        execution_id,
                        target: target.id.clone(),
                        verb,
                        recipient,
                    }),
                );
            }
            // IN FLIGHT — join it. A joiner's own `timeout` bounds only ITS
            // delivery: it does not own the execution and must not kill it.
            crate::ledger::Opened::Join(_) => {
                self.tasks.forget(&task_id);
                if let Some(d) = self.join_denial(&caller, execution_id, &target) {
                    return Launched::Settled(Outcome::refused(d));
                }
                return Launched::Joining(Box::new(Joiner {
                    timeout,
                    execution_id,
                    target: target.id.clone(),
                    verb,
                    recipient,
                }));
            }
            crate::ledger::Opened::Fresh => {
                // EVERY SEND IS A MESSAGE. The row names the call — who was
                // asked what, by which message — and takes the outcome's status
                // at settle; the reply body stays with the reply. `parent` is
                // the message that made the call. Written under the id the
                // ledger just opened, so the call, the task and the outcome are
                // all one number. Python `Agent._ledger_open`.
                //
                // The row carries the WHOLE call — the dispatch payload
                // verbatim, the retry budget, the attempt count as at dispatch,
                // the deadline — because it is what the kernel flushes into its
                // memory store, and what named-call dedup reads back after the
                // flush or a restart. `attempts` is updated on this row at every
                // retry while it is still in RAM. `caller` scopes who may see it
                // through `ledger` (it was `origin`; replies own `origin` now).
                let mut row = Map::new();
                row.insert("id".to_string(), Value::from(execution_id));
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
                    "caller".to_string(),
                    Value::from(caller.as_ref().map(|o| o.0.clone())),
                );
                row.insert(
                    "payload".to_string(),
                    taped_payload(
                        &crate::secret::shown(&item.payload, &marks),
                        item.binary.as_ref(),
                    ),
                );
                row.insert("named".to_string(), Value::from(name.is_some()));
                row.insert(
                    "reply_to".to_string(),
                    Value::from(recipient.as_ref().map(|r| r.0.clone())),
                );
                row.insert("retries".to_string(), Value::from(retries));
                row.insert("attempts".to_string(), Value::from(1));
                row.insert(
                    "timeout".to_string(),
                    timeout.as_ref().map_or(Value::Null, |d| d.given.clone()),
                );
                if let Some(n) = name.as_deref() {
                    row.insert("name".to_string(), Value::from(n));
                }
                if !marks.is_empty() {
                    // the marks are paths, not values
                    row.insert("secret".to_string(), json!(marks.as_ref()));
                }
                let _ = self.rows_put(&target.id, row);
            }
        }

        Launched::Dispatched(Box::new(Dispatch {
            target,
            payload: item.payload,
            verb,
            timeout,
            execution_id,
            task_id,
            binary: item.binary,
            caller,
            marks,
            recipient,
            // A JOIN never reaches here; a fresh NAMED `kernel` reflect does.
            named_fast_reflect: kernel_reflect,
        }))
    }

    /// START one launched item and answer its ACK: `accepted` with its pid (none
    /// for the unnamed `kernel` reflect fast path), or the launch's refusal, for
    /// which nothing runs and `then` is dropped. The item's ASYNC half runs as its
    /// own task — a child of the dispatch that called — and its settle goes to
    /// `then` when given, else to the item's `reply_to`.
    ///
    /// The async half runs the dispatch, enforces the item's own deadline, and —
    /// this is the RETRY ENGINE — re-dispatches a terminal `failed`/`timed_out`
    /// under the SAME id while the budget lasts. Only the FINAL outcome settles the
    /// ledger, so a joiner or a re-sender never sees an attempt that was later
    /// retried away.
    fn start_item(
        self: &Arc<Self>,
        launched: Launched,
        then: Option<deliver::Then>,
        caller: Option<AgentId>,
    ) -> Outcome {
        let launched = match launched {
            Launched::Settled(o) => return o,
            Launched::Joining(j) => {
                let ack = Outcome::accepted(Some(TaskId(j.execution_id)));
                self.join(*j, then, caller);
                return ack;
            }
            Launched::Recorded(o, j) => {
                // the recorded outcome is the settle, delivered as any settle is
                let ack = Outcome::accepted(Some(TaskId(j.execution_id)));
                let kernel = Arc::clone(self);
                tokio::spawn(Inherited::capture().scope(async move {
                    let Joiner { execution_id, target, verb, recipient, .. } = *j;
                    kernel
                        .deliver_to(then, recipient, Some(execution_id), &target, &verb, o, caller)
                        .await;
                }));
                return ack;
            }
            other => other,
        };
        let (pid, fast) = match &launched {
            Launched::Dispatched(d) => (Some(d.task_id), None),
            Launched::KernelReflectFast(fr) => (None, Some((fr.target.id.clone(), fr.recipient.clone()))),
            Launched::Settled(_) | Launched::Joining(_) | Launched::Recorded(..) => {
                unreachable!("answered above")
            }
        };
        let has_then = then.is_some();
        let kernel = Arc::clone(self);
        // the handler is a child of the dispatch that called, as for any call
        let parent = current_task();
        tokio::spawn(Inherited::capture().scope(async move {
            let run = kernel.settle_core(launched, has_then);
            let (out, owed) = match parent {
                Some(p) => CURRENT_TASK.scope(p, run).await,
                None => run.await,
            };
            match (owed, fast) {
                (None, Some((target, recipient))) => {
                    kernel
                        .deliver_to(then, recipient, None, &target, "reflect", out, caller)
                        .await;
                }
                (owed, _) => kernel.finish(owed, &out, then, caller).await,
            }
        }));
        Outcome::accepted(pid)
    }

    /// [`Self::settle`]'s run and record: the outcome, and — for an execution
    /// this item ran — what its settle still owes ([`deliver::Owed`]). `has_then`:
    /// a continuation ([`Self::call`]) takes the settle, not the receiver's
    /// `reply` verb.
    async fn settle_core(
        self: &Arc<Self>,
        launched: Launched,
        has_then: bool,
    ) -> (Outcome, Option<deliver::Owed>) {
        let d = match launched {
            // a refusal, a join and a recorded outcome are answered by `start`
            Launched::Settled(o) | Launched::Recorded(o, _) => return (o, None),
            Launched::Joining(j) => {
                return (
                    Outcome::refused(crate::refusal::refusal(
                        crate::refusal::BAD_ARG,
                        format!("send: execution {} is joined, not run", j.execution_id),
                    )),
                    None,
                )
            }
            Launched::KernelReflectFast(fr) => {
                let FastReflect {
                    target,
                    payload,
                    timeout,
                    caller,
                    ..
                } = *fr;
                let read = async {
                    // the kernel IS the agent here (the fast path answers `reflect`
                    // itself): it parses through the shared entry of the common verbs
                    let args = match kernel_reflect_args(&target, &payload) {
                        Ok(a) => a,
                        Err(refused) => return (refused, Status::Completed),
                    };
                    let mut reply = crate::reflect::reflect_identity(&target);
                    match crate::reflect::apply_reflect_flags(
                        self,
                        caller.as_ref(),
                        &target,
                        &args,
                        &mut reply,
                    )
                    .await
                    {
                        Ok(()) => (reply, Status::Completed),
                        Err(raised) => (raised, Status::Failed),
                    }
                };
                // pid stays None — no task table, no ledger, no fanout ever
                // touched this item. Still awaited under the item's own
                // deadline, as python awaits it: on expiry the read is dropped
                // and the item settles `timed_out` with `task_id: "-"`.
                let (reply, status) = match timeout {
                    None => read.await,
                    Some(d) => {
                        match tokio::time::timeout(std::time::Duration::from_secs_f64(d.secs), read)
                            .await
                        {
                            Ok(v) => v,
                            Err(_) => (
                                own_deadline_refusal(&target.id, "reflect", &d, json!("-")),
                                Status::TimedOut,
                            ),
                        }
                    }
                };
                return (Outcome::new(None, status, reply), None);
            }
            Launched::Dispatched(d) => *d,
        };
        let Dispatch {
            target,
            payload,
            verb,
            timeout,
            execution_id,
            task_id,
            binary,
            caller,
            marks,
            recipient,
            named_fast_reflect,
        } = d;

        // A KILLED CALLER STILL SETTLES. Killing a subtree drops its futures
        // without unwinding, so the settle below never runs for a call whose
        // caller died with it (a leg's `inbound_call` cancelled by a drain);
        // the guard records it `cancelled`, as python's `CancelledError` does on
        // its way out through `_settle`.
        let _settle_on_drop = SettleOnDrop {
            kernel: Arc::clone(self),
            execution_id,
            target: target.id.clone(),
            verb: verb.clone(),
            recipient: recipient.clone(),
        };
        let mut next_pid = task_id;
        let mut fast_first = named_fast_reflect;
        // The pid of the attempt that actually SETTLED — a retry re-dispatches
        // under a fresh row, so the last one is what the outcome names.
        let mut noted_marks: Vec<String> = Vec::new();
        let (pid, reply, status, body) = loop {
            let attempts = self.ledger.attempt(execution_id);
            if attempts > 1 {
                // THE ROW COUNTS REAL RUNS. The call row went down with
                // `attempts: 1`; every re-dispatch updates it in place while it
                // is in RAM, and the settle row carries the final count.
                let mut bump = Map::new();
                bump.insert("attempts".to_string(), Value::from(attempts));
                self.rows.stamp(execution_id, &bump);
            }
            if std::mem::take(&mut fast_first) {
                // The caller-NAMED `kernel` reflect's first attempt: a bare
                // identity read with the flags applied, under the ledger's id
                // but with no task row, no fanout and no deadline (python
                // `_launch`'s `flags_done` branch).
                let mut reply = crate::reflect::reflect_identity(&target);
                let (reply, status) = match kernel_reflect_args(&target, &payload) {
                    Err(refused) => (refused, Status::Errored),
                    Ok(args) => {
                        match crate::reflect::apply_reflect_flags(
                            self,
                            caller.as_ref(),
                            &target,
                            &args,
                            &mut reply,
                        )
                        .await
                        {
                            Ok(()) if error_shaped(&reply) => (reply, Status::Errored),
                            Ok(()) => (reply, Status::Completed),
                            Err(raised) => (raised, Status::Failed),
                        }
                    }
                };
                if !self.ledger.should_retry(execution_id, status) {
                    break (Some(task_id), reply, status, None);
                }
                next_pid = TaskId(execution_id);
                continue;
            }
            let mut this_pid: Option<TaskId> = None;
            // Reply marks the handler notes (`crate::secret::note_reply_secret`).
            let sink = crate::secret::ReplySink::default();
            let (mut reply, mut status, body) = self
                .run_dispatch(
                    &target,
                    &payload,
                    &verb,
                    Some(&mut this_pid),
                    timeout.clone(),
                    next_pid,
                    binary.clone(),
                    caller.clone(),
                    Arc::clone(&marks),
                    Arc::clone(&sink),
                )
                .await;
            noted_marks = sink.lock().map(|m| m.clone()).unwrap_or_default();
            // Universal post-process, PER ATTEMPT (python applies it inside
            // `_settle_once`, before `_record` weighs a retry): compose every
            // reflect reply with the tree/bundles flags. A roster
            // node whose own reflect RAISED fails this whole reflect, exactly as
            // the exception propagating out of python's `_roster` does.
            //
            // NOT for a remote target: its kernel already composed the reflect
            // with ITS flags over ITS tree; this kernel has nothing to add to it.
            // NOT after a named `kernel` reflect's first attempt either: python's
            // `flags_done` stays set for the retries.
            if verb == "reflect"
                && status == Status::Completed
                && !target.is_remote()
                && !named_fast_reflect
            {
                // The agent's own entry checked the call before it answered: its typed
                // form is decoded, not judged again.
                match crate::verbs::decode::<crate::declared::common::Reflect>("reflect", &payload)
                {
                    Err(refused) => {
                        reply = refused;
                        status = Status::Errored;
                    }
                    Ok(args) => {
                        if let Err(raised) = crate::reflect::apply_reflect_flags(
                            self,
                            caller.as_ref(),
                            &target,
                            &args,
                            &mut reply,
                        )
                        .await
                        {
                            reply = raised;
                            status = Status::Failed;
                        }
                    }
                }
            }
            if !self.ledger.should_retry(execution_id, status) {
                break (this_pid, reply, status, body);
            }
            // A RETRY IS THE SAME EXECUTION, so it re-registers under the SAME
            // id — named or not — so `cancel <pid>` on attempt two reaches the
            // work it is aimed at. Cancelling the id kills the current attempt
            // AND this loop: a cancel is absorbing.
            next_pid = TaskId(execution_id);
        };

        // NO FANOUT HERE. It ran in `run_dispatch`, BEFORE the handler — which is
        // where python puts it and what a watcher needs: mirroring after the
        // reply would show a watcher the request only once the work was done,
        // its echo landing after the handler's own progress events.
        // THE REPLY'S OWN SECRETS (`Bundle::reply_secret`): recorded as `[secret]`
        // by `stored_outcome` — the settle row, `ledger`, the store — while the
        // caller (and a joiner, from RAM) gets the reply whole.
        let reply_marks = self
            .local_bundle(&target)
            .map(|b| b.reply_secret(&payload, &reply))
            .unwrap_or_default();
        // plus the ones the handler noted for a reply it relayed from a connected
        // kernel's store.
        let reply_marks = crate::secret::well_formed(reply_marks, &noted_marks);
        let (out, joiners) = self.ledger.settle(
            execution_id,
            Outcome::new(pid, status, reply)
                .with_body(body)
                .with_secret(reply_marks),
        );
        // A SETTLE IS A ROW — see `stamp_call` — and then a MESSAGE: delivered to
        // the call's receiver (a continuation, else its `reply` verb). A settle
        // owed to nobody is its row alone.
        let reply_verb = !has_then && recipient.as_ref().is_some_and(|r| self.declares_reply(r));
        let owes = has_then || reply_verb;
        let settle_row = self.stamp_call(pid, &out, &target.id).await;
        // every joiner gets the settle once its row is written
        for j in joiners {
            j(out.clone());
        }
        let owed = deliver::Owed {
            pid: execution_id,
            target: target.id.clone(),
            verb,
            recipient: reply_verb.then_some(recipient).flatten(),
            settle_row: settle_row.filter(|_| owes),
        };
        (out, Some(owed))
    }

    /// The kernel's OWN dispatch of one payload — python `_run_dispatch`, kept
    /// for the distilled roster, which must reflect each node WITHOUT the
    /// reflect flags (no recursion into rosters).
    ///
    /// It is a call like any other: a minted id, a call row (`internal: true`
    /// tells a reader it was the kernel's own read, not a caller's send), a
    /// task-table row, the watcher fanout and the state events. It opens no
    /// ledger execution, so nothing stamps or settles the row.
    pub(crate) async fn internal_dispatch(
        self: &Arc<Self>,
        target: &Arc<Agent>,
        payload: &Value,
    ) -> (Value, Status) {
        let pid = self.mint_id();
        let caller = current_sender();
        let mut row = Map::new();
        row.insert("id".to_string(), Value::from(pid));
        row.insert("kind".to_string(), Value::from("call"));
        row.insert(
            "verb".to_string(),
            payload.get("type").cloned().unwrap_or(Value::Null),
        );
        row.insert(
            "parent".to_string(),
            Value::from(current_task().map(TaskId::get)),
        );
        row.insert(
            "caller".to_string(),
            Value::from(caller.as_ref().map(|o| o.0.clone())),
        );
        row.insert("payload".to_string(), payload.clone());
        row.insert("internal".to_string(), Value::from(true));
        let _ = self.rows_put(&target.id, row);
        let (reply, status, _no_body) = self
            .run_dispatch(
                target,
                payload,
                verb_of(payload),
                None,
                None,
                TaskId(pid),
                None,
                caller,
                Arc::new(Vec::new()),
                crate::secret::ReplySink::default(),
            )
            .await;
        (reply, status)
    }

    /// ONE KERNEL BOOKKEEPING CALL — the kernel runs `agent`'s handler DIRECTLY:
    /// the boot of a child `create_agent` just made and the boots at
    /// `kernel_start`, the `shutdown` of a stop, the reads and writes of the
    /// kernel's ledger in its memory store, a record or sidecar written through
    /// the loader. Python `Kernel.bookkeeping` / `Agent._kernel_run`.
    ///
    /// A plain call, not a send — no gate, no task row, no call row, no ledger —
    /// so it is the same whoever's dispatch asked for it (the operator, or the
    /// root answering a kernel verb). The handler runs AS `agent` (its sends and
    /// the tasks it spawns are its own) with origin `None`: the kernel asked. Like
    /// any dispatch it bumps the agent's in-flight count and fans the payload out
    /// first. `body` is a raw request body at its payload path.
    ///
    /// Answers the outcome a caller branches on: a raise (or a panic) is
    /// `failed`, a refusal `errored`, anything else `completed`; `pid` is `None`.
    pub(crate) async fn bookkeeping(
        self: &Arc<Self>,
        agent: &Arc<Agent>,
        payload: Value,
        body: Option<crate::outcome::Blob>,
    ) -> Outcome {
        self.bookkeeping_as(agent, payload, body, None).await
    }

    /// [`Self::bookkeeping`] with the handler's origin `origin`: a lifecycle verb
    /// ([`Self::announce`]) runs with the agent itself as its origin. The sender
    /// stays `agent`.
    pub(crate) async fn bookkeeping_as(
        self: &Arc<Self>,
        agent: &Arc<Agent>,
        payload: Value,
        body: Option<crate::outcome::Blob>,
        origin: Option<AgentId>,
    ) -> Outcome {
        use futures_util::FutureExt;
        let _in_flight = InFlightGuard::enter(self, agent);
        let item = match body.clone() {
            Some(blob) => {
                let mut head = payload.clone();
                let _ = blob.place(&mut head, Value::Null);
                crate::inbox::InboxItem::binary(head, blob)
            }
            None => crate::inbox::InboxItem::text(payload.clone()),
        };
        self.fanout(agent, "send", &item, current_sender().as_ref())
            .await;
        let mut for_handler = payload;
        if let Some(b) = &body {
            let _ = b.take(&mut for_handler);
        }
        let run = IN_BOOKKEEPING.scope(
            true,
            crate::secret::CURRENT_SECRET.scope(
                Arc::new(Vec::new()),
                CURRENT_ORIGIN.scope(
                    origin,
                    CURRENT_SENDER.scope(
                        Some(agent.id.clone()),
                        dispatch(Arc::clone(self), Arc::clone(agent), for_handler, body),
                    ),
                ),
            ),
        );
        let (result, status, body) = match std::panic::AssertUnwindSafe(run).catch_unwind().await {
            Ok(Ok((v, b))) if error_shaped(&v) => (v, Status::Errored, b),
            Ok(Ok((v, b))) => (v, Status::Completed, b),
            Ok(Err(raised)) => (raised, Status::Failed, None),
            Err(p) => (
                json!({ "error": format!("panic: {}", panic_message(&p)), "raised": true }),
                Status::Failed,
                None,
            ),
        };
        Outcome::new(None, status, result).with_body(body)
    }

    /// Settle the call: append a `settle` row carrying what the kernel KEEPS of
    /// the outcome ([`crate::ledger::stored_outcome`]: the whole result for a
    /// named call, the error text for a dead letter, `{status, bytes}` otherwise).
    ///
    /// THE SETTLE IS ITS OWN ROW — a row is append-only, so the ending is an
    /// append, never an edit of the call row. With the call row it is the record
    /// of what a call became, and it is what the kernel flushes into its memory
    /// store and reads back for `ledger` and for named-call dedup.
    ///
    /// The call row may already be flushed (and gone from RAM) by now; the target
    /// comes from the live execution. While the call row is still in RAM it takes
    /// the python stamp `{status, error?, reason?}` (the error cut to 200 CODE
    /// POINTS, both optional keys by python truthiness). ON THE ROOT: a settle is
    /// KERNEL bookkeeping; `settled` names the target. A dead ending (failed /
    /// timed_out / cancelled) is ANNOUNCED — `on_deadletter` — because nothing
    /// retries it on its own: someone has to look.
    ///
    /// Answers the settle row's id: once its delivery ran, a `delivered` row
    /// naming it is appended ([`Self::finish`]); the settle row is never touched.
    async fn stamp_call(
        self: &Arc<Self>,
        pid: Option<TaskId>,
        out: &Outcome,
        target: &AgentId,
    ) -> Option<i64> {
        let (dead, row) = self.stamp_call_rows(pid, out, target);
        if let Some(pid) = dead {
            self.announce_deadletter(pid.get(), out, out.attempts).await;
        }
        row
    }

    /// The rows half of [`Self::stamp_call`], synchronous: the call row's
    /// `outcome` and the root's settle row. `Some(pid)` when the outcome is a
    /// dead letter still to be announced, and the settle row's id.
    fn stamp_call_rows(
        &self,
        pid: Option<TaskId>,
        out: &Outcome,
        target: &AgentId,
    ) -> (Option<TaskId>, Option<i64>) {
        let Some(pid) = pid else {
            return (None, None);
        };
        let mut stamp = Map::new();
        stamp.insert("status".to_string(), Value::from(out.status.as_str()));
        // review bug 3: the error text is read from the SHOWN result, so a value the
        // bundle marked (`Outcome.secret`, e.g. an `error` path) never reaches the row.
        let shown_result = crate::secret::shown(&out.result, &out.secret);
        if let Some(res) = shown_result.as_object() {
            if let Some(err) = crate::refusal::error_text(&shown_result) {
                stamp.insert(
                    "error".to_string(),
                    Value::from(err.chars().take(200).collect::<String>()),
                );
                if let Some(reason) = res.get("reason").filter(|v| !v.is_null()) {
                    stamp.insert("reason".to_string(), reason.clone());
                }
            }
        }
        let mut onto = Map::new();
        onto.insert("outcome".to_string(), Value::Object(stamp));
        self.rows.stamp(pid.get(), &onto);
        let named = self
            .ledger
            .info(pid.get())
            .is_some_and(|(_, _, name)| name.is_some());
        let settled = self
            .ledger
            .info(pid.get())
            .map_or_else(|| target.clone(), |(t, _, _)| t);
        let mut row = None;
        if let Some(root) = self.root() {
            let mut settle = Map::new();
            settle.insert("kind".to_string(), Value::from("settle"));
            settle.insert("parent".to_string(), Value::from(pid.get()));
            settle.insert("settled".to_string(), Value::from(settled.0));
            settle.insert(
                "outcome".to_string(),
                crate::ledger::stored_outcome(out, named, crate::jsonsafe::BodyAs::Len),
            );
            settle.insert("attempts".to_string(), Value::from(out.attempts));
            row = self.rows_put(&root.id, settle).ok();
        }
        (crate::ledger::is_dead(out.status).then_some(pid), row)
    }

    /// Run ONE dispatch as a registered, killable task and await it — the unit
    /// of the task table.
    ///
    /// **Parent propagation.** `tokio::spawn` MOVES the future onto a fresh
    /// task, and task-locals do NOT inherit across that move — so the scopes
    /// are re-established INSIDE the spawned task: `CURRENT_SENDER` (the target:
    /// identity comes from dispatch), `CURRENT_ORIGIN` (the caller),
    /// `CURRENT_TASK` (this dispatch's own id, so any `send` the
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
    // deadline, the task id, (since raw bodies joined the batch) the request
    // body, and the caller the gate judged. Bundling them into a struct would
    // only move the same eight fields.
    #[allow(clippy::too_many_arguments)]
    async fn run_dispatch(
        self: &Arc<Self>,
        target: &Arc<Agent>,
        payload: &Value,
        verb: &str,
        task_out: Option<&mut Option<TaskId>>,
        timeout: Option<Deadline>,
        task_id: TaskId,
        binary: Option<crate::outcome::Blob>,
        caller: Option<AgentId>,
        marks: Arc<Vec<String>>,
        sink: crate::secret::ReplySink,
    ) -> (Value, Status, Option<crate::outcome::Blob>) {
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
        // `_in_flight`, fans out, and only then resolves the verb. The events
        // name the CALLER the gate judged, never the recipient. The guard
        // publishes the matching `drain` on every exit path, timeout included.
        //
        // A REMOTE TARGET is not an agent of this tree: nobody here can watch it
        // and it has no backlog to report, so neither the in-flight count nor
        // the fanout applies (python's `RemoteTarget` has neither). It keeps
        // everything else — the task row above, the deadline, the settle.
        let _in_flight = (!target.is_remote()).then(|| InFlightGuard::enter(self, target));
        if !target.is_remote() {
            // A MARKED call is mirrored as shown (`crate::secret`).
            let payload = &crate::secret::shown(payload, &marks);
            let item = match binary.clone() {
                // A REQUEST BODY rides at ITS path — where python's own payload
                // dict holds it, in the caller's position when the payload names
                // the slot — so a watcher receives the body, not a header with a
                // hole where it was. A call that only READS a raw chunk carries
                // none: its payload is a plain dict, a text frame.
                Some(blob) => {
                    let mut head = payload.clone();
                    // admitted at launch: the path names a slot of the payload.
                    // The watcher's frame names it step by step, as python's
                    // encoder does (`_binary_path` is a list of steps).
                    let _ = blob.place(&mut head, Value::Null);
                    crate::inbox::InboxItem::binary(head, blob)
                }
                None => crate::inbox::InboxItem::text(payload.clone()),
            };
            self.fanout(target, "send", &item, caller.as_ref()).await;
        }
        // ONE DISPATCH, whatever the call carries — python's one `handle`. A
        // request body IS the payload's value at its path (python's
        // `payload[field] = body` / `set_path` overwrites the slot), so the
        // header the handler gets holds nothing there — a caller names the slot
        // only to keep python's key order on the row and the watcher frame. Any
        // handler may answer a reply body; nothing about the call decides that.
        let mut payload_for_handler = payload.clone();
        if let Some(b) = &binary {
            // a payload that only NAMED the slot holds `null` there; one that
            // did not leaves nothing to take
            let _ = b.take(&mut payload_for_handler);
        }

        // a deferred settle's own reply marks land where the handler's would
        let deferred_marks = Arc::clone(&sink);
        let join = {
            let kernel = Arc::clone(self);
            let tgt = Arc::clone(target);
            let payload = payload_for_handler;
            // IDENTITY COMES FROM DISPATCH: inside the spawned task the sender is
            // the TARGET, so every send its handler makes is the target's.
            let sender = target.id.clone();
            // The CALLER the gate judged — what the handler reads as its origin.
            let origin = caller;
            let tid = task_id;
            let cancel = handle.cancel_signal();
            let killed = handle.killed_flag();
            // Inside the kernel's own ledger flush, the dispatch it starts is
            // silent too (python copies `_state_quiet` into the task).
            let quiet = state_quiet();
            tokio::spawn(async move {
                STATE_QUIET
                    .scope(
                        quiet,
                        crate::secret::REPLY_SECRET.scope(
                            sink,
                            crate::secret::CURRENT_SECRET.scope(
                                marks,
                                CURRENT_ORIGIN.scope(
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
                                                    // A request body rides under this same
                                                    // row/ledger/timeout, and ANY REPLY BODY
                                                    // comes back with it: the batch IS
                                                    // the carrier for bytes, so any verb
                                                    // through `send` returns its chunk the
                                                    // way python's reply dict holds one.
                                                    v = async {
                                                        // ANOTHER KERNEL'S AGENT: the call
                                                        // goes to its peer's route, bytes
                                                        // and all; no handler runs here. The
                                                        // execution defers: that kernel's
                                                        // answer settles it, verbatim (#795).
                                                        if let Some(remote) = tgt.remote.as_ref() {
                                                            return Ok(crate::peers::call_peer(
                                                                &kernel, remote, payload, binary,
                                                            )
                                                            .await);
                                                        }
                                                        dispatch(kernel, tgt, payload, binary).await
                                                    } => Some(v),
                                                }
                                            }),
                                        ),
                                    ),
                                ),
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
        let stop = handle.cancel_signal();
        let _guard = DispatchGuard {
            kernel: Arc::clone(self),
            id: task_id,
            handle,
        };

        // THE HANDLER, then — when it DEFERRED its settle ([`Self::defer`]) — the
        // settle another agent's answer brings ([`Self::settle_deferred`]). The
        // handler's own task has ended by then; what stays is this execution's
        // row (`ps` shows it, `cancel` and the deadline reach it).
        let work = async {
            let ran = join.await;
            if !matches!(ran, Ok(Some(Ok(_)))) {
                self.defers.forget(task_id);
                return Ran::Handler(ran);
            }
            let Some(rx) = self.defers.waiting(task_id) else {
                return Ran::Handler(ran);
            };
            let settled = tokio::select! {
                biased;
                _ = stop.notified() => None,
                o = rx => o.ok(),
            };
            self.defers.forget(task_id);
            match settled {
                Some(o) => Ran::Deferred(o),
                None => Ran::Handler(Ok(None)),
            }
        };

        // THE ITEM'S OWN DEADLINE. Expiry kills through the FRONT DOOR
        // (`cancel_task`) so every bundle's `on_cancel` hook runs BEFORE the
        // abort — dropping the future would strand the OS resources those hooks
        // release (a subprocess, a PTY, a socket). Each hook runs as its own
        // agent (`cancel_task`), never as the batch caller.
        //
        // The race is reported honestly: a handler that finishes during the
        // grace window COMPLETES — `cancel_task` no-ops on a finished id and
        // the join below yields its real reply. A deferred settle that arrives
        // after the deadline is LATE: nothing waits for it any more.
        let outcome = match timeout {
            None => work.await,
            Some(d) => {
                let dur = std::time::Duration::from_secs_f64(d.secs);
                match tokio::time::timeout(dur, work).await {
                    Ok(j) => j,
                    Err(_) => {
                        self.defers.forget(task_id);
                        self.cancel_task(&task_id).await;
                        // PYTHON'S PROSE, verbatim: target id RAW, verb
                        // `py_repr`-quoted, the timeout as given, plus `task_id`.
                        let r = own_deadline_refusal(&target.id, verb, &d, json!(task_id.0));
                        return (r, Status::TimedOut, None);
                    }
                }
            }
        };
        let outcome = match outcome {
            // the settle the deferring handler's answer brought, as given; its
            // reply marks (a far store's `get` fields) are this reply's
            Ran::Deferred(o) => {
                if let Ok(mut m) = deferred_marks.lock() {
                    m.extend(o.secret);
                }
                return (o.result, o.status, o.body);
            }
            Ran::Handler(o) => o,
        };

        match outcome {
            // A RAISE — a bundle `Err`, a python exception a kernel verb
            // emulates, or a panic below — is the ONE terminal handler failure:
            // retryable, and a dead-letter row when the budget runs out. Known
            // out of band, never read off the reply: a REPLY that merely carries
            // `raised: true` (a peer's or a relay's answer, passed on verbatim)
            // is a returned dict, and python settles it errored.
            Ok(Some(Err(raised))) => (raised, Status::Failed, None),
            // FAILURE IS A STATUS. A handler that RAN and said no — a missing
            // path, a patch that did not match, a refusal it composed itself —
            // settles `errored`, not `completed`. The reply is kept verbatim
            // either way; what changes is that a reader learns the outcome from
            // `status` instead of parsing `result`. Retries are unchanged:
            // `is_retryable` covers the transport classes only, so a semantic
            // no is never re-sent.
            Ok(Some(Ok((v, body)))) if error_shaped(&v) => (v, Status::Errored, body),
            Ok(Some(Ok((v, body)))) => (v, Status::Completed, body),
            Ok(None) => {
                if current_killed() {
                    // We are in the killed subtree — park; our own stop signal
                    // lands at this yield and drops us. Never resume.
                    std::future::pending::<()>().await;
                    unreachable!("a killed task is dropped at its next yield");
                }
                (json!({ "cancelled": true }), Status::Cancelled, None)
            }
            // A handler PANIC is a result, not a crash of the caller: it settles
            // as `failed` with `raised: true`. `send` propagates nothing — every
            // net a caller wrapped around it is now dead code.
            Err(e) if e.is_panic() => {
                let why = panic_message(&e.into_panic());
                (
                    json!({ "error": format!("panic: {why}"), "raised": true }),
                    Status::Failed,
                    None,
                )
            }
            Err(_) => (json!({ "cancelled": true }), Status::Cancelled, None),
        }
    }

    /// Emit a payload to a target's inbox without dispatching. Returns
    /// immediately. Publishes a `kind:"emit"` state event and mirrors to
    /// watchers — the same fanout a dispatch runs, minus the in-flight bump.
    ///
    /// AN EMIT TO A NON-AGENT ID IS A NO-OP. The target agent is resolved FIRST:
    /// no agent, no inbox put, no state event — so a synthetic WS client id
    /// (`_ws_<hex>`) produces no telemetry for a sprite that does not exist, as
    /// python drops it silently (`_proxy.py:196-200`).
    pub async fn emit(self: &Arc<Self>, target_id: &AgentId, payload: Value) {
        self.emit_item(target_id, crate::inbox::InboxItem::text(payload))
            .await;
    }

    /// [`Self::emit`] for a payload carrying raw bytes — a `read_stream` chunk a
    /// bridge re-emits from a remote watch, which must reach a local watcher
    /// WHOLE. Same no-op rule for a non-agent id.
    ///
    /// GATED LIKE A SEND when the emitter is an agent: its lines (and the ancestor
    /// cap) must allow verb `emit` on the target. Its own tree, itself included,
    /// it reaches by position. A denied emit is dropped; an emit has no reply.
    /// NO ORIGIN SCOPE, NO EMIT, as with [`Self::send`]: work spawned with a bare
    /// `tokio::spawn` is never the operator, and its emit is dropped.
    ///
    /// A QUALIFIED `<guid>|<agent>` OF A CONNECTED PEER goes to every live route
    /// of that guid ([`crate::peers::emit_peer`]) — every tab of one identity.
    pub async fn emit_item(self: &Arc<Self>, target_id: &AgentId, item: crate::inbox::InboxItem) {
        self.emit_item_with(target_id, item, true).await;
    }

    /// [`Self::emit`], QUIET: the payload reaches the target's
    /// inbox and its watchers and NOTHING is published on the state stream — a
    /// streaming `part_delta` arrives at token rate, and the state stream carries a
    /// part's lifecycle, never its every delta. Python `Agent.emit(..., quiet=True)`.
    pub async fn emit_quiet(self: &Arc<Self>, target_id: &AgentId, payload: Value) {
        self.emit_item_with(target_id, crate::inbox::InboxItem::text(payload), false)
            .await;
    }

    /// [`Self::emit_quiet`] for a payload that may carry raw bytes: a wire leg's
    /// `settle` for its watchers (a reply body included), kept off the state stream.
    pub async fn emit_item_quiet(self: &Arc<Self>, target_id: &AgentId, item: crate::inbox::InboxItem) {
        self.emit_item_with(target_id, item, false).await;
    }

    async fn emit_item_with(
        self: &Arc<Self>,
        target_id: &AgentId,
        item: crate::inbox::InboxItem,
        publish: bool,
    ) {
        if origin_scope().is_none() {
            return;
        }
        let Some(target) = self.agents.get(target_id).map(|e| Arc::clone(&e)) else {
            if let Some(remote) = self
                .qualified_target(target_id.as_str())
                .filter(|a| a.is_remote())
            {
                crate::peers::emit_peer(self, &remote, item).await;
            }
            return;
        };
        let sender = current_sender();
        if let Some(orig) = sender.as_ref() {
            if self.gate(orig, "emit", &target).is_some() {
                return;
            }
        }
        self.fanout_with(&target, "emit", &item, sender.as_ref(), publish)
            .await;
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
    /// around the caller's lines: an agent that may not CALL an agent could still
    /// read everything sent TO it. The watcher needs a `watch` line on the source
    /// (or the source in its own tree).
    ///
    /// Ungated, a browser leg granted exactly `assistant:[send]` could mirror ANY
    /// agent's inbox in its session and read the packed confidential context out
    /// of the model's prompt.
    ///
    /// An OPERATOR watch (`current_sender()` is `None` inside an origin scope) is
    /// the origin root of trust and stays ungated, as with send. With NO ORIGIN
    /// SCOPE at all (a bare `tokio::spawn`) the watch is refused with the unknown
    /// answer: the absence of an identity is never the operator.
    ///
    /// A QUALIFIED `<guid>|<agent>` OF A CONNECTED PEER watches that agent on its
    /// kernel ([`crate::peers`]): the same gate, the same reader rule.
    pub async fn watch(self: &Arc<Self>, src_id: &AgentId, watcher_id: AgentId) -> Option<Value> {
        if origin_scope().is_none() {
            return Some(crate::refusal::unknown_agent(src_id.as_str()));
        }
        let Some(src) = self.agents.get(src_id).map(|e| Arc::clone(&e)) else {
            let remote = self
                .qualified_target(src_id.as_str())
                .filter(|a| a.is_remote())?;
            return self.watch_peer(&remote, watcher_id).await;
        };
        if let Some(orig) = current_sender() {
            if let Some(denial) = self.gate(&orig, "watch", &src) {
                return Some(denial);
            }
        }
        if let Some(r) = self.no_consumer(&watcher_id, src_id) {
            return Some(r);
        }
        {
            src.watcher_ids.write().await.insert(watcher_id.clone());
        }
        None
    }

    /// A WATCHER MUST ALREADY BE A READER: a lazily created inbox would be a
    /// sender with no receiver, so the mirror would go nowhere and the caller
    /// would have no way to know. Registering a consumer is what `watch`
    /// requires, not what it does. The refusal, or `None`.
    pub(crate) fn no_consumer(&self, watcher_id: &AgentId, src_id: &AgentId) -> Option<Value> {
        if self.inboxes.contains_key(watcher_id) {
            return None;
        }
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
        Some(r)
    }

    /// May `watcher` watch the STATE STREAM of `node`'s subtree? Refusal, or
    /// `None`.
    ///
    /// The state stream is a `watch` on a node (grant model v9): the watcher's
    /// lines and the ancestor cap must allow `watch` on `node`, as for any send.
    /// What it then receives is cut per watcher ([`Self::state_watch_event`],
    /// [`Self::state_watch_snapshot`]). An OPERATOR tap (no sender inside an
    /// origin scope) is the origin root of trust and stays ungated, as with send
    /// and watch; with no origin scope at all it is refused.
    ///
    /// An ingress rule answers "may this client reach this leg"; it says nothing
    /// about what the client may then READ.
    pub fn authorize_state_watch(&self, watcher: &AgentId, node: &AgentId) -> Option<Value> {
        if origin_scope().is_none() {
            return Some(crate::refusal::unknown_agent(node.as_str()));
        }
        current_sender()?;
        let Some(n) = self.agents.get(node).map(|e| Arc::clone(&e)) else {
            return Some(crate::refusal::unknown_agent(node.as_str()));
        };
        self.gate(watcher, "watch", &n)
    }

    /// Does `watcher`, watching `node`, cover agent `id`: `id` is in `node`'s
    /// subtree and the watcher holds anything on it?
    fn state_watch_covers(&self, watcher: &AgentId, node: &AgentId, id: &AgentId) -> bool {
        self.in_tree(id, node) && self.held(watcher, id) != gate::Held::Nothing
    }

    /// One state event as `watcher`, watching `node`, sees it, or `None` when
    /// it gets none: only an event whose `agent_id` is in `node`'s subtree and
    /// covered by the watcher passes, and a `sender` the watcher cannot see
    /// carries no id (its value becomes `null`).
    ///
    /// For the watcher path of a transport; an in-process bookkeeping
    /// subscriber ([`Self::add_state_subscriber`]) reads the stream unfiltered.
    pub fn state_watch_event(
        &self,
        watcher: &AgentId,
        node: &AgentId,
        event: &Value,
    ) -> Option<Value> {
        let Value::Object(ev) = event else {
            return None;
        };
        let id = AgentId::from(ev.get("agent_id").and_then(Value::as_str)?);
        if !self.state_watch_covers(watcher, node, &id) {
            return None;
        }
        let mut ev = ev.clone();
        if let Some(sender) = ev.get("sender").and_then(Value::as_str).map(AgentId::from) {
            if self.held(watcher, &sender) == gate::Held::Nothing {
                ev.insert("sender".to_string(), Value::Null);
            }
        }
        Some(Value::Object(ev))
    }

    /// [`Self::state_snapshot`] cut to the agents of `node`'s subtree that
    /// `watcher` covers.
    pub fn state_watch_snapshot(&self, watcher: &AgentId, node: &AgentId) -> Vec<Value> {
        self.state_snapshot()
            .into_iter()
            .filter(|e| {
                e.get("agent_id").and_then(Value::as_str).is_some_and(|id| {
                    self.state_watch_covers(watcher, node, &AgentId::from(id))
                })
            })
            .collect()
    }

    /// Detach a previously-registered watcher.
    ///
    /// For a qualified `<guid>|<agent>` this forgets a PEER watch — connected or
    /// not.
    pub async fn unwatch(&self, src_id: &AgentId, watcher_id: &AgentId) {
        let local = self.agents.get(src_id).map(|e| Arc::clone(&e));
        match local {
            Some(src) => {
                src.watcher_ids.write().await.remove(watcher_id);
            }
            None => self.unwatch_peer(src_id.as_str(), watcher_id).await,
        }
    }

    /// Whether anything still mirrors `src_id`'s traffic: a watcher that has
    /// registered and not yet detached. An agent that is gone has none.
    pub async fn has_watchers(&self, src_id: &AgentId) -> bool {
        let Some(src) = self.agents.get(src_id).map(|e| Arc::clone(&e)) else {
            return false;
        };
        let watched = !src.watcher_ids.read().await.is_empty();
        watched
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
        sender: Option<&AgentId>,
    ) {
        self.fanout_with(target, kind, item, sender, true).await;
    }

    /// [`Self::fanout`] of an item each receiver gets its OWN copy of: the
    /// target and each watcher that is an agent get `item_for(Some(id))`; a
    /// watcher that is no agent and the state stream get `item_for(None)`. A
    /// receiver whose `item_for` answers `None` gets nothing (for kernel
    /// bookkeeping that carries a record, cut per receiver).
    pub(crate) async fn fanout_each(
        &self,
        target: &Arc<Agent>,
        kind: &str,
        item_for: impl Fn(Option<&AgentId>) -> Option<crate::inbox::InboxItem>,
        sender: Option<&AgentId>,
    ) {
        if state_quiet() {
            return;
        }
        let public = item_for(None);
        if let Some(item) = item_for(Some(&target.id)) {
            if let Some(tx) = self.inboxes.get(&target.id) {
                tx.send(item);
            }
        }
        if let Some(item) = &public {
            let summary = summarize_payload(&item.payload, item.body.as_ref());
            self.publish_state_with_body(
                &traffic_event(&target.id, kind, target.in_flight(), sender, &summary, item),
                item.body.as_ref(),
            );
        }
        let watchers: Vec<AgentId> = target.watcher_ids.read().await.iter().cloned().collect();
        for w in watchers {
            let Some(tx) = self.inboxes.get(&w).map(|e| e.clone()) else {
                continue;
            };
            let wa = self.agents.get(&w).map(|e| Arc::clone(&e));
            let mine = if wa.is_some() { item_for(Some(&w)) } else { public.clone() };
            if let Some(item) = mine {
                tx.send(item);
            }
            if let (Some(wa), Some(item)) = (wa, &public) {
                let summary = summarize_payload(&item.payload, item.body.as_ref());
                self.publish_state_with_body(
                    &traffic_event(&w, kind, wa.in_flight(), sender, &summary, item),
                    item.body.as_ref(),
                );
            }
        }
    }

    /// [`Self::fanout`]; `publish: false` delivers to the inboxes and publishes
    /// nothing on the state stream ([`Self::emit_quiet`]).
    async fn fanout_with(
        &self,
        target: &Arc<Agent>,
        kind: &str,
        item: &crate::inbox::InboxItem,
        sender: Option<&AgentId>,
        publish: bool,
    ) {
        // THE KERNEL'S OWN LEDGER WRITES AND READS REACH NO INBOX either: inside them
        // (`STATE_QUIET`) the store's dispatch is mirrored to no watcher, and what the
        // store emits (`appended` per execution row) reaches nobody. A view watching the
        // store would read it again on each, its read would row, the row flush, forever.
        if state_quiet() {
            return;
        }
        // `sender` is THE CALLER, not the target; an operator send carries JSON
        // null.
        let summary = summarize_payload(&item.payload, item.body.as_ref());
        if let Some(tx) = self.inboxes.get(&target.id) {
            // Never blocks the emitter, and at the bound the OLDEST payload is
            // evicted — python's `_put_drop_oldest`
            // rule.
            tx.send(item.clone());
        }
        if publish {
            self.publish_state_with_body(
                &traffic_event(&target.id, kind, target.in_flight(), sender, &summary, item),
                item.body.as_ref(),
            );
        }
        let watchers: Vec<AgentId> = target.watcher_ids.read().await.iter().cloned().collect();
        for w in watchers {
            let Some(tx) = self.inboxes.get(&w).map(|e| e.clone()) else {
                continue;
            };
            tx.send(item.clone());
            if !publish {
                continue;
            }
            if let Some(wa) = self.agents.get(&w).map(|e| Arc::clone(&e)) {
                self.publish_state_with_body(
                    &traffic_event(&w, kind, wa.in_flight(), sender, &summary, item),
                    item.body.as_ref(),
                );
            }
        }
    }
}

/// One traffic state event, in python's key order:
/// `{agent_id, kind, backlog, sender, summary, payload}` (`ts` is stamped by
/// `publish_state`). `sender` is the originator or JSON null — never the target's
/// own id.
///
/// BUILT BY HAND, not by `json!`. The macro routes a `Value` operand through
/// `serde_json::to_value(..).unwrap()`, and that round-trip PANICS on the one
/// number python writes and JSON has no grammar for: `Infinity`/`NaN`, which
/// this kernel stores as an arbitrary-precision literal and serde's
/// number emitter re-parses. A payload is a caller's, so `{"nan": nan}` on the
/// CLI would take the whole process down with `invalid number`, where python
/// prints the record. Nothing that carries a caller's value may go through
/// `json!`.
fn traffic_event(
    agent_id: &AgentId,
    kind: &str,
    backlog: i64,
    sender: Option<&AgentId>,
    summary: &str,
    item: &crate::inbox::InboxItem,
) -> Value {
    let mut out = Map::new();
    out.insert("agent_id".to_string(), Value::from(agent_id.0.clone()));
    out.insert("kind".to_string(), Value::from(kind));
    out.insert("backlog".to_string(), Value::from(backlog));
    out.insert(
        "sender".to_string(),
        Value::from(sender.map(|s| s.0.clone())),
    );
    out.insert("summary".to_string(), Value::from(summary));
    out.insert("payload".to_string(), item.payload.clone());
    Value::Object(out)
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

/// Best-effort human text out of a panic payload — the two shapes
/// `panic!` produces plus a fallback, so a handler that panicked with a
/// non-string never reads as "panic: <nothing>". `pub(crate)`: also the
/// state-subscriber isolation in [`crate::kernel`].
pub(crate) fn panic_message(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "handler panicked".to_string()
    }
}

/// Caller-side guard for one dispatch: on ANY exit path (reply, kill, or the
/// caller's own future being dropped) it stops the dispatch task and drops its
/// row. Stopping a task that already finished is a no-op; forgetting a row that
/// `cancel` already removed is a no-op — both are idempotent by design.
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

/// Settles an execution `cancelled` when its settling future is dropped before
/// the ledger holds an outcome — the caller was killed. The ledger and the rows
/// are written here, synchronously, so a drain waiting on `unsettled` sees the
/// call end; the dead-letter announcement is a fanout and runs as its own task.
struct SettleOnDrop {
    kernel: Arc<Kernel>,
    execution_id: i64,
    target: AgentId,
    /// The call's verb and receiver: the `cancelled` settle is delivered like any.
    verb: String,
    recipient: Option<AgentId>,
}

impl Drop for SettleOnDrop {
    fn drop(&mut self) {
        if self.kernel.ledger.is_done(self.execution_id) {
            return;
        }
        let pid = Some(TaskId(self.execution_id));
        let (out, joiners) = self.kernel.ledger.settle(
            self.execution_id,
            Outcome::new(pid, Status::Cancelled, json!({ "cancelled": true })),
        );
        let recipient = self
            .recipient
            .take()
            .filter(|r| self.kernel.declares_reply(r));
        let (dead, row) = self.kernel.stamp_call_rows(pid, &out, &self.target);
        for j in joiners {
            j(out.clone());
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let kernel = Arc::clone(&self.kernel);
            let owed = deliver::Owed {
                pid: self.execution_id,
                target: self.target.clone(),
                verb: std::mem::take(&mut self.verb),
                settle_row: row.filter(|_| recipient.is_some()),
                recipient,
            };
            rt.spawn(async move {
                if let Some(pid) = dead {
                    kernel
                        .announce_deadletter(pid.get(), &out, out.attempts)
                        .await;
                }
                kernel.finish(Some(owed), &out, None, None).await;
            });
        }
    }
}

/// Resolve verb → system handler or bundle handler — THE ONE DISPATCH every
/// call takes, a request body or not (python `_agent.py`'s one `handle`).
///
/// The bundle is reached through [`crate::bundle::Bundle::handle_binary`], so
/// ANY verb may answer a raw reply body beside its JSON — python's reply dict
/// holding `bytes` at a key. There is no verb list and no flag on the call: a
/// leg answers binary exactly when the outcome carries a body.
///
/// BOXED, not `async fn`, and deliberately so: the dispatch path recurses
/// (`send` → dispatch → `create_agent` → `send` → …), and since `send` now
/// SPAWNS its dispatch, that cycle would force the compiler to prove
/// `Send`-ness of a future in terms of itself — auto-trait inference gives up
/// and reports "not Send". Declaring one concrete `dyn Future + Send` at the
/// recursion point cuts the cycle.
///
/// `Err` is a RAISE (python's handler exception, settled `failed`); `Ok` is a
/// reply, whatever it carries, with the reply body at its slot.
#[allow(clippy::type_complexity)] // the Pin<Box<dyn Future>> IS the point (see above).
fn dispatch(
    kernel: Arc<Kernel>,
    target: Arc<Agent>,
    payload: Value,
    body: Option<crate::outcome::Blob>,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(Value, Option<crate::outcome::Blob>), Value>>
            + Send,
    >,
> {
    Box::pin(async move {
        let verb_owned = verb_of(&payload).to_string();
        let verb = verb_owned.as_str();
        if is_kernel_verb(verb) {
            // ROOT ONLY. These verbs act on the TREE and the kernel-wide task
            // table, so the agent named is the OPERAND, not the actor. Answered
            // everywhere, "send create_agent to X" could mean X performs it, X
            // is created under something, or X is the parent — only the last was
            // ever true, and a model asked "which target?" for 16 messages and
            // finished with nothing created.
            //
            // Refused HERE, in the handler — not at the call gate.
            if target.parent_id.is_some() {
                return Ok((crate::refusal::not_here(verb, target.id.as_str()), None));
            }
            // The kernel is the agent of its own verbs: `handle_kernel_verb` parses
            // the call through the shared entry and replies with its refusal.
            return handle_kernel_verb(&kernel, &target, &payload)
                .await
                .map(|v| (v, None));
        }
        // THE TREE ROOT ANSWERS `reflect` WITH THE SUBSTRATE IDENTITY even when
        // it carries a handler_module (the root loader): its persistence verbs
        // (load_tree / persist_record / boot) still route to the bundle below,
        // but `reflect <root>` is the kernel's own identity + tree, matching the
        // `kernel` address. Python `_agent.py`: `if verb == "reflect" and
        // self.parent is None`.
        if verb == "reflect" && target.parent_id.is_none() {
            if let Err(refused) = crate::declared::common::Call::parse(&target.id.0, &payload) {
                return Ok((refused, None));
            }
            return Ok((crate::reflect::reflect_identity(&target), None));
        }
        // No handler_module → only system verbs answerable.
        let Some(hm) = target.handler_module.as_deref() else {
            // Universal answers for bare agents, through the shared entry of the
            // common verbs: their arguments are checked like any agent's.
            if verb == "reflect" {
                if let Err(refused) = crate::declared::common::Call::parse(&target.id.0, &payload) {
                    return Ok((refused, None));
                }
            }
            return Ok((
                match verb {
                    "reflect" => crate::reflect::reflect_identity(&target),
                    // CODED, not a bare error. A reason-less `{error}` is
                    // decorated by `apply_reflect_flags` into something that
                    // reads like a healthy reflect (a tree around it)
                    // with a stray error field — the caller cannot tell a
                    // refusal from an answer.
                    _ => no_handler_module_refusal(&target.id.0, verb),
                },
                None,
            ));
        };
        let Some(bundle) = kernel.bundles.get(hm) else {
            // NOT a create-time refusal: this kernel legitimately holds records
            // whose bundle belongs to ANOTHER runtime (the browser's `*.ts`
            // agents persist here), or is installed later. The call simply
            // cannot be answered HERE, and says so with a code.
            return Ok((not_installed_refusal(&target.id.0, hm, verb), None));
        };
        // The kernel ROUTES: the bundle's own entry (the generated `Parse`, shared
        // library code the bundle cannot skip) checks the verb and its arguments
        // and replies with the refusal.
        // The reply body comes back AT ITS SLOT of the reply: `bytes` for a
        // `read_stream`, a bundle's own key for its own verb, the peer's own key
        // for a bridge relaying a peer's reply.
        match bundle
            .handle_binary(&target.id, payload, body, &kernel)
            .await
        {
            Ok((Some(v), body)) => Ok((v, body)),
            Ok((None, body)) => Ok((Value::Null, body)),
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
            Err(e) => Err(json!({ "error": e.to_string(), "raised": true })),
        }
    })
}

/// A payload or a result AS THE TAPE RECORDS IT — verbatim, except for the ONE
/// reduction.
///
/// XML text cannot carry raw bytes and base64 is banned, so a raw body is
/// written as its LENGTH at the path it rides on — where python's own
/// payload dict holds it, which python's `plain()` writes as `{len: N}` in place.
/// Everything else — all text, all JSON — is untouched. Durable bytes live behind
/// `file_bridge`, the one disk surface; the tape records that N of them passed,
/// not which.
fn taped_payload(value: &Value, body: Option<&crate::outcome::Blob>) -> Value {
    let Some(body) = body else {
        return value.clone();
    };
    let mut out = value.clone();
    // admitted at launch: the path names a slot of the payload
    let _ = body.place(&mut out, json!({"len": body.bytes.len()}));
    out
}

/// How one attempt's handler ended: it answered (or was stopped, or raised),
/// or it deferred and a later settle answered for it ([`Ran::Deferred`]).
type HandlerRun =
    Result<Option<Result<(Value, Option<crate::outcome::Blob>), Value>>, tokio::task::JoinError>;

/// How one attempt's run ended (see [`HandlerRun`] for the handler's own).
enum Ran {
    Handler(HandlerRun),
    Deferred(Outcome),
}

pub(crate) mod deliver;
pub use deliver::{Asker, Deferred};
pub(crate) use deliver::DeferTable;
pub(crate) mod gate;
mod kernel_verbs;
use kernel_verbs::{handle_kernel_verb, is_kernel_verb};

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "send/batch_tests.rs"]
mod batch_tests;

#[cfg(test)]
#[path = "send/ledger_tests.rs"]
mod ledger_tests;

#[cfg(test)]
#[path = "send/delivery_tests.rs"]
mod delivery_tests;
