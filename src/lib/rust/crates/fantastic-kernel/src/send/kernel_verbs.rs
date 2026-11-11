//! The KERNEL VERBS — the substrate's own surface, answered by the ROOT alone.
//!
//! They act on the TREE and the kernel-wide tables, so the agent a caller names
//! is the OPERAND, not the actor. That is why they live behind one address
//! (`kernel`) and why every reflect advertises them: answered everywhere, "send
//! create_agent to X" read three ways and only one was ever true.

use super::*;
use crate::declared::IdOrName;

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
            // WAITING for a set of agents — one wake for N reports (a message of
            // `type: "report"` from each named agent). Caller-scoped.
            | "await_set"
            | "await_cancel"
            // HOW TO CALL an agent — its bundle's verb docs. Answered by the
            // root and gated like every kernel verb (`gate`).
            | "docs"
    )
}

/// `Err` is a python RAISE the verb emulates (a store read that failed, an
/// `int()` that threw) — settled `failed`, out of band; `Ok` is the reply.
pub(super) async fn handle_kernel_verb(
    kernel: &Arc<Kernel>,
    target: &Arc<Agent>,
    payload: &Value,
) -> Result<Value, Value> {
    use crate::declared::kernel::Call as K;
    // The kernel is the agent of its own verbs: the shared entry checks the call
    // against the declaration; the typed call is the exhaustive list it answers.
    let call = match K::parse("kernel", payload) {
        Ok(c) => c,
        Err(refused) => return Ok(refused),
    };
    Ok(match call {
        K::Docs(args) => {
            if args.of.contains('|') {
                crate::docs::remote_docs_reply(kernel, &args).await
            } else {
                crate::docs::docs_reply(kernel, &args)
            }
        }
        K::ListAgents(args) => {
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
            let (out, next) = crate::paging::page(out, args.limit, args.after.as_deref(), |r| {
                r.get("id")
                    .map_or_else(String::new, fantastic_bundle::py_str)
            });
            // `more` is not decoration (py-canonical): a model that opens with
            // `list_agents` gets bare records and never learns that authoring a
            // script agent is the mechanism, while one that opens with `reflect`
            // gets `self_extension`. This verb rides every reflect's
            // `system_verbs`, so it needs a signpost back to the briefing.
            json!({ "agents": out, "next": next, "more": LIST_HINT })
        }
        K::CreateAgent(args) => crate::lifecycle::create_agent(kernel, target, args).await,
        K::DeleteAgent(args) => crate::lifecycle::delete_agent(kernel, target, &args).await,
        K::UpdateAgent(args) => update_agent(kernel, target, args).await,
        // The EXECUTION LEDGER — a PROJECTION, not a table: the executions still
        // in RAM (in flight, or settled and not yet flushed) merged with the
        // `call`/`settle`/`ack` rows the kernel flushed into its memory store —
        // each call paired with its settle. CALLER-SCOPED: an agent sees what IT
        // originated, the operator sees all. Unscoped this would be a side-channel
        // around `callable_by`.
        K::Ledger(args) => {
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
            let (rows, next) = crate::paging::page(rows, args.limit, args.after.as_deref(), |r| {
                r.get("id")
                    .map_or_else(String::new, fantastic_bundle::py_str)
            });
            json!({ "executions": rows, "next": next })
        }
        K::AckDeadletter(args) => ack_deadletter(kernel, &args.id).await?,
        K::AwaitSet(args) => {
            crate::await_set::await_set(kernel, crate::send::current_origin(), &args).await
        }
        K::AwaitCancel(args) => {
            crate::await_set::await_cancel(kernel, crate::send::current_origin(), &args).await
        }
        // The live TASK TABLE, kernel-wide (like `list_agents`), not per-agent:
        // one row per in-flight dispatch, `parent` chaining them into the send
        // tree. `ps` shows its own row too — it IS a dispatch. The table is honest.
        K::Ps(args) => {
            // PAGED like `list_agents` — same cursor law: the id of the last row
            // handed over. Over a LIVE table a cursor may miss a row that
            // finished mid-walk; this is a kernel, not a snapshot store.
            let (rows, next) = crate::paging::page(
                kernel.tasks.rows(),
                args.limit,
                args.after.as_deref(),
                |r| {
                    r.get("id")
                        .map_or_else(String::new, fantastic_bundle::py_str)
                },
            );
            json!({ "tasks": rows, "next": next })
        }
        // Kill a task AND ITS SUBTREE (children first, like killing a process
        // group). Each bundle's `on_cancel` hook runs BEFORE the abort so real
        // resources (OS subprocesses, PTYs, sockets) die with it. An unknown or
        // already-finished id returns {cancelled:false} — it does not pretend.
        // `task_id` is the declared `IdOrName`: a number is the task's id, text is the
        // NAME a caller gave its own execution — the ledger resolves it. Text is never
        // read as a number. Either resolving to nothing is an honest
        // `{cancelled:false}`, not a refusal: python does not pretend to know whether
        // a stranger's id ever existed.
        K::Cancel(args) => {
            let resolved = match args.task_id {
                IdOrName::Id(n) => (n > 0).then_some(n),
                IdOrName::Name(name) => kernel.ledger.resolve_name(&name),
            };
            match resolved {
                Some(n) => kernel.cancel_task(&TaskId(n)).await,
                None => json!({"cancelled": false, "killed": []}),
            }
        }
        // THE START SEQUENCE — see `Kernel::start`. Idempotent.
        K::KernelStart(_) => kernel.start(true).await,
        K::ShutdownAgents(_) => {
            // Root control surface only, gated on rootness like
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
        K::KernelStop(_) => {
            // Root control surface only. Gate on rootness
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
    })
}

async fn update_agent(
    kernel: &Arc<Kernel>,
    caller: &Arc<Agent>,
    args: crate::declared::kernel::UpdateAgent,
) -> Value {
    let id = AgentId::from(args.id.as_str());
    // The patch is the declared arguments' remainder: every field of the record to
    // change. Capture changed-field names so the lifecycle event mirrors Python's
    // `agent_updated.changed`.
    let patch: Map<String, Value> = args.fields;
    let changed: Vec<String> = patch.keys().cloned().collect();
    let Some(target) = kernel.agents.get(&id).map(|e| Arc::clone(&e)) else {
        return crate::refusal::unknown_agent(
            id.as_str(),
            kernel.agents.iter().map(|e| e.key().to_string()).collect(),
        );
    };
    // Grant-map SHAPE, judged here at write time — the gate only judges edges
    // (py-canonical: `can_call:"*"` accepted as a bare string would leave a
    // silently-dead grant; refuse loud at the point of the mistake).
    if let Some(why) = crate::lifecycle::bad_grant_shape(&patch) {
        let mut r = crate::refusal::refusal(crate::refusal::INVALID_META, &why);
        crate::refusal::with(&mut r, "id", json!(id));
        return r;
    }
    // WRITE-TIME VALIDATION. The target's bundle judges the patch before anything
    // changes — its shared record entry (the fields its record declares, their types,
    // the substrate wiring no record carries), then its own `validate_meta` — so a
    // refusal names the bad field at the point of the mistake instead of surfacing
    // calls later. A bare agent (no bundle) is judged by the kernel as the agent it
    // is: the common record, open to whatever fields it was given.
    match target
        .handler_module
        .as_deref()
        .and_then(|hm| kernel.bundles.get(hm))
    {
        Some(bundle) => {
            if let Err(refused) = bundle.check_record("update_agent", &patch) {
                return refused;
            }
            let current = target.meta.read().expect("meta poisoned").clone();
            if let Some(why) = bundle.validate_meta(&patch, &current) {
                let mut r = crate::refusal::refusal(crate::refusal::INVALID_META, why);
                crate::refusal::with(&mut r, "id", json!(id));
                return r;
            }
        }
        None => {
            if let Err(refused) = crate::verbs::check_record(
                "update_agent",
                "bare agent",
                &patch,
                &crate::declared::record::RECORD,
                &crate::declared::bare_record::RECORD,
            ) {
                return refused;
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

/// `ack_deadletter` — acknowledge a DEAD LETTER the caller originated (the
/// operator: any). The ack is a row in the kernel's store (`kind: ack`); an acked
/// dead letter may be pruned. Python `_verb_ack_deadletter`.
async fn ack_deadletter(kernel: &Arc<Kernel>, id: &IdOrName) -> Result<Value, Value> {
    // the declared type decides: a number is an execution's id, text its name
    let raw = match id {
        IdOrName::Id(n) => json!(n),
        IdOrName::Name(s) => json!(s),
    };
    let viewer = crate::send::current_origin();
    let entries = kernel
        .ledger_entries()
        .await
        .map_err(|e| crate::executions::raised("RuntimeError", &e))?;
    let hit = entries.into_iter().find(|e| match id {
        IdOrName::Id(n) => e.get("id").and_then(Value::as_i64) == Some(*n),
        IdOrName::Name(_) => e.get("name") == Some(&raw),
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
