//! WRITING AN AI AGENT's RECORD, from the set-up page.
//!
//! The page asks the human for the id, the handler (one of the backends this
//! build links, from the kernel's own bundle catalog), and then THE BACKEND'S
//! OWN FIELDS as its record schema declares them (`Bundle::record_schema`:
//! name, type, required, enum, the backend's default, a hint) — generic, so a
//! new backend needs no page change. Required fields start empty (nothing is
//! guessed); a field with a default starts at the BACKEND's value. The backend
//! validates at the create — one truth — and its refusal names every missing
//! field with its hint.
//!
//! Grants are two-sided and MERGED, never replaced: the human may call the new
//! agent, the new agent may call each target it names, and each target's
//! `callable_by` gains the new agent. Its conversation goes to the kernel's
//! memory store (the root's `memory_store_id`), wired the same way both sides.

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// Where a new agent goes: beside the human's own agents.
pub const PARENT: &str = "set_of_agents";

/// Fields the WRITE sets itself — never asked for on the page.
pub const WRITE_OWNED: [&str; 1] = ["memory_store_id"];

/// One field of a backend's record schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: String,
    pub required: bool,
    pub one_of: Vec<String>,
    pub default: Option<Value>,
    pub hint: String,
}

/// A linked AI backend and what its record takes.
#[derive(Debug, Clone, PartialEq)]
pub struct Backend {
    pub module: String,
    /// The page's fields (the write-owned ones left out), in schema order.
    pub fields: Vec<Field>,
    /// The variable its key is read from, when it takes one.
    pub key_var: Option<String>,
}

impl Backend {
    /// A backend from its catalog schema.
    pub fn from_schema(module: &str, schema: &Value) -> Self {
        let fields = schema["fields"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| {
                        let name = f["name"].as_str()?.to_string();
                        (!WRITE_OWNED.contains(&name.as_str())).then(|| Field {
                            name,
                            ty: f["type"].as_str().unwrap_or("str").to_string(),
                            required: f["required"].as_bool().unwrap_or(false),
                            one_of: f["enum"]
                                .as_array()
                                .map(|e| e.iter().filter_map(Value::as_str).map(str::to_string).collect())
                                .unwrap_or_default(),
                            default: f.get("default").cloned().filter(|v| !v.is_null()),
                            hint: f["hint"].as_str().unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { module: module.to_string(), fields, key_var: schema["key_var"].as_str().map(str::to_string) }
    }
}

/// The backends this build links — every registered module with a record
/// schema that ends `_backend.tools` — with their schemas.
pub fn backends(host: &Host) -> Vec<Backend> {
    let mut v: Vec<Backend> = host
        .kernel
        .bundles
        .catalog()
        .filter(|(m, _)| m.ends_with("_backend.tools"))
        .filter_map(|(m, b)| b.record_schema().map(|s| Backend::from_schema(m, &s)))
        .collect();
    v.sort_by(|a, b| a.module.cmp(&b.module));
    v
}

/// A field's text as the page holds it → the record's value, typed by the
/// schema. Empty text = not set.
pub fn coerce(f: &Field, text: &str) -> Result<Option<Value>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let bad = |what: &str| format!("{}: `{t}` is not {what}", f.name);
    Ok(Some(match f.ty.as_str() {
        "int" => json!(t.parse::<i64>().map_err(|_| bad("an integer"))?),
        "float" => json!(t.parse::<f64>().map_err(|_| bad("a number"))?),
        "bool" => json!(match t {
            "true" | "yes" | "1" => true,
            "false" | "no" | "0" => false,
            _ => return Err(bad("true or false")),
        }),
        "enum" if !f.one_of.iter().any(|o| o == t) => return Err(bad(&format!("one of {}", f.one_of.join("|")))),
        "list" => json!(t.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()).collect::<Vec<_>>()),
        "map" => serde_json::from_str::<Value>(t).ok().filter(Value::is_object).ok_or_else(|| bad("a JSON object"))?,
        _ => json!(t),
    }))
}

/// A value as the page shows it (the inverse of [`coerce`]).
pub fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_string)).collect::<Vec<_>>().join(", "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The record the page wants written.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub id: String,
    pub handler: String,
    /// The backend's own fields that are set, already typed.
    pub fields: Map<String, Value>,
    pub may_call: Vec<String>,
}

impl Record {
    /// What the PAGE must have before a write: the id and the handler. The
    /// backend's own required fields are the backend's to refuse, by name.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.id.trim().is_empty() {
            m.push("id");
        }
        if self.handler.is_empty() {
            m.push("handler");
        }
        m
    }
}

fn err_of(r: &fantastic_kernel::Outcome) -> Option<String> {
    r.result.get("error").and_then(Value::as_str).map(|e| {
        match r.result.get("reason").and_then(Value::as_str) {
            Some(why) => format!("{e} — {why}"),
            None => e.to_string(),
        }
    })
}

fn grants(host: &Host, id: &str, field: &str) -> Map<String, Value> {
    host.kernel
        .agents
        .get(&fantastic_kernel::AgentId::from(id))
        .and_then(|a| a.value().record().meta.get(field).and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

async fn merge(host: &Host, id: &str, field: &str, add: &str) -> Result<(), String> {
    let mut g = grants(host, id, field);
    g.insert(add.into(), json!("*"));
    let r = host.operator("kernel", json!({"type": "update_agent", "id": id, field: g})).await;
    match err_of(&r) {
        Some(e) => Err(format!("could not grant {id}.{field} += {add}: {e}")),
        None => Ok(()),
    }
}

/// Create, grant and boot. Returns where the record was written.
pub async fn write(host: &Host, rec: &Record) -> Result<String, String> {
    let missing = rec.missing();
    if !missing.is_empty() {
        return Err(format!("missing: {}", missing.join(", ")));
    }
    let id = rec.id.trim();
    if host.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(id)) {
        return Err(format!("`{id}` already exists"));
    }
    for t in &rec.may_call {
        if !host.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(t.as_str())) {
            return Err(format!("may call: no agent `{t}`"));
        }
    }
    let user = host.cfg.user.clone();
    // Its conversation lives in the kernel's memory store, both sides granted.
    let store = host.admit_to_memory(id).await?;
    let mut can_call: Map<String, Value> = rec.may_call.iter().map(|t| (t.clone(), json!("*"))).collect();
    can_call.insert(store.clone(), json!("*"));
    let mut create = rec.fields.clone();
    create.extend(
        json!({
            "type": "create_agent",
            "parent_id": PARENT,
            "id": id,
            "handler_module": rec.handler,
            "callable_by": {user.as_str(): "*"},
            "can_call": can_call,
            "memory_store_id": store,
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    let create = Value::Object(create);
    let r = host.operator("kernel", create).await;
    if let Some(e) = err_of(&r) {
        return Err(e);
    }
    merge(host, &user, "can_call", id).await?;
    for t in &rec.may_call {
        merge(host, t, "callable_by", id).await?;
    }
    let r = host.operator(id, json!({"type": "boot"})).await;
    if let Some(e) = err_of(&r) {
        return Err(format!("`{id}` was written but did not boot: {e}"));
    }
    Ok(format!(".fantastic/agents/{PARENT}/agents/{id}/agent.yaml"))
}
