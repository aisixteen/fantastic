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
            | "get"
            // The TASK TABLE — work in flight, universal. Baked into the
            // substrate, in no bundle's verb map: cancellation is a property of
            // the one primitive (`send`), so EVERY agent is cancelable and a
            // caller never needs to know which bundle it faces. Plain verbs — an
            // AGENT can call them on another agent's work.
            | "ps"
            | "cancel"
            // NAMED and unnamed executions and what they became. Terminal
            // non-completed rows ARE the dead-letter queue.
            | "ledger"
            // THE BLOB CHANNEL — an agent's sidecars, persisted through the
            // loader's discovered store. The consumer never computes a path and
            // never wires a provider of its own.
            | "persist_blob"
            | "load_blob"
            // Root-only control verb — gated inside `handle_system_verb`
            // to the tree root; a non-root target answers with an error.
            | "shutdown_kernel"
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
/// "spawn a child agent" cut, and a model spent 16 turns unable to work out what
/// the target meant, finishing with `agents_created: []`.
pub fn kernel_verb_docs() -> Value {
    json!({
        "create_agent": "args: handler_module:str, parent_id:str REQUIRED, id:str?, **meta. \
    Creates a CHILD OF `parent_id` — your own id grows your own subtree \
    (no grant needed); 'core' for top-level. No default parent.",
        "update_agent": "args: id:str, **meta. EVERY FIELD YOU NAME IS REPLACED, NOT MERGED \
    (a field you do not name is untouched), so a dict field must be sent \
    WHOLE — `can_call`/`callable_by` included: restate every key you want \
    to keep, the baseline `$self`/`$parent`/`$children` among them, or the \
    keys you omit are GONE. Granting one edge reads like a PATCH and \
    behaves like a PUT.",
        "delete_agent": "args: id:str. CASCADES — deletes that agent AND ITS WHOLE SUBTREE, \
    deepest first; refuses if any descendant carries `delete_lock`.",
        "list_agents": "No args. Every agent's record, flat. Paged.",
        "get": "args: id:str. One agent's record — null if unknown.",
        "ps": "No args. The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, \
    verb, started_at}]}, one row per in-flight dispatch. Paged.",
        "cancel": "args: task_id:str. Kills that task AND ITS SUBTREE — the id names its \
    own agent. Returns {cancelled:bool, killed:[…]}; the blocked caller's \
    batch item settles as status:'cancelled'. Unknown id ⇒ {cancelled:false}.",
        "shutdown_kernel": "No args. Stops the kernel PROCESS — under env:'container', the container.",
    })
}

/// The `_` line that heads `kernel_verbs` — states WHERE to send them and what
/// the named id means. Without it "send create_agent to X" reads three ways (X
/// performs it / X is created / X is the parent) and only the last is true.
pub const KERNEL_VERBS_PREAMBLE: &str =
    "Kernel-owned: send to 'kernel'. The id you name is the OPERAND, \
not the actor. (`verbs` is what this agent itself does.)";

/// `kernel_verbs` as a reflect renders it. `full` keeps each doc whole; the
/// default truncates at the first em-dash.
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
    names.sort();
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

pub(super) async fn handle_kernel_verb(
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
            // PAGED. Absent `limit` ⇒ everything and `next: null`, so a caller
            // that never learned about paging is unaffected; the cursor is the
            // last id handed over, never an offset.
            let (out, next) = crate::paging::page(out, payload, |r| {
                r.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
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
        // The live TASK TABLE, kernel-wide (like `list_agents`), not per-agent:
        // one row per in-flight dispatch, `parent` chaining them into the send
        // tree. `ps` shows its own row too — it IS a dispatch. The table is honest.
        // The EXECUTION LEDGER, ORIGIN-SCOPED: an agent sees only what it
        // originated, the operator sees all. Unscoped this would be a
        // side-channel around `callable_by` — reading other agents' result
        // bodies without ever being granted an edge to them.
        "ledger" => {
            let viewer = crate::send::current_origin();
            let rows = kernel.ledger.rows_for(viewer.as_ref());
            let (rows, next) = crate::paging::page(rows, payload, |r| {
                r.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            });
            json!({ "executions": rows, "next": next })
        }
        "persist_blob" => crate::persistence::persist_blob(kernel, payload).await,
        "load_blob" => crate::persistence::load_blob(kernel, payload).await,
        "ps" => {
            // PAGED like `list_agents` — same cursor law: the id of the last row
            // handed over. Over a LIVE table a cursor may miss a row that
            // finished mid-walk; this is a kernel, not a snapshot store.
            let (rows, next) = crate::paging::page(kernel.tasks.rows(), payload, |r| {
                r.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            });
            json!({ "tasks": rows, "next": next })
        }
        // Kill a task AND ITS SUBTREE (children first, like killing a process
        // group). Each bundle's `on_cancel` hook runs BEFORE the abort so real
        // resources (OS subprocesses, PTYs, sockets) die with it. An unknown or
        // already-finished id returns {cancelled:false} — it does not pretend.
        "cancel" => match payload.get("task_id").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => kernel.cancel_task(&TaskId::from(t)).await,
            _ => crate::refusal::missing("cancel", "task_id"),
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
        _ => unreachable!("is_kernel_verb gate"),
    }
}

async fn update_from_payload(kernel: &Arc<Kernel>, caller: &Arc<Agent>, payload: &Value) -> Value {
    let id = match payload.get("id").and_then(Value::as_str) {
        Some(s) => AgentId::from(s),
        None => return crate::refusal::missing("update_agent", "id"),
    };
    let Some(target) = kernel.agents.get(&id).map(|e| Arc::clone(&e)) else {
        return crate::refusal::unknown_agent(
            id.as_str(),
            kernel.agents.iter().map(|e| e.key().to_string()).collect(),
        );
    };
    // Patch is every field in payload except `type` and `id`. Capture
    // changed-field names so the lifecycle event mirrors Python's
    // `agent_updated.changed`.
    let mut patch: Map<String, Value> = Map::new();
    let mut changed: Vec<String> = Vec::new();
    if let Some(obj) = payload.as_object() {
        for (k, v) in obj {
            // `parent_id` is STRUCTURAL — where the agent actually sits, which
            // only the tree knows. Letting it into meta lets a record LIE about
            // its own parent: `$parent` then resolves differently on the two
            // halves of the gate, and a reboot silently corrects it from the
            // agent's position on disk.
            if k == "type"
                || k == "id"
                || k == "parent_id"
                || RESERVED_RECORD_KEYS.contains(&k.as_str())
            {
                continue;
            }
            patch.insert(k.clone(), v.clone());
            changed.push(k.clone());
        }
    }
    // Grant-map SHAPE, judged here at write time — the gate only judges edges
    // (py-canonical, 2026-09-01: `can_call:"*"` accepted as a bare string left
    // a silently-dead grant; refuse loud at the point of the mistake).
    if let Some(why) = crate::lifecycle::bad_grant_shape(&patch) {
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
