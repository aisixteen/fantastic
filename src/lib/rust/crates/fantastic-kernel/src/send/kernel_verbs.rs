//! The KERNEL VERBS — the substrate's own surface, answered by the ROOT alone.
//!
//! They act on the TREE and the kernel-wide tables, so the agent a caller names
//! is the OPERAND, not the actor. That is why they live behind one address
//! (`kernel`) and why every reflect advertises them: answered everywhere, "send
//! create_agent to X" read three ways and only one was ever true.

use super::*;
use crate::declared::IdOrName;

/// Is `verb` one the kernel answers itself (the declared kernel verbs).
pub(super) fn is_kernel_verb(verb: &str) -> bool {
    crate::declared::kernel::VERBS.iter().any(|v| v.name == verb)
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
        K::CreateAgent(args) => {
            let caller = crate::send::current_origin();
            crate::lifecycle::create_agent(kernel, caller.as_ref(), args).await
        }
        K::DeleteAgent(args) => {
            let caller = crate::send::current_origin();
            crate::lifecycle::delete_agent(kernel, caller.as_ref(), &args).await
        }
        K::UpdateAgent(args) => update_agent(kernel, target, args).await,
        // The EXECUTION LEDGER — a PROJECTION, not a table: the executions still
        // in RAM (in flight, or settled and not yet flushed) merged with the
        // `call`/`settle`/`ack` rows the kernel flushed into its memory store —
        // each call paired with its settle. CALLER-SCOPED: an agent sees what IT
        // originated, the operator sees all. It needs no line: unscoped it would be
        // a read of every caller's executions.
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
                crate::paging::id_text(r.get("id"))
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
        // The live TASK TABLE: one row per in-flight dispatch, `parent` chaining
        // them into the send tree. `ps` shows its own row too — it IS a dispatch.
        // It needs no line. The operator sees every row; an agent sees the rows of
        // the agents in its own tree, with a `parent` task of an agent outside it
        // shown as `null`. Kernel service tasks are the operator's alone.
        K::Ps(args) => {
            let viewer = crate::send::current_origin();
            let rows = visible_tasks(kernel, viewer.as_ref());
            // PAGED — the cursor is the id of the last row handed over. Over a
            // LIVE table a cursor may miss a row that finished mid-walk; this is
            // a kernel, not a snapshot store.
            let (rows, next) = crate::paging::page(rows, args.limit, args.after.as_deref(), |r| {
                crate::paging::id_text(r.get("id"))
            });
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
        //
        // OWN ONLY: an agent cancels the tasks of the agents in its own tree, and
        // resolves only the names of executions it started; anything else answers
        // as unresolved. A kernel service task stays the operator's.
        K::Cancel(args) => {
            let viewer = crate::send::current_origin();
            let resolved = match args.task_id {
                IdOrName::Id(n) => Some(n),
                IdOrName::Name(name) => kernel.ledger.resolve_own_name(&name, viewer.as_ref()),
            };
            let reachable = |n: i64| match &viewer {
                None => true,
                Some(v) => {
                    let t = TaskId(n);
                    !kernel.is_service_task(&t)
                        && kernel
                            .tasks
                            .agent_of(&t)
                            .is_some_and(|a| kernel.in_tree(&a, v))
                }
            };
            match resolved {
                Some(n) if reachable(n) => kernel.cancel_task(&TaskId(n)).await,
                _ => json!({"cancelled": false, "killed": []}),
            }
        }
        // THE PROCESS VERBS are the in-process operator's alone: an agent caller
        // (a remote operator through a leg included) is refused before anything
        // runs. Kernel verbs are answered on the root only (a kernel verb sent to
        // any other agent is answered `not_here` before it reaches here).
        K::KernelStart(_) | K::ShutdownAgents(_) | K::KernelStop(_)
            if crate::send::current_origin().is_some() =>
        {
            let verb = verb_of(payload);
            crate::refusal::refusal(
                crate::refusal::NOT_ALLOWED,
                format!("{verb}: operator only"),
            )
        }
        // THE START SEQUENCE — see `Kernel::start`. Idempotent.
        K::KernelStart(_) => kernel.start(true).await,
        K::ShutdownAgents(_) => kernel.shutdown_agents().await,
        K::KernelStop(_) => {
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

/// The `ps` rows `viewer` sees: every row for the operator (`None`); for an
/// agent, the rows whose `agent` is in its own tree, kernel service tasks left
/// out, and a `parent` task whose agent is outside that set shown as `null`.
fn visible_tasks(kernel: &Arc<Kernel>, viewer: Option<&AgentId>) -> Vec<Value> {
    let rows = kernel.tasks.rows();
    let Some(v) = viewer else {
        return rows;
    };
    let shown = |row: &Value| -> bool {
        let Some(n) = row.get("id").and_then(Value::as_i64) else {
            return false;
        };
        !kernel.is_service_task(&TaskId(n))
            && row
                .get("agent")
                .and_then(Value::as_str)
                .is_some_and(|a| kernel.in_tree(&AgentId::from(a), v))
    };
    let ids: std::collections::HashSet<i64> = rows
        .iter()
        .filter(|r| shown(r))
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .collect();
    rows.into_iter()
        .filter(|r| r.get("id").and_then(Value::as_i64).is_some_and(|n| ids.contains(&n)))
        .map(|mut r| {
            let hidden = r
                .get("parent")
                .and_then(Value::as_i64)
                .is_some_and(|p| !ids.contains(&p));
            if hidden {
                r["parent"] = Value::Null;
            }
            r
        })
        .collect()
}

/// Is field `key` of `target` FORCED by a live agent's create line: a line whose
/// target's tree strictly holds `target`, whose `who` matches its id and whose
/// `what[bundle][key]` is a value (not `"*"`)? Only the operator changes a forced
/// field. A scan of the live lines at update time.
fn forced_field(kernel: &Arc<Kernel>, target: &Agent, key: &str) -> bool {
    let bundle = target.handler_module.as_deref().unwrap_or("");
    kernel.agent_ids().iter().any(|holder| {
        let Some(h) = kernel.agents.get(holder).map(|e| Arc::clone(&e)) else {
            return false;
        };
        let can = h
            .meta
            .read()
            .expect("meta lock poisoned")
            .get("can")
            .cloned()
            .unwrap_or(Value::Null);
        crate::authz::solver::lines_of(&can).into_iter().any(|l| {
            let (crate::authz::Target::Id(t), crate::authz::Level::Create { who, what }) =
                (l.target, l.level)
            else {
                return false;
            };
            let t = AgentId::from(t.as_str());
            t != target.id
                && kernel.in_tree(&target.id, &t)
                && crate::lifecycle::who_matches(&who, target.id.as_str())
                && matches!(
                    crate::lifecycle::field_rule(&what, bundle, key),
                    crate::lifecycle::FieldRule::Forced(_)
                )
        })
    })
}

/// `update_agent`, answered by the ROOT (`root`): the agent named by `id` is the
/// operand. The CALLER is `current_origin()`: the operator (`None`) changes any
/// field; an agent changes field K of agent X only when its lines (capped) allow
/// `update K` on X, except that it writes every non-`can` field of its OWN record
/// (a bundle's own fields). `can` on its own record stays refused (never upward),
/// and a field a create line forces is the operator's alone. Any refused key
/// answers the unknown answer for the target, as a missing id does. A `can` an
/// agent writes holds only lines its own written lines hold (rule 7,
/// [`crate::authz::glue::check_can_edit`]).
async fn update_agent(
    kernel: &Arc<Kernel>,
    root: &Arc<Agent>,
    args: crate::declared::kernel::UpdateAgent,
) -> Value {
    let id = AgentId::from(args.id.as_str());
    // The patch is the declared arguments' remainder: every field of the record to
    // change. Capture changed-field names so the lifecycle event mirrors Python's
    // `agent_updated.changed`.
    let patch: Map<String, Value> = args.fields;
    let changed: Vec<String> = patch.keys().cloned().collect();
    let Some(target) = kernel.agents.get(&id).map(|e| Arc::clone(&e)) else {
        return crate::refusal::unknown_agent(id.as_str());
    };
    let origin = crate::send::current_origin();
    if let Some(caller) = &origin {
        let own = *caller == target.id;
        let refused = patch.keys().any(|k| {
            let by_line = if own {
                k != "can"
            } else {
                kernel.allowed(
                    caller,
                    &crate::authz::solver::Act::Update(k.clone()),
                    &target,
                )
            };
            !by_line || forced_field(kernel, &target, k)
        });
        if refused {
            return crate::refusal::unknown_agent(id.as_str());
        }
    }
    // WRITE-TIME VALIDATION: the target's own entry judges the patch before anything
    // changes (a bundle's shared record entry and its `validate_meta`; a bare agent's
    // open common record); the kernel only routes the refusal.
    let current = target.meta.read().expect("meta poisoned").clone();
    let judged = match target
        .handler_module
        .as_deref()
        .and_then(|hm| kernel.bundles.get(hm))
    {
        Some(bundle) => bundle.check_write("update_agent", &patch, &current, Some(&id.0)),
        None => crate::declared::bare_record::check("update_agent", &patch),
    };
    if let Err(refused) = judged {
        return refused;
    }
    // `can` LINES, judged at write time against the legal shape (their declared
    // shape was the entry's, above).
    if let Some(why) = crate::lifecycle::bad_can(&patch) {
        let mut r = crate::refusal::refusal(crate::refusal::INVALID_META, &why);
        crate::refusal::with(&mut r, "id", json!(id));
        return r;
    }
    // RULE 7: an agent hands out only what its own written lines hold, cap
    // included; only the lines added or widened against the stored `can` are
    // checked, so narrowing is always allowed.
    if let (Some(caller), Some(new)) = (&origin, patch.get("can")) {
        let old = current.get("can").cloned().unwrap_or(Value::Null);
        if let Err(refused) =
            crate::authz::glue::check_can_edit(kernel, caller, &target.id, &old, new)
        {
            return refused;
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
    // Emit `agent_updated` on the root's inbox so watchers (canvas
    // frame chrome, telemetry panels) refresh without polling. Mirrors
    // Python's `await self.emit(self.id, {type:"agent_updated", ...})`.
    kernel
        .emit(
            &root.id,
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
    if hit.get("acked").and_then(Value::as_bool) == Some(true) {
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
