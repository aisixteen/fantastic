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
///
/// C89: python `ERRORED if isinstance(reply, dict) and reply.get("error")
/// else COMPLETED` (`_agent.py:1051-1052`) — FULL python truthiness, not just
/// bool/string. `error: 0`, `0.0`, `[]`, `{}` and `""` are all falsy and
/// settle COMPLETED; only a truthy `error` value settles ERRORED.
fn error_shaped(v: &Value) -> bool {
    match v.get("error") {
        None => false,
        Some(e) => py_truthy(e),
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

/// `agent <id!r> has no handler_module; cannot answer verb <verb!r>` —
/// SHARED by the text and binary dispatch paths (C34): python has one
/// dispatch path for both channels, so both answer the identical refusal.
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

/// `agent <id!r>: handler_module <hm!r> cannot be imported in this runtime:
/// <ExceptionClass>: <message>` — SHARED by the text and binary dispatch
/// paths (C34/C24). PYTHON'S PREFIX, verbatim; the TAIL is not stable across
/// python's own runs (measured: "No module named 'yaml_state'" one round,
/// "No module named 'yaml_state.tools'" the next), so it names the closest
/// static equivalent of the dynamic import failure rather than pinning
/// python's exact wording.
fn not_installed_refusal(id: &str, hm: &str, verb: &str) -> Value {
    let mut r = crate::refusal::refusal(
        crate::refusal::UNHANDLED,
        format!(
            "agent {}: handler_module {} cannot be imported in this runtime: \
             ModuleNotFoundError: No module named {}",
            fantastic_bundle::py_repr(id),
            fantastic_bundle::py_repr(hm),
            fantastic_bundle::py_repr(hm),
        ),
    );
    crate::refusal::with(&mut r, "verb", json!(verb));
    r
}

/// `<verb>: <arg> must be <want>, got <repr>` — python's fuller `bad()` form
/// that also echoes the received value: `refusal(BAD_ARG, f"...got
/// {v!r}", arg=arg)`. C30/C38's `timeout`/`payload.type`/`retries` refusals
/// need the RAW value in the message; the shared `refusal::bad` omits the
/// `got` clause because most call sites never have it to hand.
fn bad_got(verb: &str, arg: &str, want: &str, got: &Value) -> Value {
    let mut r = crate::refusal::refusal(
        crate::refusal::BAD_ARG,
        format!(
            "{verb}: {arg} must be {want}, got {}",
            fantastic_bundle::py_repr_value(got)
        ),
    );
    crate::refusal::with(&mut r, "arg", json!(arg));
    r
}

/// `timeout` validation (C30/C38): `item.raw_timeout`, when forwarded by a
/// caller relaying an untyped value, SUPERSEDES the typed `item.timeout`.
/// Python: `if timeout is not None and (isinstance(timeout, bool) or not
/// isinstance(timeout, (int, float)) or timeout <= 0): refuse`.
fn validate_timeout(item: &Item) -> Result<Option<f64>, Value> {
    if let Some(raw) = &item.raw_timeout {
        if raw.is_null() {
            return Ok(None);
        }
        let ok = match raw {
            Value::Bool(_) => false,
            Value::Number(n) => n.as_f64().is_some_and(|f| f > 0.0),
            _ => false,
        };
        if !ok {
            return Err(bad_got(
                "send",
                "timeout",
                "a positive number of seconds",
                raw,
            ));
        }
        return Ok(raw.as_f64());
    }
    if let Some(t) = item.timeout {
        if !(t.is_finite() && t > 0.0) {
            return Err(bad_got(
                "send",
                "timeout",
                "a positive number of seconds",
                &json!(t),
            ));
        }
        return Ok(Some(t));
    }
    Ok(None)
}

/// `retries` validation (C30/C38): `item.raw_retries` SUPERSEDES the typed
/// `item.retries` (which is always valid by construction — a `u32` cannot be
/// negative or a bool). Python: `retries = item.get("retries", 0); if
/// isinstance(retries, bool) or not isinstance(retries, int) or retries < 0:
/// refuse`.
fn resolve_retries(item: &Item) -> Result<u32, Value> {
    let Some(raw) = &item.raw_retries else {
        return Ok(item.retries.unwrap_or(0));
    };
    if raw.is_null() {
        return Ok(item.retries.unwrap_or(0));
    }
    match raw {
        Value::Number(n) => match n.as_i64() {
            Some(v) if v >= 0 => Ok(u32::try_from(v).unwrap_or(u32::MAX)),
            _ => Err(bad_got("send", "retries", "a non-negative int", raw)),
        },
        _ => Err(bad_got("send", "retries", "a non-negative int", raw)),
    }
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
    /// C88: the UNNAMED `kernel` reflect fast path — python `_agent.py:846`.
    /// A pure identity read: no ledger entry, no call row, no task-table
    /// row, no watcher fanout. Still async (reflect flags may nest
    /// dispatches), so it rides the settle phase like a real dispatch — just
    /// with none of that bookkeeping, and its outcome's `pid` is `None`.
    KernelReflectFast(Box<FastReflect>),
}

/// The unnamed `kernel` reflect fast path's payload — see [`Launched::
/// KernelReflectFast`].
struct FastReflect {
    target: Arc<Agent>,
    payload: Value,
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
    /// The id of the execution being JOINED (python `d.pid`) — not this
    /// joiner's own dispatch, which never gets one. Needed only to render
    /// the joiner-timeout refusal's `task_id` + pid-in-prose (C31).
    exec_id: i64,
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

    /// `id` validation + resolution (C30/C38): a NAME (non-empty string)
    /// opens-or-joins by name; an int JOINS an existing execution or
    /// refuses; a bool or anything else is `bad_arg`. `item.raw_id`, when a
    /// caller is forwarding a value it did not itself parse, SUPERSEDES the
    /// typed `item.id`.
    ///
    /// Returns `(join_id, name, caller_id)`: `join_id` is `Some` when an
    /// existing execution must be joined (an int naming one, or a name the
    /// ledger already resolves); `name` is `Some` only for the string case
    /// (what a FRESH open binds); `caller_id` is the raw id `Value` itself
    /// (`Value::Null` when the item carried none), kept for the ledger's
    /// rebind text (C31).
    ///
    /// Python: `_agent.py:614-681` (`_launch`'s `exec_id`/`join_id`/`name`
    /// block).
    fn resolve_exec_id(&self, item: &Item) -> Result<(Option<i64>, Option<String>, Value), Value> {
        let raw = item
            .raw_id
            .clone()
            .or_else(|| item.id.clone().map(Value::String));
        let Some(raw) = raw else {
            return Ok((None, None, Value::Null));
        };
        if raw.is_null() {
            return Ok((None, None, Value::Null));
        }
        let bad_shape = || {
            bad_got(
                "send",
                "id",
                "a name (non-empty string) or a message id (int)",
                &raw,
            )
        };
        match &raw {
            Value::Bool(_) => Err(bad_shape()),
            Value::Number(n) => {
                let Some(n) = n.as_i64() else {
                    return Err(bad_shape());
                };
                if !self.ledger.has(n) {
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
                Ok((Some(n), None, raw))
            }
            Value::String(s) if !s.is_empty() => {
                let joined = self.ledger.resolve_name(s);
                Ok((joined, Some(s.clone()), raw))
            }
            Value::String(_) => Err(crate::refusal::bad("send", "id", "a non-empty name")),
            _ => Err(bad_shape()),
        }
    }

    /// One item's SYNCHRONOUS half: validate, resolve, gate. Python
    /// `Agent._launch` (`kernel/_agent.py:601-732`) — C30's check order:
    /// target → payload dict → timeout → payload.type → id → retries →
    /// killed → draining → resolve → gate.
    fn launch(self: &Arc<Self>, item: Item) -> Launched {
        // 1. target — must be a non-empty string.
        if item.target.as_str().is_empty() {
            return Launched::Settled(Outcome::refused(crate::refusal::missing("send", "target")));
        }
        // 2. payload dict.
        if !item.payload.is_object() {
            return Launched::Settled(Outcome::refused(crate::refusal::missing("send", "payload")));
        }
        // 3. timeout.
        let timeout = match validate_timeout(&item) {
            Ok(t) => t,
            Err(r) => return Launched::Settled(Outcome::refused(r)),
        };
        // 4. payload.type — `type` is the verb; there is no send without one.
        // Validated for EVERY send (operator included) BEFORE the gate, so a
        // typeless payload is a coded refusal, never an `unhandled` from the
        // target (measured 2026-09-02).
        let Some(type_v) = item.payload.get("type") else {
            return Launched::Settled(Outcome::refused(crate::refusal::missing(
                "send",
                "payload.type",
            )));
        };
        let verb = match type_v.as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return Launched::Settled(Outcome::refused(bad_got(
                    "send",
                    "payload.type",
                    "a non-empty string",
                    type_v,
                )))
            }
        };
        // 5. id — a NAME (non-empty string) opens-or-joins by name; an int
        // JOINS an existing execution or refuses; anything else is `bad_arg`.
        let (join_id, name, caller_id) = match self.resolve_exec_id(&item) {
            Ok(v) => v,
            Err(r) => return Launched::Settled(Outcome::refused(r)),
        };
        // 6. retries — a non-negative int, never defaulted otherwise.
        let retries = match resolve_retries(&item) {
            Ok(v) => v,
            Err(r) => return Launched::Settled(Outcome::refused(r)),
        };
        // 7. killed — A DYING TASK REGISTERS NO NEW WORK. `cancel_task` marks
        // the whole subtree before it aborts anything, because `on_cancel`
        // hooks await — so a doomed handler gets scheduling turns on its way
        // out, and what it sent there used to start fresh dispatches that
        // outlived the cancel.
        if current_killed() {
            return Launched::Settled(Outcome::new(
                None,
                Status::Cancelled,
                json!({"cancelled": true}),
            ));
        }
        // 8. draining — WHILE DRAINING, NO NEW ROOTS, checked BEFORE the
        // target is even resolved (rust used to resolve first). A send from
        // inside live work (a parent task in the table — including an
        // agent's own `shutdown` turn using its tools) passes; everything
        // else is a new root and is refused BEFORE it costs an id. The
        // root's own control surface stays open: kernel verbs
        // (`shutdown_agents` again, `shutdown_kernel`, `ps`, `messages`,
        // `ledger`) and `reflect` are how an operator watches and finishes
        // the stop. Python `Agent._send` (`kernel/_agent.py`).
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

        // 9. resolve — `"kernel"` aliases to the root, else through
        // `well_known` (C35) then a literal id.
        let Some(target) = self.resolve_target(item.target.as_str()) else {
            return Launched::Settled(Outcome::refused(crate::refusal::unknown_agent(
                item.target.as_str(),
                self.agents.iter().map(|e| e.key().0.clone()).collect(),
            )));
        };

        // 10. ── CALL GATE — fully-gated, two-sided, from-above ──────────
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

        // C88: the UNNAMED `kernel` reflect fast path. python checks the
        // LITERAL alias the caller sent (`target_id == "kernel"`, not the
        // resolved root id — a call spelling the root's real id directly, or
        // any other alias that also resolves to it, does NOT take this
        // path), applied AFTER the gate so it is authorized like any other
        // reflect (`_agent.py:846`). A NAMED or JOINING send (an `id` in the
        // payload) still opens the ledger (`_agent.py:816`) — this fast path
        // is unnamed-only: no caller-supplied id at all.
        if join_id.is_none()
            && name.is_none()
            && item.target.as_str() == "kernel"
            && verb == "reflect"
        {
            return Launched::KernelReflectFast(Box::new(FastReflect {
                target,
                payload: item.payload,
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
        let exec_id = join_id.unwrap_or_else(|| self.rows.mint_id());
        let task_id = TaskId(exec_id);

        match self.ledger.open(
            exec_id,
            &caller_id,
            &target.id,
            &item.payload,
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
            crate::ledger::Opened::Settled(o, orig_payload) => {
                self.tasks.forget(&task_id);
                // AUTHORIZED AGAINST THE EXECUTION IT JOINS. The recorded
                // outcome belongs to whoever ran it, so handing it back is
                // equivalent to having made THAT call — the gate must be asked
                // that question, not about the verb this caller happened to
                // send. Measured in python 2026-08-28: a caller granted only a
                // harmless verb reused another's id and got its secret verbatim.
                if let Some(d) = self.join_denial(&origin, &orig_payload, &target) {
                    return Launched::Settled(Outcome::refused(d));
                }
                return Launched::Settled(*o);
            }
            // IN FLIGHT — join it. A joiner's own `timeout` bounds only ITS
            // wait: it does not own the execution and must not kill it.
            crate::ledger::Opened::Join(rx, orig_payload) => {
                self.tasks.forget(&task_id);
                if let Some(d) = self.join_denial(&origin, &orig_payload, &target) {
                    return Launched::Settled(Outcome::refused(d));
                }
                return Launched::Joining(Box::new(Joiner {
                    rx,
                    timeout,
                    exec_id,
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
            timeout,
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
            Launched::KernelReflectFast(fr) => {
                let FastReflect { target, payload } = *fr;
                let mut reply = crate::reflect::reflect_identity(&target);
                let mut status = Status::Completed;
                if let Err(raised) =
                    crate::reflect::apply_reflect_flags(self, &target, &payload, &mut reply).await
                {
                    reply = raised;
                    status = Status::Failed;
                }
                // pid stays None — no task table, no ledger, no fanout ever
                // touched this item (C88).
                return Outcome::new(None, status, reply);
            }
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
        let (pid, reply, status, body) = loop {
            self.ledger.attempt(exec_id);
            let mut this_pid: Option<TaskId> = None;
            let (mut reply, mut status, body) = self
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
            // Universal post-process, PER ATTEMPT (python applies it inside
            // `_settle_once`, before `_record` weighs a retry): compose every
            // reflect reply with the tree/bundles/readme/more flags. A roster
            // node whose own reflect RAISED fails this whole reflect, exactly as
            // the exception propagating out of python's `_roster` does.
            if verb == "reflect" && status == Status::Completed {
                if let Err(raised) =
                    crate::reflect::apply_reflect_flags(self, &target, &payload, &mut reply).await
                {
                    reply = raised;
                    status = Status::Failed;
                }
            }
            if !self.ledger.should_retry(exec_id, status) {
                break (this_pid, reply, status, body);
            }
            // A RETRY IS THE SAME EXECUTION, so it re-registers under the SAME
            // id — named or not. It used to mint a fresh pid for an unnamed
            // retry, so `cancel <pid>` on attempt two missed the work it was
            // aimed at. Cancelling the id kills the current attempt AND this
            // loop: a cancel is absorbing.
            next_pid = TaskId(exec_id);
        };

        // NO FANOUT HERE. It ran in `run_dispatch`, BEFORE the handler — which is
        // where python puts it and what a watcher needs: mirroring after the
        // reply meant a watcher saw the request only once the work was already
        // done, so its echo landed after the handler's own progress events and
        // the measured order inverted (`>start job_started start`).
        let out = self
            .ledger
            .settle(exec_id, Outcome::new(pid, status, reply).with_body(body));
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
        self.ledger_settled(pid);
        out
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
        let pid = self.rows.mint_id();
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
            "origin".to_string(),
            Value::from(current_sender().map(|o| o.0)),
        );
        row.insert("internal".to_string(), Value::from(true));
        let _ = self.rows.put(&target.id, row);
        let (reply, status, _no_body) = self
            .run_dispatch(
                target,
                payload,
                verb_of(payload),
                None,
                None,
                TaskId(pid),
                None,
            )
            .await;
        (reply, status)
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
                    Err(_) => {
                        // PYTHON'S PROSE, verbatim (C31): the JOINED
                        // execution's id (not this joiner's own dispatch,
                        // which never gets one) — bare digits, python's
                        // `{int!r}`. The execution itself keeps running; only
                        // this caller's wait gave up.
                        let mut r = crate::refusal::refusal(
                            crate::refusal::TIMEOUT,
                            format!(
                                "send: joined execution {} did not settle \
                                 within this caller's {secs}s wait; the \
                                 execution itself keeps running",
                                j.exec_id,
                            ),
                        );
                        crate::refusal::with(&mut r, "task_id", json!(j.exec_id));
                        Outcome::new(None, Status::TimedOut, r)
                    }
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
    ) -> (Value, Status, Option<Vec<u8>>) {
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
                                        // this same row/ledger/timeout, and the
                                        // REPLY BODY comes back with it (C37):
                                        // the batch IS the carrier for bytes, so
                                        // a `read_stream` through `send` returns
                                        // its chunk the way python's reply dict
                                        // holds one.
                                        v = async {
                                            match binary {
                                                Some(b) => {
                                                    dispatch_binary(Arc::clone(&kernel), tgt, payload, b).await
                                                }
                                                None => {
                                                    (dispatch(kernel, tgt, payload).await, None)
                                                }
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
                        // PYTHON'S PROSE, verbatim (C31): target id RAW, verb
                        // `py_repr`-quoted, plus `task_id`.
                        let mut r = crate::refusal::refusal(
                            crate::refusal::TIMEOUT,
                            format!(
                                "send: {} {} exceeded its {secs}s timeout; task cancelled",
                                target.id,
                                fantastic_bundle::py_repr(verb),
                            ),
                        );
                        crate::refusal::with(&mut r, "task_id", json!(task_id.0));
                        return (r, Status::TimedOut, None);
                    }
                }
            }
        };

        match outcome {
            // `raised` marks a terminal handler failure (a bundle `Err`, or a
            // panic below) — retryable, and a dead-letter row when the budget
            // runs out.
            Ok(Some((v, _))) if v.get("raised").and_then(Value::as_bool) == Some(true) => {
                (v, Status::Failed, None)
            }
            // FAILURE IS A STATUS. A handler that RAN and said no — a missing
            // path, a patch that did not match, a refusal it composed itself —
            // settles `errored`, not `completed`. The reply is kept verbatim
            // either way; what changes is that a reader learns the outcome from
            // `status` instead of parsing `result`. Retries are unchanged:
            // `is_retryable` covers the transport classes only, so a semantic
            // no is never re-sent.
            Ok(Some((v, body))) if error_shaped(&v) => (v, Status::Errored, body),
            Ok(Some((v, body))) => (v, Status::Completed, body),
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
    ) -> (Value, Option<Vec<u8>>) {
        // Empty target → `missing_arg` (C30's "also on send_with_binary"),
        // BEFORE resolution.
        if target_id.as_str().is_empty() {
            return (crate::refusal::missing("send", "target"), None);
        }
        // Resolve target: `"kernel"` aliases to the root, else through
        // `well_known` (C35) then a literal id.
        let Some(target) = self.resolve_target(target_id.as_str()) else {
            // The SAME refusal the text channel gives — coded, and carrying the
            // live id index so the caller can see what it could have meant.
            return (
                crate::refusal::unknown_agent(
                    target_id.as_str(),
                    self.agents.iter().map(|e| e.key().0.clone()).collect(),
                ),
                None,
            );
        };

        let verb = verb_of(&header).to_string();

        // Call gate (see `Kernel::send`) — the binary path carries the same
        // agent-originated write_stream/read_stream sends, gated two-sided.
        // Only an operator (`current_sender()` is None) skips it.
        if let Some(orig) = current_sender() {
            if let Some(denial) = self.gate(&orig, &verb, &target, &header) {
                return (denial, None);
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
            dispatch_binary(kernel, target_for_dispatch, header_clone, blob).await
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
///
/// BUILT BY HAND, not by `json!`. The macro routes a `Value` operand through
/// `serde_json::to_value(..).unwrap()`, and that round-trip PANICS on the one
/// number python writes and JSON has no grammar for: `Infinity`/`NaN`, which
/// this kernel stores as an arbitrary-precision literal (C74) and serde's
/// number emitter re-parses. A payload is a caller's, so `{"nan": nan}` on the
/// CLI took the whole process down with `invalid number` (exit 101, measured
/// against the python baseline, which prints the record). Nothing that carries
/// a caller's value may go through `json!`.
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

/// Caller-side guard for one dispatch: on ANY exit path (reply, kill, or the
/// caller's own future being dropped) it stops the dispatch task and drops its
/// row. Stopping a task that already finished is a no-op; forgetting a row that
/// `cancel` already removed is a no-op — both are idempotent by design.
/// Best-effort human text out of a panic payload — the two shapes
/// `panic!` produces plus a fallback, so a handler that panicked with a
/// non-string never reads as "panic: <nothing>". `pub(crate)`: also the
/// state-subscriber isolation in [`crate::kernel`] (C87).
pub(crate) fn panic_message(p: &Box<dyn std::any::Any + Send>) -> String {
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
        // THE TREE ROOT ANSWERS `reflect` WITH THE SUBSTRATE IDENTITY even when
        // it carries a handler_module (the root loader): its persistence verbs
        // (load_tree / persist_record / boot) still route to the bundle below,
        // but `reflect <root>` is the kernel's own identity + tree, matching the
        // `kernel` alias. Python `_agent.py`: `if verb == "reflect" and
        // self.parent is None`.
        if verb == "reflect" && target.parent_id.is_none() {
            return crate::reflect::reflect_identity(&target);
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
                _ => no_handler_module_refusal(&target.id.0, verb),
            };
        };
        let Some(bundle) = kernel.bundles.get(hm) else {
            // NOT a create-time refusal: this kernel legitimately holds records
            // whose bundle belongs to ANOTHER runtime (the browser's `*.ts`
            // agents persist here), or is installed later. The call simply
            // cannot be answered HERE, and says so with a code.
            return not_installed_refusal(&target.id.0, hm, verb);
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
///
/// PYTHON RUNS ONE DISPATCH PATH FOR BOTH CHANNELS — there is no separate
/// binary refusal text in `_agent.py`; a binary send hits the exact same
/// "no handler_module" / "cannot be imported" refusals `dispatch` (above)
/// answers, `verb` key included, and a bundle error settles `raised:true`
/// the same way (C34). This used to carry its own, different wording, so a
/// caller reading `reason`/`error` from a binary refusal saw a shape the text
/// channel never produced.
/// BOXED like [`dispatch`], and for the same reason: this path now reaches the
/// kernel verbs, so `create_agent` → `send` → dispatch closes the recursion and
/// auto-trait inference can no longer prove the future `Send` in terms of itself.
#[allow(clippy::type_complexity)] // the Pin<Box<dyn Future>> IS the point (see above).
fn dispatch_binary(
    kernel: Arc<Kernel>,
    target: Arc<Agent>,
    header: Value,
    blob: Vec<u8>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = (Value, Option<Vec<u8>>)> + Send>> {
    Box::pin(async move {
        let verb = verb_of(&header);
        // PYTHON HAS NO SECOND HANDLER for a payload that carries bytes: the
        // kernel-verb branch, the root-`reflect` branch and the bare-agent answers
        // all sit in `_agent.py`'s ONE `handle`, above the bundle. Missing here, an
        // octet POST of `{"type":"reflect"}` to the ROOT was answered by the loader
        // bundle, so one verb described the disk root on the binary leg and the
        // kernel on the text leg.
        if is_kernel_verb(verb) {
            if target.parent_id.is_some() {
                return (crate::refusal::not_here(verb, target.id.as_str()), None);
            }
            return (
                handle_kernel_verb(&kernel, &target, verb, &header).await,
                None,
            );
        }
        if verb == "reflect" && target.parent_id.is_none() {
            return (crate::reflect::reflect_identity(&target), None);
        }
        let Some(hm) = target.handler_module.as_deref() else {
            return (
                match verb {
                    "boot" | "shutdown" => Value::Null,
                    "reflect" => crate::reflect::reflect_identity(&target),
                    _ => no_handler_module_refusal(&target.id.0, verb),
                },
                None,
            );
        };
        let Some(bundle) = kernel.bundles.get(hm) else {
            return (not_installed_refusal(&target.id.0, hm, verb), None);
        };
        match bundle
            .handle_binary(&target.id, header, blob, &kernel)
            .await
        {
            Ok((Some(v), body)) => (v, body),
            Ok((None, body)) => (Value::Null, body),
            // Settle `raised:true`, same shape as the text channel's bundle error.
            Err(e) => (json!({ "error": e.to_string(), "raised": true }), None),
        }
    })
}

/// Substrate verbs answered natively on every agent — explicitly
/// `list_agents`' pointer back to the briefing — py-canonical `_LIST_HINT`.
const LIST_HINT: &str = "Raw records — how agents are WIRED, not what they are FOR, \
and no manual. <type>reflect</type> on the root is the briefing: each agent's purpose, \
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
