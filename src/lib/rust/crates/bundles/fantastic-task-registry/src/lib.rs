//! `task_registry` — which agent or subtree holds which task, and where it came from.
//!
//! A generic extension between agents and task sources (gh, jira, ...): it makes NO
//! outbound call. `source` and `update_to` are opaque strings; the LLM reads them and
//! pushes updates itself. OFF BY DEFAULT: a host registers the bundle only when
//! `FANTASTIC_TASK_REGISTRY` is truthy ([`register_if_enabled`]), and an instance
//! whose record has no `task_store_id` refuses every verb `disabled`.
//!
//! STORAGE IS ABSTRACT. The registry keeps its tables in the store agent named by its
//! record's `task_store_id`, through the store interface the agent memory uses
//! (`append {rows}`, `read {kinds, match, order, limit, after}`): the creator wires
//! `can_call` to the store and the store's `callable_by` to the registry. Each write
//! appends one snapshot row `{kind:"task", task_id, content:<entry>}`; every read takes
//! the latest row per `task_id`. Nothing is held in memory (a restart rebuilds from the
//! store) and nothing falls back to RAM: a store that refuses or is unreachable is
//! `store_refused`. A file, a path or a database is the store's business, never this
//! bundle's.
//!
//! WRITERS are branch leads. The authority is the kernel call gate: only a lead holds
//! the write verbs in its `can_call` on the registry. A worker only `report`s to its
//! lead (agentic_resources) and the lead writes here. The one rule inside the bundle
//! is that a `report`'s `from` is the caller or one of its descendants.
//!
//! Verbs (mirror Python verb-for-verb): `reflect`, `add`, `assign`, `reassign`,
//! `status`, `report`, `list`, `get`.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::py_repr;
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::refusal::{self, bad, missing};
use fantastic_kernel::{current_origin, Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "task_registry.tools";

/// The environment variable that turns the bundle on in a host.
pub const ENABLE_ENV: &str = "FANTASTIC_TASK_REGISTRY";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// The states a task takes.
pub const STATES: [&str; 6] = ["waiting", "running", "blocked", "done", "failed", "paused"];

/// How many rows one store read asks for.
const PAGE: u64 = 500;

/// Is an env value on? `1`, `true`, `yes`, `on` (any case).
#[must_use]
pub fn enabled_value(v: Option<&str>) -> bool {
    v.is_some_and(|s| {
        matches!(
            s.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Register the bundle only when `value` (the env var) turns it on. Returns
/// whether it registered. Off: no handler, so nothing changes for the host.
pub fn register_if_enabled(reg: &mut BundleRegistry, value: Option<&str>) -> bool {
    if enabled_value(value) {
        reg.register(HANDLER_MODULE, TaskRegistryBundle::default());
        true
    } else {
        false
    }
}

/// The task_registry bundle. It holds no tasks: only one lock per registry agent, so
/// the read-modify-append of two leads cannot race.
#[derive(Default)]
pub struct TaskRegistryBundle {
    locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

fn verb_docs_json() -> Value {
    json!({
        "reflect": "Identity and whether the registry is enabled (has a task_store_id). No args.",
        "add": "args: id:str (req), source:str (req, opaque: gh:o/r#12, jira:KEY-5, ...), parent:str? (a task id), update_to:str? (default source), holder:str|{subtree:str}? (an existing agent), status:str? (default waiting). Refused `exists` (id taken), `not_found` (parent or holder). Returns the entry.",
        "assign": "args: id:str (req), holder:str|{subtree:str} (req; an existing agent id, or a subtree named by its root agent). Only a task nobody holds; else `already_held`. Returns the entry.",
        "reassign": "args: id:str (req), holder:str|{subtree:str} (req). Only a held task; else `not_held`. Agent to agent, to a subtree and back; every step is kept in history {from, to, by, at}. Returns the entry.",
        "status": "args: id:str (req), status:waiting|running|blocked|done|failed|paused (req). Returns the entry.",
        "report": "args: id:str (req), from:str? (an agent id; default you), state:str?, result:any?. Records the task's last report {from, state, result, at}. `from` must be you or one of your descendants, else `not_in_subtree`. Returns the entry.",
        "list": "args: parent:str?, holder:str|{subtree:str}?, status:str?. Returns {tasks:[entry]} sorted by id.",
        "get": "args: id:str (req). Returns the entry plus children (the ids of the tasks whose parent it is); an unknown id is `not_found`.",
    })
}

// ── helpers ─────────────────────────────────────────────────────────

fn meta_of(a: &Agent) -> Map<String, Value> {
    a.meta.read().expect("meta poisoned").clone()
}

fn agent_of(kernel: &Kernel, id: &AgentId) -> Option<Arc<Agent>> {
    kernel.agents.get(id).map(|e| Arc::clone(&e))
}

/// A refusal of the registry's own: `{error:<code>, reason:<text>}`.
fn fail(code: &str, reason: String) -> Value {
    json!({"error": code, "reason": reason})
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Every id above `id`, nearest first.
fn ancestors(kernel: &Kernel, id: &AgentId) -> Vec<AgentId> {
    let mut out = Vec::new();
    let mut cur = agent_of(kernel, id).and_then(|a| a.parent_id.clone());
    while let Some(p) = cur {
        cur = agent_of(kernel, &p).and_then(|a| a.parent_id.clone());
        out.push(p);
    }
    out
}

/// A required non-empty string arg.
fn str_arg<'a>(verb: &str, payload: &'a Value, arg: &str) -> Result<&'a str, Value> {
    match payload.get(arg) {
        None | Some(Value::Null) => Err(missing(verb, arg)),
        Some(Value::String(s)) if !s.is_empty() => Ok(s),
        Some(_) => Err(bad(verb, arg, "a non-empty string")),
    }
}

/// An optional non-empty string arg.
fn opt_str<'a>(verb: &str, payload: &'a Value, arg: &str) -> Result<Option<&'a str>, Value> {
    match payload.get(arg) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s)),
        Some(_) => Err(bad(verb, arg, "a non-empty string")),
    }
}

