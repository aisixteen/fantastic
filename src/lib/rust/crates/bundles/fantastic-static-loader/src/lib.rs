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
use fantastic_bundle::{py_repr, py_repr_value, py_str, py_truthy};
use fantastic_file::fs;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{refusal, UNHANDLED};
use fantastic_kernel::{with_operator, yaml_io, Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// The `handler_module` value that routes to this bundle.
pub const HANDLER_MODULE: &str = "static_loader.tools";

const README: &str = include_str!("readme.md");

/// Record field naming the manifest path.
const MANIFEST_FIELD: &str = "manifest";

/// The declarative roster loader bundle.
pub struct StaticLoaderBundle;

/// The raw `manifest` value off the record — reflect shows it as written.
fn manifest_meta(agent: &Agent) -> Value {
    agent
        .meta
        .read()
        .ok()
        .and_then(|g| g.get(MANIFEST_FIELD).cloned())
        .unwrap_or(Value::Null)
}

/// Read + parse the manifest into the declared agent specs. Cold read, clamped
/// to the kernel's running dir `base` ([`Kernel::workdir`]) — the same one-time
/// bring-up path as the root loader's tree read (before any file_bridge agent
/// can exist), and through the SAME disk surface: `fantastic_file::fs`, as
/// python imports `file_bridge.fs`.
fn load_manifest(agent: &Agent, base: &std::path::Path) -> Result<Vec<Value>, String> {
    let raw = manifest_meta(agent);
    let path = match raw.as_str() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => {
            return Err(format!(
                "static_loader: no {} path on the record",
                py_repr(MANIFEST_FIELD)
            ))
        }
    };
    let text = fs::read_text(base, &path).map_err(|e| {
        format!(
            "static_loader: cannot read manifest {}: {e}",
            py_repr(&path)
        )
    })?;
    // python `yaml.safe_load` — PyYAML's YAML 1.1, the kernel's one reader.
    let data = yaml_io::load(&text).map_err(|e| {
        format!(
            "static_loader: manifest {} is not valid YAML: {e}",
            py_repr(&path)
        )
    })?;
    // `yaml.safe_load(text) or {}`: a python-FALSY document (empty, `~`, `0`,
    // `false`, `[]`, `{}`) is an empty mapping, not a shape complaint.
    let data = if py_truthy(&data) { data } else { json!({}) };
    let Some(obj) = data.as_object() else {
        return Err(format!(
            "static_loader: manifest {} must be a mapping with `agents:`",
            py_repr(&path)
        ));
    };
    match obj.get("agents") {
        None => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(_) => Err(format!(
            "static_loader: `agents:` in {} must be a list",
            py_repr(&path)
        )),
    }
}

