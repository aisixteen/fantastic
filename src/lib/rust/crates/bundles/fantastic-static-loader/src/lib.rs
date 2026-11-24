//! static_loader — a DECLARATIVE YAML roster loader.
//!
//! Mirror of the python canonical `static_loader.tools`. A `fantastic.yaml`
//! manifest declares the agents; on `boot` this loader reads it and hydrates
//! them — the root's `create_agent` / `update_agent`, sent AS THE LOADER — as its
//! own children, each carrying the `can` the manifest names (nothing is stamped).
//! The kernel knows nothing about YAML; this bundle owns the format.
//!
//! ONCE LOADED, A BUNDLE IS AN AGENT LIKE ANY OTHER: the pass is the loader's own
//! sends, checked against its own lines, whoever booted it (its create, a restart,
//! a `boot` verb). So a loader builds only what its record's `can` allows — a
//! create line on itself for its roster, and in writing every line the manifest
//! hands out — and an agent-created loader holds only what its creator could give.
//!
//! After boot the roster is CLOSED: deny-all means a roster agent cannot
//! `create_agent`, so the manifest IS the roster. Idempotent by id (a reboot
//! re-reads the manifest and RE-APPLIES the declared fields over an existing
//! agent), so the manifest and the persisted tree never duplicate. Every entry
//! MUST name an explicit `id` — an id-less one is refused loud, because nothing
//! could match it and each reboot would mint another agent beside the last.

use async_trait::async_trait;
use fantastic_bundle::{py_repr, py_repr_value};
use fantastic_file::fs;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use schemars::JsonSchema;
use serde::Deserialize;
use fantastic_kernel::{yaml_io, Agent, AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// The `handler_module` value that routes to this bundle.
pub const HANDLER_MODULE: &str = "static_loader.tools";

/// Hydrate the declared roster — kernel bookkeeping at boot.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

mod declared {
    use super::Boot;
    fantastic_kernel::verbs! { "boot" => Boot }
}

/// The fields a static_loader record carries.
#[derive(Debug, JsonSchema)]
pub struct LoaderRecord {
    /// The path of the YAML manifest that declares the roster.
    #[schemars(length(min = 1))]
    pub manifest: Option<String>,
}

fantastic_kernel::record!(LoaderRecord);

/// What the bundle is for — its one short man text.
const MAN: &str = "A declarative roster loader: at boot it reads the YAML manifest its record names and creates the agents it declares as its own children, re-applying the manifest over agents that already exist.";

/// Record field naming the manifest path.
const MANIFEST_FIELD: &str = "manifest";

/// The declarative roster loader bundle.
pub struct StaticLoaderBundle;

/// The raw `manifest` value off the record — reflect shows it as written.
fn manifest_meta(agent: &Agent) -> Value {
    Some(agent.record().meta)
        .and_then(|g| g.get(MANIFEST_FIELD).cloned())
        .unwrap_or(Value::Null)
}

/// Read + parse the manifest into the declared agent specs. Cold read, clamped
/// to the kernel's state folder `base` ([`Kernel::state_dir`]); a loader's
/// manifest is never read against the work folder — the same one-time
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
    // an empty document is an empty mapping, not a shape complaint
    let data = if data.is_null() { json!({}) } else { data };
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

/// One tree verb of the hydration (`create_agent`/`update_agent`), sent to the ROOT
/// as the loader itself: checked against its own lines.
async fn tree_verb(kernel: &Arc<Kernel>, payload: Value) -> Value {
    kernel
        .send(vec![fantastic_kernel::Item::new(
            fantastic_kernel::AgentId::from("kernel"),
            payload,
        )])
        .await
        .remove(0)
        .result
}

async fn boot(agent: &Arc<Agent>, kernel: &Arc<Kernel>) -> Value {
    let agents = match kernel
        .state_dir()
        .and_then(|base| load_manifest(agent, &base))
    {
        Ok(a) => a,
        Err(e) => return json!({ "error": e, "booted": false }),
    };
    let mut created: Vec<String> = Vec::new();
    let mut reapplied: Vec<String> = Vec::new();
    for spec in agents {
        // an entry without a handler_module is no entry; the value is judged by
        // `create_agent`.
        let handler_module = spec.as_object().and_then(|o| o.get("handler_module"));
        if !handler_module.is_some_and(|h| !h.is_null()) {
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
        if let Some(live) = kernel.resolve_target(id) {
            // Only the loader's OWN children are its roster: an id that lives
            // anywhere else is another agent's, never re-applied from here.
            if live.parent_id.as_ref() != Some(&agent.id) {
                return json!({
                    "error": format!(
                        "static_loader: {} exists outside this loader — a manifest \
                         re-applies only the agents it placed",
                        py_repr(id)
                    ),
                    "booted": false,
                });
            }
            // THE MANIFEST IS TRUTH; a store is only a MEDIUM (disk, RAM,
            // cloud, an API). So an id that already exists is RE-APPLIED
            // from the manifest, never skipped: an edited grant in the yaml
            // must take effect, never be silently overridden by the hydrated
            // record.
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
            if !patch.is_empty() {
                // the verb is set AFTER the declared fields: a declared `type` key can
                // never change which verb is called.
                let mut call = Map::new();
                call.insert("id".to_string(), json!(id));
                for (k, v) in patch {
                    call.insert(k, v);
                }
                call.insert("type".to_string(), json!("update_agent"));
                let r = tree_verb(kernel, Value::Object(call)).await;
                if let Some(e) = fantastic_kernel::refusal::error_text(&r) {
                    return json!({
                        "error": format!(
                            "static_loader: re-applying {}: {}",
                            py_repr(id),
                            e
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
        // `create_agent` is kernel-owned and answered by the ROOT alone. The
        // manifest's `can` rides as written (checked as lines the loader hands
        // out); the child boots inside create_agent.
        // The verb and `parent_id` land AFTER the spread: an entry cannot change the
        // verb, and one naming its own parent cannot place the agent outside the
        // loader's subtree.
        let mut payload = obj.clone();
        payload.insert("type".to_string(), json!("create_agent"));
        payload.insert("parent_id".to_string(), json!(agent.id.0));
        let rec = tree_verb(kernel, Value::Object(payload)).await;
        match rec.get("id").and_then(Value::as_str) {
            Some(id) => created.push(id.to_string()),
            None => {
                return json!({
                    "error": format!(
                        "static_loader: create failed for {}: {}",
                        py_repr_value(&spec),
                        fantastic_kernel::refusal::error_text(&rec).unwrap_or_else(|| rec.to_string())
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
        "sentence": MAN,
        // python `agent.display_name or id`: an EMPTY name is no name.
        "display_name": agent
            .display_name()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| agent.id.0.clone()),
        "loader": true,
        // The RAW record value, as python reports it — not filtered to a string.
        "manifest": manifest_meta(agent),
    })
}

#[async_trait]
impl Handler for StaticLoaderBundle {
    type Call = declared::Call;

    fn name(&self) -> &str {
        "static_loader"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        declared::VERBS
    }

    fn dynamic_verbs(&self, agent: &Agent) -> Vec<fantastic_kernel::DynVerb> {
        declared::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &RECORD
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

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let Some(agent) = kernel.resolve_target(agent_id.as_str()) else {
            return Ok(Some(json!({ "error": format!("no agent {agent_id}") })));
        };
        let reply = match call {
            declared::Call::Reflect(_) => reflect_reply(&agent),
            declared::Call::Boot(_) => boot(&agent, kernel).await,
        };
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests;