fn state_arg(verb: &str, arg: &str, v: &str) -> Result<(), Value> {
    if STATES.contains(&v) {
        Ok(())
    } else {
        Err(bad(verb, arg, &format!("one of {}", STATES.join("|"))))
    }
}

/// A holder: an agent id, or `{subtree: <root agent id>}`. It must name an existing
/// agent. Returns the holder as stored.
fn holder_arg(kernel: &Kernel, verb: &str, v: &Value) -> Result<Value, Value> {
    let (id, out) = match v {
        Value::String(s) if !s.is_empty() => (s.as_str(), v.clone()),
        Value::Object(m) if m.len() == 1 => match m.get("subtree") {
            Some(Value::String(s)) if !s.is_empty() => (s.as_str(), v.clone()),
            _ => return Err(bad(verb, "holder", "an agent id or {subtree: agent id}")),
        },
        _ => return Err(bad(verb, "holder", "an agent id or {subtree: agent id}")),
    };
    if agent_of(kernel, &AgentId::from(id)).is_none() {
        return Err(refusal::refusal(
            refusal::NOT_FOUND,
            format!("{verb}: holder {} is not an agent", py_repr(id)),
        ));
    }
    Ok(out)
}

fn not_found_task(verb: &str, id: &str) -> Value {
    refusal::refusal(
        refusal::NOT_FOUND,
        format!("{verb}: task {} not found", py_repr(id)),
    )
}

// ── the store ───────────────────────────────────────────────────────