async fn boot(agent: &Arc<Agent>, kernel: &Arc<Kernel>) -> Value {
    let agents = match kernel
        .workdir()
        .and_then(|base| load_manifest(agent, &base))
    {
        Ok(a) => a,
        Err(e) => return json!({ "error": e, "booted": false }),
    };
    let mut created: Vec<String> = Vec::new();
    let mut reapplied: Vec<String> = Vec::new();
    for spec in agents {
        // python: `not isinstance(spec, dict) or not spec.get("handler_module")`
        // — a FALSY handler_module is as absent as none at all; any truthy value
        // passes here and is judged by `create_agent`.
        let handler_module = spec.as_object().and_then(|o| o.get("handler_module"));
        if !handler_module.is_some_and(py_truthy) {
            return json!({
                "error": format!(
                    "static_loader: each roster entry needs a handler_module: {}",
                    py_repr_value(&spec)
                ),
                "booted": false,
            });
        }
        let obj = spec
            .as_object()
            .expect("a spec with a handler_module is a mapping");
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
                "error": format!(
                    "static_loader: roster entry needs an explicit id: {}",
                    py_repr_value(&spec)
                ),
                "booted": false,
            });
        };
        if let Some(live) = kernel.agents.get(&AgentId::from(id)) {
            // THE MANIFEST IS TRUTH; a store is only a MEDIUM (disk, RAM,
            // cloud, an API). So an id that already exists is RE-APPLIED
            // from the manifest, never skipped: editing a grant in the yaml
            // used to change nothing at all, because the hydrated record
            // silently won and the operator got no word about it.
            let declared_hm = obj.get("handler_module").unwrap_or(&Value::Null);
            if live.handler_module.as_deref() != declared_hm.as_str() {
                // A bundle cannot be swapped under a live agent (its
                // in-process state belongs to the old one).
                return json!({
                    "error": format!(
                        "static_loader: {} exists as {} but the manifest declares \
                         {} — a handler_module is fixed at create; delete the agent \
                         to re-declare it",
                        py_repr(id),
                        live.handler_module.as_deref().map_or("None".to_string(), py_repr),
                        py_repr_value(declared_hm)
                    ),
                    "booted": false,
                });
            }
            drop(live);
            let mut patch = obj.clone();
            patch.shift_remove("id");
            patch.shift_remove("handler_module");
            patch.shift_remove("parent_id");
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
                // python spreads `**declared` LAST: a declared `type` key
                // overrides the verb, and the verb keeps its leading position.
                let mut call = Map::new();
                call.insert("type".to_string(), json!("update_agent"));
                call.insert("id".to_string(), json!(id));
                for (k, v) in patch {
                    call.insert(k, v);
                }
                let r = with_operator(kernel.send(vec![Item::new(
                    AgentId::from("kernel"),
                    Value::Object(call),
                )]))
                .await
                .remove(0)
                .result;
                // Only a TRUTHY error is a failure — python tests `r.get("error")`.
                if let Some(e) = r.get("error").filter(|e| py_truthy(e)) {
                    return json!({
                        "error": format!(
                            "static_loader: re-applying {}: {}",
                            py_repr(id),
                            py_str(e)
                        ),
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
        // python: `{"type": "create_agent", **spec, "parent_id": agent.id}` — the
        // verb leads (a spec `type` key overrides its VALUE, in place) and
        // `parent_id` lands AFTER the spread, so an entry naming its own parent
        // cannot place the agent outside the loader's subtree.
        let mut payload = Map::new();
        payload.insert("type".to_string(), json!("create_agent"));
        for (k, v) in obj.clone() {
            payload.insert(k, v);
        }
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
                    "error": format!(
                        "static_loader: create failed for {}: {}",
                        py_repr_value(&spec),
                        py_str(&rec)
                    ),
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
        // python `agent.display_name or id`: an EMPTY name is no name.
        "display_name": agent
            .display_name()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| agent.id.0.clone()),
        "loader": true,
        // The RAW record value, as python reports it — not filtered to a string.
        "manifest": manifest_meta(agent),
        // python's first docstring lines, in VERBS order.
        "verbs": {
            "reflect": "The loader's posture: what manifest it hydrates, and that it IS a loader",
            "boot": "Hydrate the declared roster as OPERATOR.",
        },
    })
}

#[async_trait]
impl Bundle for StaticLoaderBundle {
    fn name(&self) -> &str {
        "static_loader"
    }

    /// The manifest's SHAPE at write time — a NAMED manifest must be a non-empty
    /// path string. Merged view (record + patch): a record may exist without a
    /// manifest yet. Whether the file is readable is `boot`'s question: it reads
    /// through `Kernel::workdir`, which a write-time check is never handed.
    fn validate_meta(
        &self,
        meta: &serde_json::Map<String, Value>,
        record: &serde_json::Map<String, Value>,
    ) -> Option<String> {
        let raw = meta
            .get(MANIFEST_FIELD)
            .or_else(|| record.get(MANIFEST_FIELD))?;
        match raw.as_str() {
            Some(p) if !p.is_empty() => None,
            _ => Some(format!(
                "static_loader: {MANIFEST_FIELD} must be a non-empty path string"
            )),
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
            // Every other verb — `shutdown` included, which this bundle does not
            // declare — is unhandled, in the one refusal shape.
            _ => {
                let raw = payload.get("type").cloned().unwrap_or(Value::Null);
                let mut v = refusal(
                    UNHANDLED,
                    format!("static_loader: unknown type {}", py_repr_value(&raw)),
                );
                fantastic_kernel::refusal::with(&mut v, "verb", raw);
                v
            }
        };
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests;
