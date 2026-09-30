//! WRITING AN AI AGENT's RECORD, from the set-up page.
//!
//! Five fields, all from the human: the id, the handler (one of the backends
//! this build links, from the kernel's own bundle catalog), the model (as the
//! backend names it), the endpoint (every backend requires it; none has its
//! own) and who it may call. Nothing is guessed or defaulted: a
//! missing field is refused here, and a field the backend itself requires is
//! refused by the backend's own `validate_meta`, with its words.
//!
//! Grants are two-sided and MERGED, never replaced: the human may call the new
//! agent, the new agent may call each target it names, and each target's
//! `callable_by` gains the new agent. Its conversation goes to the kernel's
//! memory store (the root's `memory_store_id`), wired the same way both sides.

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// Where a new agent goes: beside the human's own agents.
pub const PARENT: &str = "set_of_agents";

/// The backends this build links: registered modules that are AI backends.
pub fn backends(host: &Host) -> Vec<String> {
    let mut v: Vec<String> = host
        .kernel
        .bundles
        .catalog()
        .map(|(m, _)| m.to_string())
        .filter(|m| m.ends_with("_backend.tools"))
        .collect();
    v.sort();
    v
}

/// The record the page wants written.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub id: String,
    pub handler: String,
    pub model: String,
    pub endpoint: String,
    pub may_call: Vec<String>,
}

impl Record {
    /// What is still missing, by field name. Empty means writable.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.id.trim().is_empty() {
            m.push("id");
        }
        if self.handler.is_empty() {
            m.push("handler");
        }
        if self.model.trim().is_empty() {
            m.push("model");
        }
        if self.endpoint.trim().is_empty() {
            m.push("endpoint");
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
    let create = json!({
        "type": "create_agent",
        "parent_id": PARENT,
        "id": id,
        "handler_module": rec.handler,
        "model": rec.model.trim(),
        "endpoint": rec.endpoint.trim(),
        "callable_by": {user.as_str(): "*"},
        "can_call": can_call,
        "memory_store_id": store,
    });
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