/// The store agent's id, or `None` when the registry is not configured.
fn store_id(meta: &Map<String, Value>) -> Option<String> {
    meta.get("task_store_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// ONE gated send to the store, as the registry. A refusal or a failed dispatch is
/// `store_refused`, with the store's own error and reason.
async fn store_call(kernel: &Arc<Kernel>, store: &str, payload: Value) -> Result<Value, Value> {
    let out = kernel
        .send(vec![Item::new(AgentId::from(store), payload)])
        .await
        .remove(0);
    let res = out.result;
    let error = res
        .get("error")
        .filter(|e| !e.is_null() && *e != &json!(false) && *e != &json!(""));
    if error.is_some() || out.status.as_str() != "completed" {
        let text = match error {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => out.status.as_str().to_string(),
        };
        let mut r = fail(
            "store_refused",
            format!("store {} refused: {text}", py_repr(store)),
        );
        r["store_error"] = json!(text);
        r["store_reason"] = res.get("reason").cloned().unwrap_or(Value::Null);
        return Err(r);
    }
    Ok(res)
}

/// Every task, latest snapshot per `task_id`, read page by page.
async fn load(
    kernel: &Arc<Kernel>,
    store: &str,
    only: Option<&str>,
) -> Result<BTreeMap<String, Value>, Value> {
    let mut tasks = BTreeMap::new();
    let mut after = Value::Null;
    loop {
        let mut q = json!({"type": "read", "kinds": ["task"], "order": "asc", "limit": PAGE});
        if let Some(t) = only {
            q["match"] = json!({"task_id": t});
        }
        if !after.is_null() {
            q["after"] = after.clone();
        }
        let res = store_call(kernel, store, q).await?;
        for row in res
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(id), Some(content)) = (
                row.get("task_id").and_then(Value::as_str),
                row.get("content").filter(|c| c.is_object()),
            ) {
                tasks.insert(id.to_string(), content.clone());
            }
        }
        match res.get("next") {
            Some(n) if !n.is_null() => after = n.clone(),
            _ => return Ok(tasks),
        }
    }
}

/// Append the new snapshot of a task.
async fn save(kernel: &Arc<Kernel>, store: &str, entry: &Value) -> Result<(), Value> {
    let row = json!({"kind": "task", "task_id": entry["id"], "content": entry});
    store_call(kernel, store, json!({"type": "append", "rows": [row]})).await?;
    Ok(())
}

fn history_item(from: &Value, to: &Value, by: &Option<AgentId>) -> Value {
    json!({"from": from, "to": to, "by": by.as_ref().map(AgentId::as_str), "at": now()})
}

// ── the bundle ──────────────────────────────────────────────────────

impl TaskRegistryBundle {
    fn lock_of(&self, reg: &AgentId) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.locks.lock().expect("locks poisoned");
        Arc::clone(m.entry(reg.as_str().to_string()).or_default())
    }
}

#[async_trait]
impl Bundle for TaskRegistryBundle {
    fn name(&self) -> &str {
        "task_registry"
    }

    fn verb_docs(&self) -> Value {
        verb_docs_json()
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let raw = payload.get("type").unwrap_or(&Value::Null);
        let verb = raw.as_str().unwrap_or("");
        let Some(me) = agent_of(kernel, agent_id) else {
            return Ok(None);
        };
        let meta = meta_of(&me);
        let store = store_id(&meta);
        let reply = match verb {
            "boot" | "shutdown" => return Ok(None),
            "reflect" => json!({
                "id": agent_id.as_str(),
                "sentence": "A registry of tasks: who holds each one and where it came from.",
                "enabled": store.is_some(),
            }),
            _ if store.is_none() => fail(
                "disabled",
                "task_registry is disabled: the record has no task_store_id".to_string(),
            ),
            "list" | "get" => {
                let store = store.unwrap_or_default();
                match verb {
                    "list" => list_reply(kernel, &store, payload).await,
                    _ => get_reply(kernel, &store, payload).await,
                }
            }
            "add" | "assign" | "reassign" | "status" | "report" => {
                let store = store.unwrap_or_default();
                let lock = self.lock_of(agent_id);
                let _held = lock.lock().await;
                let caller = current_origin();
                match verb {
                    "add" => add_reply(kernel, &store, &caller, payload).await,
                    "assign" => assign_reply(kernel, &store, &caller, payload, false).await,
                    "reassign" => assign_reply(kernel, &store, &caller, payload, true).await,
                    "status" => status_reply(kernel, &store, payload).await,
                    _ => report_reply(kernel, &store, &caller, payload).await,
                }
            }
            _ => {
                let mut r = refusal::refusal(
                    refusal::UNHANDLED,
                    format!(
                        "task_registry: unknown type {}",
                        fantastic_bundle::py_repr_value(raw)
                    ),
                );
                refusal::with(&mut r, "verb", raw.clone());
                r
            }
        };
        Ok(Some(reply))
    }
}

