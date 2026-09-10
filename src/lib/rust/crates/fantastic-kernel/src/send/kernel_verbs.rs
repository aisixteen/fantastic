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
            // NAMED and unnamed executions and what they became. Terminal
            // non-completed rows ARE the dead-letter queue.
            | "ledger"
            // CONVERSATION ROWS BY ID, gated INSIDE against each row's owner.
            | "messages"
            // THE ROW STORE's whole surface — hydration's pair, PRIVILEGED.
            | "rows_read"
            | "rows_write"
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
        // The verb lists EVERY row it holds, so the doc says so. It used to have
        // no entry here at all (python's and swift's both claimed NAMED-only
        // while their replies carried the unnamed rows too), which is the worst
        // of the three: a caller reading a reflect learned the verb existed only
        // from the code.
        "rows_read": "args: agent:str (req), after:int?, limit:int?. PRIVILEGED (a \
    wildcard does not carry it). \
    That agent's rows WHOLE (content included), oldest first — {rows:[…], next?}. \
    The hydration read; agents read digests through `messages` instead.",
        "rows_write": "args: agent:str (req), rows:list[dict] (req). PRIVILEGED (a \
    wildcard does not carry it). \
    Appends rows to that agent's conversation, each under a FRESH id — {ids:[int]}. \
    A row may not carry an `id`; re-ids are the caller's to remap.",
        "messages": "args: ids:list[int] (req), limit:int?, after:int?. Conversation \
    rows by id, at DIGEST resolution — {rows:[{id, agent, parent, kind, role, head, \
    bytes, title?, level?, refs?}], refused:[ids], next?}. Your own rows always; \
    another agent's only if its callable_by names you for `messages`. Bodies are \
    behind that agent's `pull`.",
        "ledger": "No args. EVERY execution and what it became — named under its id, \
    unnamed under its minted pid — {executions:[{id, target, status, attempts, \
    retries, named, outcome?}]}, paged, scoped to what YOU originated. \
    Failed/timed_out/cancelled rows are the dead-letter queue: inspect, then \
    retry with a NEW id if you choose — nothing is retried silently.",
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
        // CONVERSATION ROWS BY ID. Gated INSIDE, per row, against the OWNER's
        // `callable_by` — a root verb skips ingress, so the same check `gate`
        // runs is re-run here or this would be a side channel around it. Refused
        // ids are LISTED, never silently dropped: a caller must be able to tell
        // "not yours" from "not there".
        "messages" => messages(kernel, payload),
        // THE ROW STORE's whole surface. `messages` is the read an owner may
        // GRANT per row (digests, no bodies); these two are hydration — reading
        // a conversation whole and writing one into an agent — which is what a
        // loader does from above. PRIVILEGED, so a wildcard never carries them:
        // saying yes takes a grant that NAMES them.
        "rows_read" => rows_read(kernel, payload),
        "rows_write" => rows_write(kernel, payload),
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
        // A pid is a NUMBER now, so `task_id` may arrive as one, as the decimal
        // string a URL or a form carries, or as the NAME a caller gave its own
        // execution — the ledger resolves that last case. An id that resolves to
        // nothing is an honest `{cancelled:false}`, not a refusal: python does
        // not pretend to know whether a stranger's id ever existed.
        "cancel" => match payload.get("task_id") {
            Some(Value::Number(n)) if n.as_i64().is_some() => {
                kernel.cancel_task(&TaskId(n.as_i64().unwrap())).await
            }
            Some(Value::String(t)) if !t.is_empty() => {
                let id = t
                    .parse::<i64>()
                    .ok()
                    .or_else(|| kernel.ledger.resolve_name(t));
                match id {
                    Some(n) => kernel.cancel_task(&TaskId(n)).await,
                    None => json!({"cancelled": false, "killed": []}),
                }
            }
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
            if k == "type" || k == "id" || k == "parent_id" {
                continue;
            }
            // REFUSED, not silently skipped. `handler_module` is fixed at
            // create — swapping it would leave a live agent whose in-process
            // state belongs to the OLD bundle. Reserved keys name substrate
            // wiring; they used to be dropped without a word, so the caller
            // believed a change had landed that never did (2026-09-02).
            if k == "handler_module" || RESERVED_RECORD_KEYS.contains(&k.as_str()) {
                return crate::refusal::bad(
                    "update_agent",
                    k,
                    "absent — handler_module is fixed at create_agent, and \
                     root_path/ctx/parent name substrate wiring the record may \
                     not carry",
                );
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
    // PYTHON'S REPLY SHAPE. The bare record left a caller that branches on
    // `updated` — the field the kernel_verbs doc teaches — reading `undefined`
    // on this runtime for the call that succeeded, so a model read success as
    // failure and retried.
    json!({ "updated": true, "id": rec.id, "agent": rec_value })
}

/// The `messages` verb — conversation rows by id, at digest resolution.
///
/// A port of python's `_verb_messages`. Bodies never appear here: a `head` is
/// 120 characters and the whole thing is behind the owning agent's `pull`.
fn messages(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    let ids: Vec<Option<i64>> = match payload.get("ids") {
        Some(Value::Array(a)) => a.iter().map(as_id).collect(),
        _ => Vec::new(),
    };
    if ids.is_empty() || ids.iter().any(Option::is_none) {
        let mut r = crate::refusal::refusal(
            crate::refusal::BAD_ARG,
            "messages: `ids` must be a non-empty list of message ids (ints)",
        );
        crate::refusal::with(&mut r, "arg", json!("ids"));
        return r;
    }
    let mut ids: Vec<i64> = ids.into_iter().flatten().collect();
    if let Some(after) = payload.get("after").and_then(as_id) {
        if let Some(at) = ids.iter().position(|i| *i == after) {
            ids.drain(..=at);
        }
    }

    let caller = crate::send::current_origin();
    let root_id = kernel.root().map(|r| r.id.0.clone());
    let mut decided: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    let mut out: Vec<Value> = Vec::new();
    let mut refused: Vec<i64> = Vec::new();

    for id in ids {
        let Some(row) = kernel.rows.get_one(id) else {
            refused.push(id);
            continue;
        };
        let owner = row
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !may_read(
            kernel,
            caller.as_ref(),
            &owner,
            root_id.as_deref(),
            &mut decided,
        ) {
            refused.push(id);
            continue;
        }
        out.push(digest(id, &row));
    }

    // DIGESTS FIT WHERE THEY ARE GOING: page by `limit`, and never past ~32KB.
    let limit = payload.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize;
    const CAP_BYTES: usize = 32 * 1024;
    let mut kept: Vec<Value> = Vec::new();
    let mut used = 0usize;
    for d in &out {
        if limit > 0 && kept.len() >= limit {
            break;
        }
        let n = d.to_string().len();
        if used + n > CAP_BYTES {
            break;
        }
        used += n;
        kept.push(d.clone());
    }
    let next = (kept.len() < out.len())
        .then(|| kept.last().and_then(|d| d["id"].as_i64()))
        .flatten();
    json!({"rows": kept, "refused": refused, "next": next})
}

/// A message id from whatever the caller wrote: an int, or the decimal string a
/// URL or a form carries. A bool is not an id.
fn as_id(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// May `caller` read a row owned by `owner`? Its own always; another's only when
/// that owner's `callable_by` names it for `messages`; the operator sees all.
fn may_read(
    kernel: &Arc<Kernel>,
    caller: Option<&AgentId>,
    owner: &str,
    root_id: Option<&str>,
    decided: &mut std::collections::HashMap<String, bool>,
) -> bool {
    let Some(caller) = caller else { return true };
    if caller.0 == owner {
        return true;
    }
    if let Some(known) = decided.get(owner) {
        return *known;
    }
    let allowed = kernel
        .agents
        .get(&AgentId::from(owner))
        .map(|e| {
            let holder = e.value();
            let grant = holder
                .meta
                .read()
                .expect("meta lock poisoned")
                .get("callable_by")
                .cloned()
                .unwrap_or(Value::Null);
            let children: std::collections::HashSet<String> =
                holder.child_ids().into_iter().map(|c| c.0).collect();
            crate::authz::authorize_call(
                &grant,
                "messages",
                caller.0.as_str(),
                holder.id.0.as_str(),
                holder.parent_id.as_ref().map(|p| p.0.as_str()),
                &children,
                root_id,
                // A ROOT VERB SKIPS INGRESS, which is why this runs at all — but
                // it is the OWNER's ingress being re-checked, not an egress
                // escalation, so privileged-verb cover does not apply.
                false,
            )
            .allowed
        })
        .unwrap_or(false);
    decided.insert(owner.to_string(), allowed);
    allowed
}

/// One row at DIGEST resolution — never its body.
fn digest(id: i64, row: &crate::rows::Row) -> Value {
    let content = row.get("content").and_then(Value::as_str).unwrap_or("");
    let mut d = json!({
        "id": id,
        "agent": row.get("agent"),
        "parent": row.get("parent"),
        "kind": row.get("kind").or_else(|| row.get("role")),
        "role": row.get("role"),
        "head": content.chars().take(120).collect::<String>().replace('\n', " "),
        "bytes": content.len(),
    });
    let m = d.as_object_mut().expect("digest is an object");
    if let Some(t) = row.get("title").filter(|v| !v.is_null()) {
        m.insert("title".to_string(), t.clone());
    }
    if let Some(l) = row.get("level").filter(|v| !v.is_null()) {
        m.insert("level".to_string(), l.clone());
    }
    if let Some(Value::Array(r)) = row.get("refs") {
        m.insert("refs".to_string(), json!(r.len()));
    }
    match row.get("kind").and_then(Value::as_str) {
        Some("call") | Some("task") => {
            m.insert(
                "verb".to_string(),
                row.get("verb").cloned().unwrap_or(Value::Null),
            );
            if let Some(n) = row.get("name").filter(|v| !v.is_null()) {
                m.insert("name".to_string(), n.clone());
            }
            if let Some(status) = row.get("status").filter(|v| !v.is_null()) {
                m.insert("status".to_string(), status.clone());
            }
        }
        Some("view") => {
            // A VIEW DIGEST SAYS HOW MUCH IT HOLDS, never the array itself.
            let n = row.get("ids").and_then(Value::as_array).map_or(0, Vec::len);
            m.insert("ids".to_string(), json!(n));
            m.insert(
                "budget".to_string(),
                row.get("budget").cloned().unwrap_or(Value::Null),
            );
            m.insert(
                "title".to_string(),
                row.get("why").cloned().unwrap_or(Value::Null),
            );
        }
        _ => {}
    }
    d
}

/// `rows_read` — one agent's conversation WHOLE, oldest first.
fn rows_read(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    let Some(agent) = payload
        .get("agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        let mut r = crate::refusal::refusal(
            crate::refusal::BAD_ARG,
            "rows_read: `agent` must be an agent id",
        );
        crate::refusal::with(&mut r, "arg", json!("agent"));
        return r;
    };
    let mut rows: Vec<Value> = kernel
        .rows
        .for_agent(&AgentId::from(agent))
        .into_iter()
        .map(Value::Object)
        .collect();
    if let Some(after) = payload.get("after").and_then(as_id) {
        // AN UNKNOWN CURSOR PAGES NOTHING, never the head again.
        let at = rows
            .iter()
            .position(|r| r["id"].as_i64() == Some(after))
            .map(|i| i + 1)
            .unwrap_or(rows.len());
        rows.drain(..at);
    }
    let limit = payload.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize;
    let truncated = limit > 0 && rows.len() > limit;
    if truncated {
        rows.truncate(limit);
    }
    let next = truncated
        .then(|| rows.last().and_then(|r| r["id"].as_i64()))
        .flatten();
    json!({"rows": rows, "next": next})
}

/// `rows_write` — append rows to an agent's conversation, each under a fresh id.
fn rows_write(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    let Some(agent) = payload
        .get("agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        let mut r = crate::refusal::refusal(
            crate::refusal::BAD_ARG,
            "rows_write: `agent` must be an agent id",
        );
        crate::refusal::with(&mut r, "arg", json!("agent"));
        return r;
    };
    let bad_rows = |msg: &str| {
        let mut r = crate::refusal::refusal(crate::refusal::BAD_ARG, msg);
        crate::refusal::with(&mut r, "arg", json!("rows"));
        r
    };
    let Some(rows) = payload.get("rows").and_then(Value::as_array) else {
        return bad_rows("rows_write: `rows` must be a non-empty list of rows");
    };
    if rows.is_empty() || !rows.iter().all(Value::is_object) {
        return bad_rows("rows_write: `rows` must be a non-empty list of rows");
    }
    // THE STORE IS THE ONLY ALLOCATOR. A caller that could name an id could
    // collide with a pre-minted one, or overwrite a row it does not own — and a
    // COPY is a new row, not the same row twice.
    if rows
        .iter()
        .any(|r| r.get("id").is_some_and(|v| !v.is_null()))
    {
        return bad_rows(
            "rows_write: a row may not carry an `id` — the store mints every id, \
             and a copy is a NEW row",
        );
    }
    let agent = AgentId::from(agent);
    let mut ids = Vec::with_capacity(rows.len());
    for r in rows {
        let row = r.as_object().expect("checked above").clone();
        match kernel.rows.put(&agent, row) {
            Ok(id) => ids.push(id),
            Err(e) => return bad_rows(&format!("rows_write: {e}")),
        }
    }
    json!({"ids": ids})
}
