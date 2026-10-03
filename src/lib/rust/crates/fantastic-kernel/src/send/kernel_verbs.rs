//! The KERNEL VERBS — the substrate's own surface, answered by the ROOT alone.
//!
//! They act on the TREE and the kernel-wide tables, so the agent a caller names
//! is the OPERAND, not the actor. That is why they live behind one address
//! (`kernel`) and why every reflect advertises them: answered everywhere, "send
//! create_agent to X" read three ways and only one was ever true.

use super::*;

/// excludes `reflect`. Reflect is per-bundle (each handler customizes
/// its `sentence` + state shape); the substrate's reflect path only
/// fires for bare agents (no `handler_module`), which is handled
/// inside `dispatch`.
pub(super) fn is_kernel_verb(verb: &str) -> bool {
    matches!(
        verb,
        "create_agent"
            | "delete_agent"
            | "update_agent"
            | "list_agents"
            // The TASK TABLE — work in flight, universal. Baked into the
            // substrate, in no bundle's verb map: cancellation is a property of
            // the one primitive (`send`), so EVERY agent is cancelable and a
            // caller never needs to know which bundle it faces. Plain verbs — an
            // AGENT can call them on another agent's work.
            | "ps"
            | "cancel"
            // The EXECUTION LEDGER's read surface — every execution, live and
            // settled. Terminal non-completed rows ARE the dead-letter queue:
            // parked, inspectable, never silently retried; `ack_deadletter`
            // retires one. (NOT `ack`: kernel verbs shadow bundle verbs, and
            // exec_bridge owns `ack` for a PTY job's flow control.)
            | "ledger"
            | "ack_deadletter"
            // The kernel's LIFECYCLE. `kernel_start` is the one start sequence
            // (the root's memory store first, then everyone); `kernel_stop` the
            // graceful stop of the process. `shutdown_agents` stops the agents,
            // not the process. Root-only — gated inside `handle_kernel_verb`.
            | "kernel_start"
            | "shutdown_agents"
            | "kernel_stop"
    )
}

/// One line per KERNEL verb — the honest reflect surface.
///
/// These are the SUBSTRATE's own verbs, answered by the ROOT alone, so they
/// appear in no bundle's map. Mirrors python's `_SYSTEM_VERB_DOCS`.
///
/// THE CONSEQUENCE MUST PRECEDE THE FIRST EM-DASH. A default reflect keeps only
/// the text BEFORE it (see [`crate::reflect::apply_reflect_flags`]), so anything
/// after the dash reaches `detail:"full"` callers only — and these are the facts
/// a caller needs BEFORE firing the verb, not after. Measured 2026-08-24:
/// `create_agent` read as `args: handler_module:str, id:str?, **meta` with
/// "spawn a child agent" cut, and a model spent 16 messages unable to work out what
/// the target meant, finishing with `agents_created: []`.
pub fn kernel_verb_docs() -> Value {
    // PYTHON DECLARATION ORDER (`_SYSTEM_VERB_DOCS`): `detail:"full"` renders the
    // map in this order, only the distilled view sorts.
    json!({
        "create_agent": "args: handler_module:str, parent_id:str REQUIRED, id:str?, **meta. \
    Creates a CHILD OF `parent_id` — your own id grows your own subtree \
    (no grant needed); 'kernel_state' for top-level. No default parent.",
        "update_agent": "args: id:str, **meta. EVERY FIELD YOU NAME IS REPLACED, NOT MERGED \
    (a field you do not name is untouched), so a dict field must be sent \
    WHOLE — `can_call`/`callable_by` included: restate every key you want \
    to keep, the baseline `$self`/`$parent`/`$children` among them, or the \
    keys you omit are GONE. Granting one edge reads like a PATCH and \
    behaves like a PUT.",
        "delete_agent": "args: id:str. CASCADES — deletes that agent AND ITS WHOLE SUBTREE, \
    deepest first; refuses if any descendant carries `delete_lock`.",
        "list_agents": "No args. Every agent's record, flat. Paged.",
        "ps": "No args. The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, \
    verb, started_at}]}, one row per in-flight dispatch. Paged.",
        // EVERY execution, not just the named ones: the doc once said NAMED-only
        // while the reply carried every minted-pid row beside them.
        "ledger": "No args. EVERY execution and what it became — named under its id, \
    unnamed under its minted pid — {executions:[{id, target, status, attempts, \
    retries, named, outcome?, acked?}]}, paged, scoped to what YOU originated; read \
    from the kernel's memory store plus what is not yet flushed. `outcome` keeps the \
    RESULT only for a named call; a dead letter keeps its error; anything else is \
    {status, bytes} — the reply itself went to its caller. Failed/timed_out/cancelled \
    rows are the dead-letter queue (announced by the `on_deadletter` event): inspect, \
    retry with a NEW id if you choose, then `ack_deadletter` it — nothing is retried \
    silently.",
        "ack_deadletter": "args: id:int|str (req — an execution id, or its name). \
    Acknowledges a DEAD LETTER (failed/timed_out/cancelled) you originated: recorded \
    in the kernel's store, it may then be pruned. Returns {acked:true, id}.",
        "kernel_start": "No args. THE START SEQUENCE — the root must name \
    `memory_store_id` (fail loud otherwise): boots the root, then that store, starts \
    flushing the kernel's execution rows into it, then boots every other agent in \
    registration order. Idempotent. Returns {started, store, booted:[ids]}.",
        "cancel": "args: task_id:str. Kills that task AND ITS SUBTREE — the id names its \
    own agent. Returns {cancelled:bool, killed:[…]}; the blocked caller's \
    batch item settles as status:'cancelled'. Unknown id ⇒ {cancelled:false}.",
        // No em-dash, so the distilled line is the whole doc.
        "shutdown_agents": "No args. Stops the AGENTS, not the process: drains (new \
    roots refused), sends `shutdown` to every agent advertising it, waits for \
    in-flight work, cancels the rest. Returns {drained, shut:[ids], cancelled:[ids]}.",
        "kernel_stop": "No args. Stops the kernel PROCESS gracefully — drain, every call \
    settles (done, or cancelled into the dead letters), the last rows flush, agents \
    shut down, exit. Under env:'container', the container.",
    })
}