// ── verbs ───────────────────────────────────────────────────────────

/// Run `f` and flatten `Result<Value, Value>`.
fn flat(r: Result<Value, Value>) -> Value {
    r.unwrap_or_else(|e| e)
}

async fn add_reply(
    kernel: &Arc<Kernel>,
    store: &str,
    caller: &Option<AgentId>,
    payload: &Value,
) -> Value {
    flat(add_inner(kernel, store, caller, payload).await)
}

async fn add_inner(
    kernel: &Arc<Kernel>,
    store: &str,
    caller: &Option<AgentId>,
    payload: &Value,
) -> Result<Value, Value> {
    let id = str_arg("add", payload, "id")?;
    let source = str_arg("add", payload, "source")?;
    let parent = opt_str("add", payload, "parent")?;
    let update_to = opt_str("add", payload, "update_to")?.unwrap_or(source);
    let status = opt_str("add", payload, "status")?.unwrap_or("waiting");
    state_arg("add", "status", status)?;
    let holder = match payload.get("holder") {
        None | Some(Value::Null) => Value::Null,
        Some(h) => holder_arg(kernel, "add", h)?,
    };
    let tasks = load(kernel, store, None).await?;
    if tasks.contains_key(id) {
        return Err(refusal::refusal(
            refusal::EXISTS,
            format!("add: task {} already exists", py_repr(id)),
        ));
    }
    if let Some(p) = parent {
        if !tasks.contains_key(p) {
            return Err(not_found_task("add", p));
        }
    }
    let history = if holder.is_null() {
        json!([])
    } else {
        json!([history_item(&Value::Null, &holder, caller)])
    };
    let entry = json!({
        "id": id, "parent": parent, "source": source, "update_to": update_to,
        "holder": holder, "history": history, "status": status, "report": null,
    });
    save(kernel, store, &entry).await?;
    Ok(entry)
}

async fn assign_reply(
    kernel: &Arc<Kernel>,
    store: &str,
    caller: &Option<AgentId>,
    payload: &Value,
    re: bool,
) -> Value {
    flat(assign_inner(kernel, store, caller, payload, re).await)
}

async fn assign_inner(
    kernel: &Arc<Kernel>,
    store: &str,
    caller: &Option<AgentId>,
    payload: &Value,
    re: bool,
) -> Result<Value, Value> {
    let verb = if re { "reassign" } else { "assign" };
    let id = str_arg(verb, payload, "id")?;
    let to = match payload.get("holder") {
        None | Some(Value::Null) => return Err(missing(verb, "holder")),
        Some(h) => holder_arg(kernel, verb, h)?,
    };
    let mut entry = load(kernel, store, Some(id))
        .await?
        .remove(id)
        .ok_or_else(|| not_found_task(verb, id))?;
    let from = entry["holder"].clone();
    match (re, from.is_null()) {
        (false, false) => {
            return Err(fail(
                "already_held",
                format!(
                    "assign: task {} is held by {from}; use reassign",
                    py_repr(id)
                ),
            ))
        }
        (true, true) => {
            return Err(fail(
                "not_held",
                format!("reassign: task {} has no holder; use assign", py_repr(id)),
            ))
        }
        _ => {}
    }
    entry["history"]
        .as_array_mut()
        .expect("history is a list")
        .push(history_item(&from, &to, caller));
    entry["holder"] = to;
    save(kernel, store, &entry).await?;
    Ok(entry)
}

async fn status_reply(kernel: &Arc<Kernel>, store: &str, payload: &Value) -> Value {
    flat(status_inner(kernel, store, payload).await)
}

