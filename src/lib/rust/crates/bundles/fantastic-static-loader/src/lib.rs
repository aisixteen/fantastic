//! static_loader — a DECLARATIVE YAML roster loader.
//!
//! Mirror of the python canonical `static_loader.tools`. A `fantastic.yaml`
//! manifest declares the agents; on `boot` this loader reads it and hydrates
//! them AS OPERATOR (via [`fantastic_kernel::with_operator`] + the normal
//! `create_agent` path), as its own children — each carrying the Unix baseline
//! grant plus any explicit `can_call` the manifest names. The kernel knows
//! nothing about YAML; this bundle owns the format.
//!
//! After boot the roster is CLOSED: deny-all means a roster agent cannot
//! `create_agent`, so the manifest IS the roster. Idempotent by id (a reboot
//! re-reads + skips agents that already exist), so the manifest and the persisted
//! tree never duplicate. Declare explicit ids.

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{with_operator, Agent, AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// The `handler_module` value that routes to this bundle.
pub const HANDLER_MODULE: &str = "static_loader.tools";

const README: &str = include_str!("readme.md");

/// Record field naming the manifest path.
const MANIFEST_FIELD: &str = "manifest";

/// The declarative roster loader bundle.
pub struct StaticLoaderBundle;

fn manifest_path(agent: &Agent) -> Option<String> {
    agent.meta.read().ok().and_then(|g| {
        g.get(MANIFEST_FIELD)
            .and_then(Value::as_str)
            .map(String::from)
    })
}

/// Read + parse the manifest into the declared agent specs. Cold read, relative
/// to cwd (the CLI passes a cwd-relative path) — the one-time bring-up path, like
/// the persistence cold reads.
fn load_manifest(agent: &Agent) -> Result<Vec<Value>, String> {
    let Some(path) = manifest_path(agent) else {
        return Err(format!(
            "static_loader: no `{MANIFEST_FIELD}` path on the record"
        ));
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("static_loader: cannot read manifest {path:?}: {e}"))?;
    let data: Value = serde_yaml::from_str(&text)
        .map_err(|e| format!("static_loader: manifest {path:?} is not valid YAML: {e}"))?;
    let Some(obj) = data.as_object() else {
        return Err(format!(
            "static_loader: manifest {path:?} must be a mapping with `agents:`"
        ));
    };
    match obj.get("agents") {
        None => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(_) => Err(format!(
            "static_loader: `agents:` in {path:?} must be a list"
        )),
    }
}

async fn boot(agent: &Arc<Agent>, kernel: &Arc<Kernel>) -> Value {
    let agents = match load_manifest(agent) {
        Ok(a) => a,
        Err(e) => return json!({ "error": e, "booted": false }),
    };
    let mut created: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for spec in agents {
        let Some(obj) = spec.as_object() else {
            return json!({
                "error": format!("static_loader: each roster entry needs a handler_module: {spec}"),
                "booted": false,
            });
        };
        if obj.get("handler_module").and_then(Value::as_str).is_none() {
            return json!({
                "error": format!("static_loader: each roster entry needs a handler_module: {spec}"),
                "booted": false,
            });
        }
        if let Some(id) = obj.get("id").and_then(Value::as_str) {
            if kernel.agents.contains_key(&AgentId::from(id)) {
                skipped.push(id.to_string());
                continue;
            }
        }
        // create_agent to SELF → the declared agent becomes THIS loader's child
        // ($parent = the loader). Baseline grant is stamped + the manifest's
        // explicit `can_call` merges on top; the child boots inside create_agent.
        // Operator authority: the loader hydrating the roster it was told to build.
        let mut payload = obj.clone();
        payload.insert("type".to_string(), json!("create_agent"));
        let rec = with_operator(kernel.send(&agent.id, Value::Object(payload))).await;
        match rec.get("id").and_then(Value::as_str) {
            Some(id) => created.push(id.to_string()),
            None => {
                return json!({
                    "error": format!("static_loader: create failed for {spec}: {rec}"),
                    "booted": false,
                })
            }
        }
    }
    json!({ "booted": true, "created": created, "skipped": skipped })
}

fn reflect_reply(agent: &Agent) -> Value {
    json!({
        "id": agent.id.0,
        "sentence": "declarative YAML roster loader",
        "display_name": agent.display_name().unwrap_or_else(|| agent.id.0.clone()),
        "loader": true,
        "manifest": manifest_path(agent),
        "verbs": {
            "reflect": "No args. The loader's posture: manifest + roster-hydration.",
            "boot": "No args. Read the manifest + hydrate the declared roster (operator).",
        },
    })
}

#[async_trait]
impl Bundle for StaticLoaderBundle {
    fn name(&self) -> &str {
        "static_loader"
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
        let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
            return Ok(Some(json!({ "error": format!("no agent {agent_id}") })));
        };
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => reflect_reply(&agent),
            "boot" => boot(&agent, kernel).await,
            "shutdown" => Value::Null,
            other => json!({ "error": format!("static_loader: unknown type {other:?}") }),
        };
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests;