/// The `_` line that heads `kernel_verbs` — states WHERE to send them and what
/// the named id means. Without it "send create_agent to X" reads three ways (X
/// performs it / X is created / X is the parent) and only the last is true.
pub const KERNEL_VERBS_PREAMBLE: &str =
    "Kernel-owned: send to 'kernel'. The id you name is the OPERAND, \
not the actor. (`verbs` is what this agent itself does.)";

/// `kernel_verbs` as a reflect renders it. `full` keeps each doc whole, in
/// python's declaration order; the default truncates at the first em-dash and
/// sorts by name.
///
/// Read-only verbs used to render as `""` (name only), on the ground that their
/// reply teaches the rest — but a name with no signature is exactly what left
/// `ps` and `list_agents` unusable without a `full` reflect, and each is one line.
pub fn kernel_verbs_view(full: bool) -> Value {
    let mut out = Map::new();
    out.insert("_".to_string(), json!(KERNEL_VERBS_PREAMBLE));
    let docs = kernel_verb_docs();
    let obj = docs.as_object().expect("kernel_verb_docs is an object");
    let mut names: Vec<&String> = obj.keys().collect();
    if !full {
        names.sort();
    }
    for name in names {
        let doc = obj[name].as_str().unwrap_or("");
        let text = if full {
            doc.to_string()
        } else {
            doc.split('—').next().unwrap_or(doc).trim().to_string()
        };
        out.insert(name.clone(), Value::String(text));
    }
    Value::Object(out)
}

