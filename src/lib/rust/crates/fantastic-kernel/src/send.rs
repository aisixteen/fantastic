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

use crate::agent::{Agent, AgentId};
use crate::kernel::Kernel;
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

/// Read a holder's authority grant (`can_call` or `callable_by`) + its tree facts
/// (parent id, direct-child ids) for the gate. An absent agent → empty grant →
/// deny-all (never a hole).
fn grant_facts(holder: Option<&Agent>, field: &str) -> (Value, Option<String>, HashSet<String>) {
    match holder {
        Some(a) => {
            let grant = a
                .meta
                .read()
                .expect("meta lock poisoned")
                .get(field)
                .cloned()
                .unwrap_or(Value::Null);
            let parent = a.parent_id.as_ref().map(|p| p.0.clone());
            let kids: HashSet<String> = a.child_ids().into_iter().map(|c| c.0).collect();
            (grant, parent, kids)
        }
        None => (Value::Null, None, HashSet::new()),
    }
}

impl Kernel {
    /// Send a verb. Resolves target in the flat agents index,
    /// dispatches through system-verb table or the agent's bundle,
    /// returns the reply (`None` for fire-and-forget).
    ///
    /// `CURRENT_SENDER` is set around dispatch. State event + watcher
    /// fanout fire after a successful dispatch.
    pub async fn send(self: &Arc<Self>, target_id: &AgentId, payload: Value) -> Value {
        // Resolve target. Special id `"kernel"` aliases to the root.
        let resolved: Option<Arc<Agent>> = if target_id.as_str() == "kernel" {
            self.root()
        } else {
            self.agents.get(target_id).map(|e| Arc::clone(&e))
        };
        let Some(target) = resolved else {
            return json!({ "error": format!("no agent {target_id}") });
        };

        let sender = current_sender().unwrap_or_else(|| target.id.clone());
        let verb = verb_of(&payload).to_string();

        // ── CALL GATE — fully-gated, two-sided, from-above ──────────────
        // Trust boundary = who ORIGINATED the send. An operator send
        // (`current_sender()` is None: CLI/bootstrap/loader-at-boot/test, or a
        // verb completing its own side effect under `with_operator`) is the origin
        // root of trust → ungated. Every AGENT send — ANY verb, `reflect`
        // included, nothing exempt — needs BOTH the sender's `can_call` (egress)
        // AND the target's `callable_by` (ingress) to grant the edge.
        if let Some(orig) = current_sender() {
            if let Some(denial) = self.gate(&orig, &verb, &target, &payload) {
                return denial;
            }
        }

        // Run the dispatch as a REGISTERED, killable task (one row in the
        // kernel's task table) and await it — that is what makes ANY send
        // cancelable, on any bundle. A cancelled dispatch does NOT raise out of
        // `send`: it resolves to `{"cancelled": true}`.
        let mut reply = self.run_dispatch(&target, &payload, &verb).await;

        // Universal post-process: compose every reflect reply with the
        // tree/bundles/readme flags (mirrors Python's
        // `Agent._apply_reflect_flags`). Bundle reflects and bare-agent
        // reflects alike get the tree (default all) + the optional
        // bundles catalog + the optional readme. Errors / Null replies
        // pass through unchanged.
        if verb == "reflect" {
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

        reply
    }

    /// The two-sided call gate for an agent-originated send. Returns the error
    /// `Value` if the send is refused, else `None`. Checks egress (the sender's
    /// `can_call`) then ingress (the target's `callable_by`), both via the one
    /// [`crate::authz::authorize_call`] engine, then the authority-conferral rules
    /// (no self-raise; delegate only a subset). A holder with no record → empty
    /// grant → deny-all (never a hole).
    fn gate(
        self: &Arc<Self>,
        sender_id: &AgentId,
        verb: &str,
        target: &Arc<Agent>,
        payload: &Value,
    ) -> Option<Value> {
        let loader_id = self.root().map(|r| r.id.0.clone());
        let sender = self.agents.get(sender_id).map(|e| Arc::clone(&e));
        let (s_can, s_parent, s_kids) = grant_facts(sender.as_deref(), "can_call");

        // Egress — may the SENDER call the target?
        let d = crate::authz::authorize_call(
            &s_can,
            verb,
            target.id.as_str(),
            sender_id.as_str(),
            s_parent.as_deref(),
            &s_kids,
            loader_id.as_deref(),
            true,
        );
        if !d.allowed {
            return Some(json!({
                "error": format!("{sender_id} may not call {verb:?} on {}", target.id),
                "reason": "unauthorized", "side": "egress", "hint": d.hint,
            }));
        }

        // Ingress — does the TARGET accept the sender? (tokens relative to target;
        // privileged-no-wildcard is egress-only, so `enforce_privileged=false`.)
        let (t_call, t_parent, t_kids) = grant_facts(Some(target.as_ref()), "callable_by");
        let di = crate::authz::authorize_call(
            &t_call,
            verb,
            sender_id.as_str(),
            target.id.as_str(),
            t_parent.as_deref(),
            &t_kids,
            loader_id.as_deref(),
            false,
        );
        if !di.allowed {
            return Some(json!({
                "error": format!("{} does not accept {verb:?} from {sender_id}", target.id),
                "reason": "unauthorized", "side": "ingress", "hint": di.hint,
            }));
        }

        // Authority conferral — no self-raise, delegate only a subset you hold.
        if verb == "create_agent" || verb == "update_agent" {
            return self.check_conferral(
                sender_id,
                &s_can,
                s_parent.as_deref(),
                &s_kids,
                loader_id.as_deref(),
                payload,
            );
        }
        None
    }

    /// Authority flows from ABOVE only: an agent may not raise its OWN authority
    /// fields, and may confer on another only a SUBSET of its own `can_call`.
    fn check_conferral(
        self: &Arc<Self>,
        sender_id: &AgentId,
        s_can: &Value,
        s_parent: Option<&str>,
        s_kids: &HashSet<String>,
        loader_id: Option<&str>,
        payload: &Value,
    ) -> Option<Value> {
        let obj = payload.as_object()?;
        let confers = obj.contains_key("can_call") || obj.contains_key("callable_by");
        if !confers {
            return None;
        }
        let verb = verb_of(payload);
        let edited = obj.get("id").and_then(Value::as_str);
        // Self-raise: never edit your own authority fields.
        if verb == "update_agent" && edited == Some(sender_id.as_str()) {
            return Some(json!({
                "error": format!("{sender_id} may not edit its own can_call/callable_by"),
                "reason": "unauthorized", "side": "self-grant", "hint": crate::authz::FROM_ABOVE,
            }));
        }
        // Subset: every LATERAL edge in the conferred `can_call` must be one the
        // granter itself holds (structural tokens are free; `callable_by` grants no
        // reach, so it is unbounded).
        if let Some(Value::Object(conferred)) = obj.get("can_call") {
            for (key, verbs) in conferred {
                if key.starts_with('$') {
                    continue; // structural tokens confer no lateral reach
                }
                // Conferring `"*"` (or a list with it) requires the granter to hold
                // `"*"` on that target; a concrete list requires each verb.
                let probe: Vec<&str> = match verbs {
                    Value::Array(vs) if !vs.iter().any(|v| v.as_str() == Some("*")) => {
                        vs.iter().filter_map(Value::as_str).collect()
                    }
                    _ => vec!["*"],
                };
                for v in probe {
                    let ok = crate::authz::authorize_call(
                        s_can,
                        v,
                        key,
                        sender_id.as_str(),
                        s_parent,
                        s_kids,
                        loader_id,
                        true,
                    )
                    .allowed;
                    if !ok {
                        return Some(json!({
                            "error": format!(
                                "{sender_id} cannot delegate {v:?} on {key} — you may only \
                                 confer can_call you already hold"
                            ),
                            "reason": "unauthorized", "side": "delegation",
                            "hint": crate::authz::FROM_ABOVE,
                        }));
                    }
                }
            }
        }
        None
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
    async fn run_dispatch(
        self: &Arc<Self>,
        target: &Arc<Agent>,
        payload: &Value,
        verb: &str,
    ) -> Value {
        // Register BEFORE spawning: the row (and its stop handle) must exist
        // the instant the handler can run, or a nested `send` could claim a
        // parent that isn't in the table yet.
        let task_id = self.tasks.mint(&target.id);
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
            let tid = task_id.clone();
            let cancel = handle.cancel_signal();
            let killed = handle.killed_flag();
            tokio::spawn(async move {
                CURRENT_SENDER
                    .scope(
                        Some(sender),
                        CURRENT_TASK.scope(
                            tid,
                            CURRENT_KILLED.scope(killed, async move {
                                tokio::select! {
                                    biased;
                                    // Stopped: DROP the dispatch future — the
                                    // abort half of `cancel`.
                                    _ = cancel.notified() => None,
                                    v = dispatch(kernel, tgt, payload) => Some(v),
                                }
                            }),
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

        match join.await {
            Ok(Some(v)) => v,
            Ok(None) => {
                if current_killed() {
                    // We are in the killed subtree — park; our own stop signal
                    // lands at this yield and drops us. Never resume.
                    std::future::pending::<()>().await;
                    unreachable!("a killed task is dropped at its next yield");
                }
                json!({ "cancelled": true })
            }
            // The dispatch panicked — propagate, exactly as an inline dispatch
            // would have. (Nothing tokio-aborts a dispatch: `cancel` stops it
            // through the select branch above.)
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => json!({ "cancelled": true }),
        }
    }

    /// Send a binary-framed verb. Mirrors [`Self::send`] except the
    /// payload travels as `(header, blob)` instead of a single JSON
    /// `Value`. Dispatches through the target bundle's
    /// [`crate::bundle::Bundle::handle_binary`] (whose default impl
    /// base64-encodes the blob into `header["data"]` and calls
    /// `handle`).
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
            return (
                json!({ "error": format!("no agent {target_id}") }),
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
    pub async fn watch(&self, src_id: &AgentId, watcher_id: AgentId) {
        if let Some(src) = self.agents.get(src_id) {
            src.watcher_ids.write().await.insert(watcher_id.clone());
        }
        // Auto-vivify the watcher's inbox so fanout has a destination
        // even if `watcher_id` is a synthetic browser-client id.
        if !self.inboxes.contains_key(&watcher_id) {
            let (tx, _rx) = tokio::sync::mpsc::channel(self.inbox_bound);
            self.inboxes.insert(watcher_id, tx);
        }
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
        if is_system_verb(verb) {
            return handle_system_verb(&kernel, &target, verb, &payload).await;
        }
        // No handler_module → only system verbs answerable.
        let Some(hm) = target.handler_module.as_deref() else {
            // Universal answers for bare agents.
            return match verb {
                "boot" | "shutdown" => Value::Null,
                "reflect" => crate::reflect::reflect_identity(&target),
                _ => json!({
                    "error": format!(
                        "agent {:?} has no handler_module; cannot answer verb {:?}",
                        target.id.0, verb
                    ),
                }),
            };
        };
        let Some(bundle) = kernel.bundles.get(hm) else {
            return json!({
                "error": format!("no bundle for handler_module {hm:?}"),
            });
        };
        match bundle.handle(&target.id, &payload, &kernel).await {
            Ok(Some(v)) => v,
            Ok(None) => Value::Null,
            Err(e) => json!({ "error": e.to_string() }),
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

/// excludes `reflect`. Reflect is per-bundle (each handler customizes
/// its `sentence` + state shape); the substrate's reflect path only
/// fires for bare agents (no `handler_module`), which is handled
/// inside `dispatch`.
fn is_system_verb(verb: &str) -> bool {
    matches!(
        verb,
        "create_agent"
            | "delete_agent"
            | "update_agent"
            | "list_agents"
            | "get"
            // The TASK TABLE — work in flight, universal. Baked into the
            // substrate, in no bundle's verb map: cancellation is a property of
            // the one primitive (`send`), so EVERY agent is cancelable and a
            // caller never needs to know which bundle it faces. Plain verbs — an
            // AGENT can call them on another agent's work.
            | "ps"
            | "cancel"
            // Root-only control verb — gated inside `handle_system_verb`
            // to the tree root; a non-root target answers with an error.
            | "shutdown_kernel"
    )
}

/// One line per system verb — the honest reflect surface. Bundle verbs are
/// derived from each bundle's own reflect; these are the SUBSTRATE's own, so
/// they appear in no bundle's map and are advertised on the ROOT reflect (see
/// [`crate::reflect::apply_reflect_flags`]). Mirrors Python's
/// `_SYSTEM_VERB_DOCS`.
pub fn system_verb_docs() -> Value {
    json!({
        "create_agent": "args: handler_module:str, id:str?, **meta — spawn a child agent + boot it.",
        "update_agent": "args: id:str, **meta — patch a record's meta.",
        "delete_agent": "args: id:str — cascade-delete an agent + its subtree (delete_lock refuses).",
        "list_agents": "No args. Every agent's record, flat.",
        "get": "args: id:str — one agent's record (null if unknown).",
        "ps": "No args. The live TASK TABLE — {tasks:[{id, agent, parent, verb, started_at}]}. One row per in-flight dispatch; `parent` chains them into the send tree.",
        "cancel": "args: task_id:str — kill that task AND ITS SUBTREE (children first). Returns {cancelled:bool, killed:[task_id]}; the blocked caller's `send` resolves to {cancelled:true}. Unknown/finished id ⇒ {cancelled:false}.",
        "shutdown_kernel": "No args. ROOT ONLY — stop the kernel process gracefully.",
    })
}

async fn handle_system_verb(
    kernel: &Arc<Kernel>,
    target: &Arc<Agent>,
    verb: &str,
    payload: &Value,
) -> Value {
    match verb {
        "list_agents" => {
            let mut out: Vec<Value> = Vec::new();
            for entry in kernel.agents.iter() {
                let a = entry.value();
                out.push(serde_json::to_value(a.record()).unwrap_or(Value::Null));
            }
            // Sort by id for stable output.
            out.sort_by(|a, b| {
                a.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .cmp(b.get("id").and_then(Value::as_str).unwrap_or(""))
            });
            // `more` is not decoration (py-canonical, measured 2026-08-07): a model
            // that opened with `list_agents` got bare records, never learned that
            // authoring a script agent was the mechanism, and made 0 attempts; the
            // arm that opened with `reflect` got `self_extension` and tried 10
            // times. This verb rides every reflect's `system_verbs`, so it needs a
            // signpost back to the briefing.
            json!({ "agents": out, "more": LIST_HINT })
        }
        "create_agent" => crate::lifecycle::create_from_payload(kernel, target, payload).await,
        "delete_agent" => crate::lifecycle::delete_from_payload(kernel, target, payload).await,
        "update_agent" => update_from_payload(kernel, target, payload).await,
        // The live TASK TABLE, kernel-wide (like `list_agents`), not per-agent:
        // one row per in-flight dispatch, `parent` chaining them into the send
        // tree. `ps` shows its own row too — it IS a dispatch. The table is honest.
        "ps" => json!({ "tasks": kernel.tasks.rows() }),
        // Kill a task AND ITS SUBTREE (children first, like killing a process
        // group). Each bundle's `on_cancel` hook runs BEFORE the abort so real
        // resources (OS subprocesses, PTYs, sockets) die with it. An unknown or
        // already-finished id returns {cancelled:false} — it does not pretend.
        "cancel" => match payload.get("task_id").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => kernel.cancel_task(&TaskId::from(t)).await,
            _ => json!({ "error": "cancel: task_id (str) required" }),
        },
        "shutdown_kernel" => {
            // Privileged: root control surface only. Gate on rootness
            // (parent_id is None) so it holds whether the verb was
            // addressed to the literal root id or via the `kernel` alias —
            // never an arbitrary child agent.
            if target.parent_id.is_some() {
                return json!({
                    "error": "shutdown_kernel: root control surface only; \
                              address the kernel root (alias 'kernel')"
                });
            }
            // DEFER the shutdown signal so this ack is enqueued onto the WS
            // out-channel / written as the HTTP body and flushed to the
            // caller BEFORE the daemon serve loop begins teardown — mirrors
            // the Python side's `call_later(0.1, ev.set)`. The verb returns
            // the ack synchronously here (running inside a web task); ~100ms
            // later `request_shutdown()` fires and the CLI serve loop (which
            // adds its own short grace) drains agents, releases lock.json,
            // and exits 0. Never exit inline / signal before the ack is even
            // enqueued — that would risk truncating the ack mid-flush. Wire
            // shape mirrors Python exactly (`type` before `ok`).
            let k = Arc::clone(kernel);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                k.request_shutdown();
            });
            json!({ "type": "shutdown_kernel", "ok": true })
        }
        "get" => {
            let id = payload.get("id").and_then(Value::as_str).map(AgentId::from);
            match id.and_then(|i| kernel.agents.get(&i).map(|e| Arc::clone(&e))) {
                Some(a) => serde_json::to_value(a.record()).unwrap_or(Value::Null),
                None => Value::Null,
            }
        }
        _ => unreachable!("is_system_verb gate"),
    }
}

async fn update_from_payload(kernel: &Arc<Kernel>, caller: &Arc<Agent>, payload: &Value) -> Value {
    let id = match payload.get("id").and_then(Value::as_str) {
        Some(s) => AgentId::from(s),
        None => return json!({ "error": "update_agent requires id" }),
    };
    let Some(target) = kernel.agents.get(&id).map(|e| Arc::clone(&e)) else {
        return json!({ "error": format!("no agent {id}") });
    };
    // Patch is every field in payload except `type` and `id`. Capture
    // changed-field names so the lifecycle event mirrors Python's
    // `agent_updated.changed`.
    let mut patch: Map<String, Value> = Map::new();
    let mut changed: Vec<String> = Vec::new();
    if let Some(obj) = payload.as_object() {
        for (k, v) in obj {
            if k == "type" || k == "id" {
                continue;
            }
            patch.insert(k.clone(), v.clone());
            changed.push(k.clone());
        }
    }
    // WRITE-TIME VALIDATION. Ask the target's bundle whether it can honour this
    // patch before anything changes — a refusal names the bad field at the point of
    // the mistake instead of surfacing calls later. Mirrors python's duck-typed
    // `validate_meta`; the trait default accepts everything.
    if let Some(hm) = target.handler_module.as_deref() {
        if let Some(bundle) = kernel.bundles.get(hm) {
            let current = target.meta.read().expect("meta poisoned").clone();
            if let Some(why) = bundle.validate_meta(&patch, &current) {
                return json!({ "error": why, "id": id.to_string(), "updated": false });
            }
        }
    }
    let rec = target.update_meta(patch);
    // Persist the updated record THROUGH the discovered provider. Merge-only —
    // the existing agent.json's other fields (bundle-specific, user additions)
    // survive. No-op in InMemory mode or when no provider is wired (RAM). See
    // `persistence::persist` for the dirty-binding semantics.
    let _ = crate::persistence::persist(kernel, &target).await;
    let rec_value = serde_json::to_value(&rec).unwrap_or(Value::Null);
    // Telemetry state event — Python parity (`_agent.py:637-657`):
    // publish the full record + the list of changed fields, not just
    // `{type, id}`. State subscribers (telemetry pane, AI test
    // harnesses) lean on this for diff'ing agent state across events.
    let event = json!({
        "type": "updated",
        "id": rec.id,
        "changed": changed.clone(),
        "agent": rec_value.clone(),
    });
    kernel.publish_state(&event);
    // Emit `agent_updated` on the caller's inbox so watchers (canvas
    // frame chrome, telemetry panels) refresh without polling. Mirrors
    // Python's `await self.emit(self.id, {type:"agent_updated", ...})`.
    kernel
        .emit(
            &caller.id,
            json!({
                "type": "agent_updated",
                "id": rec.id,
                "changed": changed,
                "agent": rec_value.clone(),
            }),
        )
        .await;
    rec_value
}

#[cfg(test)]
mod tests;
