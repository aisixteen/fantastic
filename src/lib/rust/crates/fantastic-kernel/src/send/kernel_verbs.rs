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

/// What an agent never writes on its OWN record. SELF-WRITES ARE UNIFORM (owner,
/// 2026-11-24): once loaded a bundle is an agent like any other, so its code and
/// an `update_agent` call it sends (a model's, a page's) meet this one rule. Its
/// lines (never upward), its identity and its place; forced values are refused
/// as well ([`forced_field`]). Every other own field is ordinary.
const NEVER_OWN: [&str; 5] = ["can", "id", "parent", "parent_id", "handler_module"];

/// The refusal of the first key of `patch` that `caller` may not change on
/// `target`, or `None`. A caller that holds nothing on the target gets the unknown
/// answer; one that sees it (its own record included) gets `not_allowed` naming
/// the key, and what to do: a structure field is never updated, a forced field is
/// the operator's, its own `can` is written by whoever holds `update can` on it,
/// any other field by whoever holds a wider line.
fn refused_key(
    kernel: &Arc<Kernel>,
    caller: &AgentId,
    target: &Agent,
    patch: &Map<String, Value>,
) -> Option<Value> {
    let own = *caller == target.id;
    let (k, why) = patch.keys().find_map(|k| {
        let by_line = if own {
            !NEVER_OWN.contains(&k.as_str())
        } else {
            kernel.allowed(caller, &crate::authz::solver::Act::Update(k.clone()), target)
        };
        if !by_line {
            Some((k, Why::Line))
        } else if forced_field(kernel, target, k) {
            Some((k, Why::Forced))
        } else {
            None
        }
    })?;
    let key = fantastic_bundle::py_repr(k);
    let t = fantastic_bundle::py_repr(target.id.as_str());
    let seen = || kernel.held(caller, &target.id) != crate::send::gate::Held::Nothing;
    Some(if crate::authz::solver::STRUCTURE.contains(&k.as_str()) && seen() {
        crate::refusal::not_allowed(
            k,
            format!("update_agent: {key} can never be updated"),
            "id, parent and handler_module are set once, by create_agent",
        )
    } else if matches!(why, Why::Forced) && seen() {
        crate::refusal::not_allowed(
            k,
            format!("update_agent: {key} on {t} is forced by a create line"),
            "only the operator changes a forced field",
        )
    } else if own && k == "can" {
        crate::refusal::not_allowed(
            k,
            "update_agent: an agent never writes its own can",
            "ask an agent that holds update can on you",
        )
    } else if own {
        crate::refusal::not_allowed(
            k,
            format!("update_agent: an agent never writes its own {key}"),
            "it is set once, by create_agent",
        )
    } else {
        kernel.lacks(caller, &target.id, target.id.as_str(), k, || {
            format!("holds no update {key} on")
        })
    })
}

/// Why a key of an `update_agent` patch is refused.
enum Why {
    /// No line of the caller (capped) allows it, or it is never the caller's own.
    Line,
    /// A create line forces it: the operator's alone.
    Forced,
}

/// `update_agent`, answered by the ROOT (`root`): the agent named by `id` is the
/// operand. The CALLER is `current_origin()`: the operator (`None`) changes any
/// field; an agent changes field K of agent X only when its lines (capped) allow
/// `update K` on X. On its OWN record it changes its ordinary fields, every key
/// but [`NEVER_OWN`], with no line (one rule for its code and its verb calls).
/// A field a create line forces is the operator's alone. A refused key answers
/// `not_allowed` naming it when the caller sees the target, else the unknown
/// answer, as a missing id does ([`refused_key`]). A `can` an
/// agent writes holds only lines its own written lines hold (rule 7,
/// [`crate::authz::glue::check_can_edit`]). The reply carries the record cut to
/// what the caller holds on it (rule 9, the reflect cut).
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
        if let Some(refused) = refused_key(kernel, caller, &target, &patch) {
            return refused;
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
    // The check reads other records with no lock held, so the write that follows
    // is a compare-and-set on `can` (as `Kernel::edit_can`): it lands only when the
    // stored `can` is still the one checked.
    let checked_can = match (&origin, patch.get("can")) {
        (Some(caller), Some(new)) => {
            let old = current.get("can").cloned().unwrap_or(Value::Null);
            if let Err(refused) =
                crate::authz::glue::check_can_edit(kernel, caller, &target.id, &old, new)
            {
                return refused;
            }
            Some(old)
        }
        _ => None,
    };
    let rec = match checked_can {
        Some(old) => match target.update_meta_if_can(patch, &old) {
            Some(rec) => rec,
            None => {
                let mut r = crate::refusal::refusal(
                    crate::refusal::INVALID_META,
                    format!(
                        "update_agent: the can of {} changed during the edit — edit it again",
                        fantastic_bundle::py_repr(id.as_str())
                    ),
                );
                crate::refusal::with(&mut r, "id", json!(id));
                return r;
            }
        },
        None => target.update_meta(patch),
    };
    // NO INLINE PERSIST: the `updated` event published below is what the root
    // loader's `persist_writer` turns into the merge-write.
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
    // `agent_updated` on the root's inbox so watchers (canvas frame chrome,
    // telemetry panels) refresh without polling. Kernel bookkeeping, fanned out
    // like `agent_created`: EACH RECEIVER gets the updated record cut to what it
    // holds on the target (rule 9, the reflect cut), a receiver that holds nothing
    // on it no event, and a receiver the kernel cannot place (a watcher that is no
    // agent, the state stream) the identity keys only.
    let event = |shown: crate::agent::AgentRecord| {
        crate::inbox::InboxItem::text(crate::jsonsafe::obj([
            ("type", Value::from("agent_updated")),
            ("id", Value::from(rec.id.clone())),
            ("changed", Value::from(changed.clone())),
            ("agent", crate::jsonsafe::record_value(&shown)),
        ]))
    };
    kernel
        .fanout_each(
            root,
            "emit",
            |receiver| match receiver {
                Some(r) => kernel
                    .record_cut_for(Some(r), &target, rec.clone())
                    .map(&event),
                None => Some(event(Kernel::identity_record(rec.clone()))),
            },
            crate::send::current_sender().as_ref(),
        )
        .await;
    // PYTHON'S REPLY SHAPE. The bare record left a caller that branches on
    // `updated` — the field the kernel_verbs doc teaches — reading `undefined`
    // on this runtime for the call that succeeded, so a model read success as
    // failure and retried. The record is CUT to what the caller holds on the
    // target: a caller holding `update: [x]` reads its identity and `x`, never
    // the rest (`can` included). The operator and the agent itself read it whole.
    let shown = kernel
        .record_cut_for(origin.as_ref(), &target, rec.clone())
        .unwrap_or_else(|| Kernel::identity_record(rec.clone()));
    crate::jsonsafe::obj([
        ("updated", Value::from(true)),
        ("id", Value::from(rec.id)),
        ("agent", crate::jsonsafe::record_value(&shown)),
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