/// `Err` is a python RAISE the verb emulates (a store read that failed, an
/// `int()` that threw) — settled `failed`, out of band; `Ok` is the reply.
pub(super) async fn handle_kernel_verb(
    kernel: &Arc<Kernel>,
    target: &Arc<Agent>,
    verb: &str,
    payload: &Value,
) -> Result<Value, Value> {
    Ok(match verb {
        "list_agents" => {
            // REGISTRATION ORDER — python iterates `self.ctx.agents`, a plain
            // dict, and the root comes first because it registered first. The
            // port sorted by id, which put every alphabetically-earlier agent
            // ahead of the root.
            let mut out: Vec<Value> = Vec::new();
            for id in kernel.agent_ids() {
                if let Some(a) = kernel.agents.get(&id) {
                    out.push(crate::jsonsafe::record_value(&a.record()));
                }
            }
            // PAGED. Absent `limit` ⇒ everything and `next: null`, so a caller
            // that never learned about paging is unaffected; the cursor is the
            // last id handed over, never an offset.
            let (out, next) = crate::paging::page(out, payload, |r| {
                r.get("id")
                    .map_or_else(String::new, fantastic_bundle::py_str)
            });
            // `more` is not decoration (py-canonical, measured 2026-08-07): a model
            // that opened with `list_agents` got bare records, never learned that
            // authoring a script agent was the mechanism, and made 0 attempts; the
            // arm that opened with `reflect` got `self_extension` and tried 10
            // times. This verb rides every reflect's `system_verbs`, so it needs a
            // signpost back to the briefing.
            json!({ "agents": out, "next": next, "more": LIST_HINT })
        }
        "create_agent" => crate::lifecycle::create_from_payload(kernel, target, payload).await,
        "delete_agent" => crate::lifecycle::delete_from_payload(kernel, target, payload).await,
        "update_agent" => update_from_payload(kernel, target, payload).await,
        // The EXECUTION LEDGER — a PROJECTION, not a table: the executions still
        // in RAM (in flight, or settled and not yet flushed) merged with the
        // `call`/`settle`/`ack` rows the kernel flushed into its memory store —
        // each call paired with its settle. CALLER-SCOPED: an agent sees what IT
        // originated, the operator sees all. Unscoped this would be a side-channel
        // around `callable_by`.
        "ledger" => {
            let viewer = crate::send::current_origin();
            let entries = match kernel.ledger_entries().await {
                Ok(e) => e,
                Err(e) => return Err(crate::executions::raised("RuntimeError", &e)),
            };
            let rows: Vec<Value> = entries
                .into_iter()
                .filter_map(|mut e| {
                    let caller = e.shift_remove("_caller").unwrap_or(Value::Null);
                    match &viewer {
                        None => Some(Value::Object(e)),
                        Some(v) if caller.as_str() == Some(v.0.as_str()) => Some(Value::Object(e)),
                        Some(_) => None,
                    }
                })
                .collect();
            let (rows, next) = crate::paging::page(rows, payload, |r| {
                r.get("id")
                    .map_or_else(String::new, fantastic_bundle::py_str)
            });
            json!({ "executions": rows, "next": next })
        }
        "ack_deadletter" => ack_deadletter(kernel, payload).await?,
        // The live TASK TABLE, kernel-wide (like `list_agents`), not per-agent:
        // one row per in-flight dispatch, `parent` chaining them into the send
        // tree. `ps` shows its own row too — it IS a dispatch. The table is honest.
        "ps" => {
            // PAGED like `list_agents` — same cursor law: the id of the last row
            // handed over. Over a LIVE table a cursor may miss a row that
            // finished mid-walk; this is a kernel, not a snapshot store.
            let (rows, next) = crate::paging::page(kernel.tasks.rows(), payload, |r| {
                r.get("id")
                    .map_or_else(String::new, fantastic_bundle::py_str)
            });
            json!({ "tasks": rows, "next": next })
        }
        // Kill a task AND ITS SUBTREE (children first, like killing a process
        // group). Each bundle's `on_cancel` hook runs BEFORE the abort so real
        // resources (OS subprocesses, PTYs, sockets) die with it. An unknown or
        // already-finished id returns {cancelled:false} — it does not pretend.
        // A pid is a NUMBER now, so `task_id` may arrive as one, as the decimal
        // string a URL or a form carries, or as the NAME a caller gave its own
        // execution — the ledger resolves that last case. An id that resolves to
        // nothing is an honest `{cancelled:false}`, not a refusal: python does
        // not pretend to know whether a stranger's id ever existed.
        // PYTHON, VERBATIM (C18): `raw = payload.get("task_id")`; None / "" /
        // a bool / anything not (int, str) → `missing_arg`. Otherwise
        // `task_id = _as_id(raw)`; if that names nothing AND raw is a
        // string, the caller's NAME for its own execution is its handle too
        // (`resolve_name`). Still nothing → an honest `{cancelled:false}`,
        // never a refusal — python does not pretend to know whether a
        // stranger's id ever existed. Used to accept `"+5"`/padded strings
        // via a bare `str::parse::<i64>()` fallback, which python's
        // `str.isdigit()` gate refuses (they fall through to the name path
        // instead, exactly like any other non-digit string).
        "cancel" => {
            // `not isinstance(raw, (int, str))`: a FLOAT (3.5, and 3.0 too) is
            // neither, so it is refused, not answered {cancelled:false}.
            let raw = payload.get("task_id").filter(|v| !v.is_null());
            let usable = matches!(raw, Some(n @ Value::Number(_)) if fantastic_bundle::py_type_name(n) == "int")
                || matches!(raw, Some(Value::String(s)) if !s.is_empty());
            if !usable {
                return Ok(crate::refusal::missing("cancel", "task_id"));
            }
            let raw = raw.expect("checked usable above");
            let task_id = as_id(raw)?;
            let resolved = match task_id {
                Some(n) => Some(n),
                None => raw.as_str().and_then(|s| kernel.ledger.resolve_name(s)),
            };
            match resolved {
                Some(n) => kernel.cancel_task(&TaskId(n)).await,
                None => json!({"cancelled": false, "killed": []}),
            }
        }
        // THE START SEQUENCE — see `Kernel::start`. Idempotent.
        "kernel_start" => kernel.start(true).await,
        "shutdown_agents" => {
            // Privileged: root control surface only, gated on rootness like
            // `kernel_stop` so it holds whether the verb was addressed to
            // the literal root id or via the reserved `kernel` address.
            if target.parent_id.is_some() {
                return Ok(json!({
                    "error": "shutdown_agents: root control surface only; \
                              address the kernel root ('kernel')"
                }));
            }
            kernel.shutdown_agents().await
        }
        "kernel_stop" => {
            // Privileged: root control surface only. Gate on rootness
            // (parent_id is None) so it holds whether the verb was
            // addressed to the literal root id or via the reserved `kernel` address —
            // never an arbitrary child agent.
            if target.parent_id.is_some() {
                return Ok(json!({
                    "error": "kernel_stop: root control surface only; \
                              address the kernel root ('kernel')"
                }));
            }
            // Order: (1) ack and let it flush; (2) AFTER the reply is on the wire,
            // trigger the daemon's graceful path — drain, every call settles
            // (done, or cancelled into the dead letters), the last execution rows
            // flush into the kernel's store, agents shut down, the lock releases —
            // (3) exit 0. DEFERRED ~100ms (python `call_later(0.1, ev.set)`) so
            // the in-flight REST body / WS frame is written before the listeners
            // drain. With no serve loop (one-shot) there is nothing to stop — it
            // still acks.
            let k = Arc::clone(kernel);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                k.request_shutdown();
            });
            json!({ "type": "kernel_stop", "ok": true })
        }
        _ => unreachable!("is_kernel_verb gate"),
    })
}