async fn status_inner(kernel: &Arc<Kernel>, store: &str, payload: &Value) -> Result<Value, Value> {
    let id = str_arg("status", payload, "id")?;
    let status = str_arg("status", payload, "status")?;
    state_arg("status", "status", status)?;
    let mut entry = load(kernel, store, Some(id))
        .await?
        .remove(id)
        .ok_or_else(|| not_found_task("status", id))?;
    entry["status"] = json!(status);
    save(kernel, store, &entry).await?;
    Ok(entry)
}

async fn report_reply(
    kernel: &Arc<Kernel>,
    store: &str,
    caller: &Option<AgentId>,
    payload: &Value,
) -> Value {
    flat(report_inner(kernel, store, caller, payload).await)
}

async fn report_inner(
    kernel: &Arc<Kernel>,
    store: &str,
    caller: &Option<AgentId>,
    payload: &Value,
) -> Result<Value, Value> {
    let id = str_arg("report", payload, "id")?;
    let state = opt_str("report", payload, "state")?;
    if let Some(s) = state {
        state_arg("report", "state", s)?;
    }
    let result = payload.get("result").cloned().unwrap_or(Value::Null);
    let from = match opt_str("report", payload, "from")? {
        Some(f) => Some(AgentId::from(f)),
        None => caller.clone(),
    };
    // the reporter is the caller or below it; the operator may report for anyone
    if let Some(who) = from.as_ref() {
        if agent_of(kernel, who).is_none() {
            return Err(refusal::refusal(
                refusal::NOT_FOUND,
                format!("report: {} is not an agent", py_repr(who.as_str())),
            ));
        }
        if let Some(c) = caller.as_ref() {
            if who != c && !ancestors(kernel, who).contains(c) {
                return Err(fail(
                    "not_in_subtree",
                    format!(
                        "report: {} is not {c} or one of its descendants",
                        py_repr(who.as_str())
                    ),
                ));
            }
        }
    }
    let mut entry = load(kernel, store, Some(id))
        .await?
        .remove(id)
        .ok_or_else(|| not_found_task("report", id))?;
    entry["report"] = json!({
        "from": from.as_ref().map(AgentId::as_str), "state": state,
        "result": result, "at": now(),
    });
    save(kernel, store, &entry).await?;
    Ok(entry)
}

async fn list_reply(kernel: &Arc<Kernel>, store: &str, payload: &Value) -> Value {
    flat(list_inner(kernel, store, payload).await)
}

async fn list_inner(kernel: &Arc<Kernel>, store: &str, payload: &Value) -> Result<Value, Value> {
    let parent = opt_str("list", payload, "parent")?;
    let status = opt_str("list", payload, "status")?;
    let holder = payload.get("holder").filter(|h| !h.is_null());
    let tasks = load(kernel, store, None).await?;
    let out: Vec<Value> = tasks
        .into_values()
        .filter(|t| parent.is_none_or(|p| t["parent"] == json!(p)))
        .filter(|t| status.is_none_or(|s| t["status"] == json!(s)))
        .filter(|t| holder.is_none_or(|h| &t["holder"] == h))
        .collect();
    Ok(json!({"tasks": out}))
}

async fn get_reply(kernel: &Arc<Kernel>, store: &str, payload: &Value) -> Value {
    flat(get_inner(kernel, store, payload).await)
}

async fn get_inner(kernel: &Arc<Kernel>, store: &str, payload: &Value) -> Result<Value, Value> {
    let id = str_arg("get", payload, "id")?;
    let tasks = load(kernel, store, None).await?;
    let mut entry = tasks
        .get(id)
        .cloned()
        .ok_or_else(|| not_found_task("get", id))?;
    let children: Vec<&str> = tasks
        .values()
        .filter(|t| t["parent"] == json!(id))
        .filter_map(|t| t["id"].as_str())
        .collect();
    entry["children"] = json!(children);
    Ok(entry)
}

#[cfg(test)]
mod tests;
