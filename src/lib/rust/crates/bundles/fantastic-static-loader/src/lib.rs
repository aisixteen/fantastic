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
//! re-reads the manifest and RE-APPLIES the declared fields over an existing
//! agent), so the manifest and the persisted tree never duplicate. Every entry
//! MUST name an explicit `id` — an id-less one is refused loud, because nothing
//! could match it and each reboot would mint another agent beside the last.

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{with_operator, Agent, AgentId, Item, Kernel};
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
    // CLAMPED to the running dir. A cold read is still a read: `fs` is the one
    // disk surface, and a manifest path pointing outside the kernel's own dir
    // refuses here rather than being opened because this happens at bring-up.
    let cwd = std::env::current_dir().unwrap_or_default();
    let clamped = fantastic_file::clamp_root(std::path::Path::new(&path), &cwd)
        .map_err(|e| format!("static_loader: manifest {path:?} is not readable: {e}"))?;
    let text = std::fs::read_to_string(&clamped)
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
    let mut reapplied: Vec<String> = Vec::new();
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
        // An id-less entry cannot be idempotent: the roster is keyed by id, so
        // nothing matches the agent the LAST boot minted and this boot mints a
        // SECOND one, which the flush persists — the roster grows by one agent
        // per restart and the operator is never told. The manifest is truth, so
        // it must name its agents.
        let Some(id) = obj
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return json!({
                "error": format!("static_loader: roster entry needs an explicit id: {spec}"),
                "booted": false,
            });
        };
        if let Some(live) = kernel.agents.get(&AgentId::from(id)) {
            // THE MANIFEST IS TRUTH; a store is only a MEDIUM (disk, RAM,
            // cloud, an API). So an id that already exists is RE-APPLIED
            // from the manifest, never skipped: editing a grant in the yaml
            // used to change nothing at all, because the hydrated record
            // silently won and the operator got no word about it.
            let declared_hm = obj.get("handler_module").and_then(Value::as_str);
            if live.handler_module.as_deref() != declared_hm {
                // A bundle cannot be swapped under a live agent (its
                // in-process state belongs to the old one).
                return json!({
                    "error": format!(
                        "static_loader: {id:?} exists as {:?} but the manifest \
                         declares {declared_hm:?} — a handler_module is fixed at \
                         create; delete the agent to re-declare it",
                        live.handler_module
                    ),
                    "booted": false,
                });
            }
            drop(live);
            let mut patch = obj.clone();
            patch.remove("id");
            patch.remove("handler_module");
            patch.remove("parent_id");
            // `update_agent` REPLACES a named field, so a bare re-apply of
            // `can_call` would delete the structural baseline the kernel
            // stamps at create and sever the agent from its own parent and
            // children. Merge over the baseline, as `create_agent` does.
            for (field, base) in [
                ("can_call", fantastic_kernel::authz::default_can_call()),
                (
                    "callable_by",
                    fantastic_kernel::authz::default_callable_by(),
                ),
            ] {
                if let Some(Value::Object(declared)) = patch.get(field).cloned() {
                    let mut merged = base;
                    for (k, v) in declared {
                        merged.insert(k, v);
                    }
                    patch.insert(field.to_string(), Value::Object(merged));
                }
            }
            if !patch.is_empty() {
                patch.insert("type".to_string(), json!("update_agent"));
                patch.insert("id".to_string(), json!(id));
                let r = with_operator(kernel.send(vec![Item::new(
                    AgentId::from("kernel"),
                    Value::Object(patch),
                )]))
                .await
                .remove(0)
                .result;
                if let Some(e) = r.get("error") {
                    return json!({
                        "error": format!("static_loader: re-applying {id:?}: {e}"),
                        "booted": false,
                    });
                }
            }
            reapplied.push(id.to_string());
            continue;
        }
        // The declared agent becomes THIS loader's child ($parent = the loader),
        // but placement is stated by `parent_id`, not by who is addressed:
        // `create_agent` is kernel-owned and answered by the ROOT alone. Baseline
        // grant is stamped + the manifest's explicit `can_call` merges on top;
        // the child boots inside create_agent. Operator authority: the loader
        // hydrating the roster it was told to build.
        let mut payload = obj.clone();
        payload.insert("type".to_string(), json!("create_agent"));
        payload.insert("parent_id".to_string(), json!(agent.id.0));
        let rec = with_operator(kernel.send(vec![Item::new(
            AgentId::from("kernel"),
            Value::Object(payload),
        )]))
        .await
        .remove(0)
        .result;
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
    json!({ "booted": true, "created": created, "reapplied": reapplied })
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

    /// Refuse a manifest that is absent or unreadable at WRITE time, so a broken
    /// roster never becomes a booting record. Merged view (record + patch): a
    /// record may exist without a manifest yet — `boot` reports that — but a
    /// manifest that is NAMED must be readable now.
    fn validate_meta(
        &self,
        meta: &serde_json::Map<String, Value>,
        record: &serde_json::Map<String, Value>,
    ) -> Option<String> {
        let mut merged = record.clone();
        for (k, v) in meta {
            merged.insert(k.clone(), v.clone());
        }
        let raw = merged.get(MANIFEST_FIELD)?;
        let path = match raw.as_str() {
            Some(p) if !p.is_empty() => p,
            _ => {
                return Some(format!(
                    "static_loader: {MANIFEST_FIELD} must be a non-empty path string"
                ))
            }
        };
        let cwd = std::env::current_dir().unwrap_or_default();
        match fantastic_file::clamp_root(std::path::Path::new(path), &cwd) {
            Err(e) => Some(format!(
                "static_loader: manifest {path:?} is not readable: {e}"
            )),
            Ok(clamped) => std::fs::read_to_string(&clamped)
                .err()
                .map(|e| format!("static_loader: manifest {path:?} is not readable: {e}")),
        }
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