async fn update_from_payload(kernel: &Arc<Kernel>, caller: &Arc<Agent>, payload: &Value) -> Value {
    let id = match payload
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(s) => AgentId::from(s),
        None => return crate::refusal::missing("update_agent", "id"),
    };
    // REFUSED, not silently skipped, and checked BEFORE the id is even
    // resolved to a live agent — python's `for k in ("handler_module",
    // *sorted(RESERVED_RECORD_KEYS)):`. `handler_module` is fixed at create —
    // swapping it would leave a live agent whose in-process state belongs to
    // the OLD bundle. The reserved keys name substrate wiring; they used to
    // be dropped without a word, so the caller believed a change had landed
    // that never did (2026-09-02).
    let mut checks: Vec<&str> = vec!["handler_module"];
    let mut reserved: Vec<&str> = RESERVED_RECORD_KEYS.to_vec();
    reserved.sort_unstable();
    checks.extend(reserved);
    for k in checks {
        if payload.get(k).is_some() {
            return crate::refusal::bad(
                "update_agent",
                k,
                "absent — handler_module is fixed at create_agent, and \
                 root_path/ctx/parent name substrate wiring the record may \
                 not carry",
            );
        }
    }
    // `children_dir` BECOMES A DIRECTORY — one safe segment, as at create.
    if let Some(cd) = payload.get("children_dir") {
        if let Some(r) = crate::refusal::guard_agent_id_as(
            "update_agent",
            "children_dir",
            cd.as_str().unwrap_or(""),
        ) {
            return r;
        }
    }
    // Patch is every field in payload except `type`, `id` and `parent_id`.
    // `parent_id` is STRUCTURAL — where the agent actually sits, which only
    // the tree knows. Letting it into meta lets a record LIE about its own
    // parent: `$parent` then resolves differently on the two halves of the
    // gate, and a reboot silently corrects it from the agent's position on
    // disk. Capture changed-field names so the lifecycle event mirrors
    // Python's `agent_updated.changed`.
    let mut patch: Map<String, Value> = Map::new();
    let mut changed: Vec<String> = Vec::new();
    if let Some(obj) = payload.as_object() {
        for (k, v) in obj {
            if k == "type" || k == "id" || k == "parent_id" {
                continue;
            }
            patch.insert(k.clone(), v.clone());
            changed.push(k.clone());
        }
    }
    let Some(target) = kernel.agents.get(&id).map(|e| Arc::clone(&e)) else {
        return crate::refusal::unknown_agent(
            id.as_str(),
            kernel.agents.iter().map(|e| e.key().to_string()).collect(),
        );
    };
    // Grant-map SHAPE, judged here at write time — the gate only judges edges
    // (py-canonical, 2026-09-01: `can_call:"*"` accepted as a bare string left
    // a silently-dead grant; refuse loud at the point of the mistake).
    // `holds_secrets` is the exception that IS gated: only `true` or `null` is
    // written (`bad_holder_mark`), and on a holder only the operator writes
    // `null` — its clear (`check_holder_mark`, in the gate).
    if let Some(why) = crate::lifecycle::bad_grant_shape(&patch)
        .or_else(|| crate::lifecycle::bad_holder_mark(&patch))
    {
        let mut r = crate::refusal::refusal(crate::refusal::INVALID_META, &why);
        crate::refusal::with(&mut r, "id", json!(id));
        return r;
    }
    // WRITE-TIME VALIDATION. Ask the target's bundle whether it can honour this
    // patch before anything changes — a refusal names the bad field at the point of
    // the mistake instead of surfacing calls later. Mirrors python's duck-typed
    // `validate_meta`; the trait default accepts everything.
    if let Some(hm) = target.handler_module.as_deref() {
        if let Some(bundle) = kernel.bundles.get(hm) {
            let current = target.meta.read().expect("meta poisoned").clone();
            if let Some(why) = bundle.validate_meta(&patch, &current) {
                let mut r = crate::refusal::refusal(crate::refusal::INVALID_META, why);
                crate::refusal::with(&mut r, "id", json!(id));
                return r;
            }
        }
    }
    let rec = target.update_meta(patch);
    // NO INLINE PERSIST: the `updated` event published below is what the root
    // loader's `persist_writer` turns into the merge-write.
    let rec_value = crate::jsonsafe::record_value(&rec);
    // Telemetry state event, python's shape: `{agent_id, kind:"updated",
    // changed}` — the CHANGED KEYS, nothing more. The full record rode along as
    // `agent` in the port; a state event is a notification that something moved,
    // and a subscriber that wants the record reads it.
    let event = json!({
        "agent_id": rec.id,
        "kind": "updated",
        "changed": changed.clone(),
    });
    kernel.publish_state(&event);
    // Emit `agent_updated` on the caller's inbox so watchers (canvas
    // frame chrome, telemetry panels) refresh without polling. Mirrors
    // Python's `await self.emit(self.id, {type:"agent_updated", ...})`.
    kernel
        .emit(
            &caller.id,
            crate::jsonsafe::obj([
                ("type", Value::from("agent_updated")),
                ("id", Value::from(rec.id.clone())),
                ("changed", Value::from(changed)),
                ("agent", rec_value.clone()),
            ]),
        )
        .await;
    // PYTHON'S REPLY SHAPE. The bare record left a caller that branches on
    // `updated` — the field the kernel_verbs doc teaches — reading `undefined`
    // on this runtime for the call that succeeded, so a model read success as
    // failure and retried.
    crate::jsonsafe::obj([
        ("updated", Value::from(true)),
        ("id", Value::from(rec.id)),
        ("agent", rec_value),
    ])
}

/// A message id from whatever the caller wrote: a POSITIVE int, or the
/// decimal string a URL or a form carries. A bool is not an id (C18).
///
/// PYTHON'S `_as_id`, verbatim:
/// ```python
/// if isinstance(x, bool): return None
/// if isinstance(x, int): return x if x > 0 else None
/// if isinstance(x, str) and x.isdigit(): return int(x)
/// return None
/// ```
/// A non-`isdigit` string is NOT a fallback-parsed id — it is simply not one
/// (0, negatives, whitespace-padded and `+5` all fail `isdigit` and answer
/// `None`, never a best-effort `str::parse`).
///
/// `Err` is a RAISE, not a refusal: python's `_as_id` gates on `str.isdigit()`
/// and then calls `int(x)`, and the two disagree — `'²'` is a digit CPython's
/// base-10 parser rejects, so the verb dies with a `ValueError` instead of
/// answering. The whole call is reported as a raised handler, as python's
/// dispatch reports any exception out of a verb body.
fn as_id(v: &Value) -> Result<Option<i64>, Value> {
    match v {
        Value::Bool(_) => Ok(None),
        Value::Number(n) => Ok(n.as_i64().filter(|i| *i > 0)),
        // `isdecimal`, not `isdigit`: a superscript digit passed the guard and
        // then raised inside `int()` (python fixed 2026-09-20).
        Value::String(s) if fantastic_bundle::py_isdecimal(s) => {
            match fantastic_bundle::py_int(v) {
                Ok(n) => Ok(Some(n)),
                Err(e) => Err(raised(e.class_name(), e.message())),
            }
        }
        _ => Ok(None),
    }
}

/// A python exception the way a raised verb body is recorded: `{"error":
/// "<Class>: <message>", "raised": true}` — the shape python's dispatch builds
/// from `type(e).__name__` and `str(e)`, which settles the item `failed`.
fn raised(class: &str, message: &str) -> Value {
    json!({"error": format!("{class}: {message}"), "raised": true})
}

/// `ack_deadletter` — acknowledge a DEAD LETTER the caller originated (the
/// operator: any). The ack is a row in the kernel's store (`kind: ack`); an acked
/// dead letter may be pruned. Python `_verb_ack_deadletter`.
async fn ack_deadletter(kernel: &Arc<Kernel>, payload: &Value) -> Result<Value, Value> {
    let raw = payload.get("id").cloned().unwrap_or(Value::Null);
    let usable = match &raw {
        Value::Null | Value::Bool(_) => false,
        Value::String(s) => !s.is_empty(),
        _ => true,
    };
    if !usable {
        return Ok(crate::refusal::missing("ack_deadletter", "id"));
    }
    let viewer = crate::send::current_origin();
    let entries = kernel
        .ledger_entries()
        .await
        .map_err(|e| crate::executions::raised("RuntimeError", &e))?;
    let eid = as_id(&raw)?;
    let hit = entries.into_iter().find(|e| match eid {
        Some(n) => e.get("id").and_then(Value::as_i64) == Some(n),
        None => e.get("name") == Some(&raw),
    });
    let mine = hit.filter(|e| match &viewer {
        None => true,
        Some(v) => e.get("_caller").and_then(Value::as_str) == Some(v.0.as_str()),
    });
    let Some(hit) = mine else {
        let mut r = crate::refusal::refusal(
            crate::refusal::NOT_FOUND,
            format!(
                "ack_deadletter: no execution {} of yours in the ledger",
                fantastic_bundle::py_repr_value(&raw)
            ),
        );
        crate::refusal::with(&mut r, "arg", json!("id"));
        return Ok(r);
    };
    let hid = hit.get("id").and_then(Value::as_i64).unwrap_or(0);
    let status = hit.get("status").cloned().unwrap_or(Value::Null);
    if !matches!(status.as_str(), Some("failed" | "timed_out" | "cancelled")) {
        let mut r = crate::refusal::refusal(
            crate::refusal::BAD_ARG,
            format!(
                "ack_deadletter: execution {hid} is {}, not a dead letter — only \
                 failed / timed_out / cancelled work is acknowledged",
                fantastic_bundle::py_repr_value(&status)
            ),
        );
        crate::refusal::with(&mut r, "arg", json!("id"));
        return Ok(r);
    }
    if hit.get("acked").is_some_and(fantastic_bundle::py_truthy) {
        return Ok(json!({"acked": true, "id": hid, "already": true}));
    }
    if let Some(root) = kernel.root() {
        let mut row = Map::new();
        row.insert("kind".to_string(), Value::from("ack"));
        row.insert("parent".to_string(), Value::from(hid));
        row.insert(
            "caller".to_string(),
            viewer
                .as_ref()
                .map_or(Value::Null, |v| Value::from(v.0.clone())),
        );
        let _ = kernel.rows_put(&root.id, row);
    }
    kernel.ledger.mark_acked(hid);
    Ok(json!({"acked": true, "id": hid}))
}
