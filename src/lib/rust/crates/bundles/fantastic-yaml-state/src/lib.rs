//! `yaml_state` — a durable YAML key-value memory agent.
//!
//! One bundle, instantiated as N agents. The `mode` meta ("mem" | "data")
//! sets the *discipline* (and the reflect sentence) — the verbs are
//! identical:
//!
//! - data → current scratch-state, overwrite-in-place.
//! - mem  → durable keyed facts, accrete + prune at LLM discretion.
//!
//! ALL disk IO goes THROUGH a `file_bridge` AGENT (the gated fs edge — sealed /
//! deny-all by default) that the LOADER discovers — this bundle never names a
//! This bundle owns NO disk surface of its own and never touches `std::fs`: it
//! provider and never computes a path. It `send`s `persist_blob` / `load_blob` to
//! (and rust's `ai_core`). `set` / `delete` / `replace` **failfast** until
//! the kernel, exactly like the Python bundle (and surfaces a denied write rather
//! than losing it).
//! Wire it to the **`.fantastic` store** (the one the loader persists records
//! through — ONE file_bridge serves both): the path is store-relative
//! `agents/<id>/state.yaml`, so the sidecar lands next to its `agent.json`.
//! Disk-is-truth (read fresh each call, no cache).
//!
//! Keys are flat namespaced strings (dotted convention:
//! `domain.subject.attribute`). Values are arbitrary JSON. Mirrors the
//! canonical Python `yaml_state` bundle.

use async_trait::async_trait;
use fantastic_bundle as _; // keep the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const README: &str = include_str!("readme.md");

const MEM_SENTENCE: &str = "Your durable memory. Facts you must remember across sessions live here — auto-loaded into your context on boot. `set` a descriptive key the moment the user tells you something worth keeping (a name, a preference, a decision). Your current facts are already in your context — read them, don't re-fetch.";
const DATA_SENTENCE: &str = "Your durable scratch-state (component state, config, run params, current selection). One value per key, overwrite-in-place; auto-loaded into your context on boot.";

pub struct YamlStateBundle;

/// The bound file_bridge provider id, off this agent's record.
/// `state.yaml` in the agent's own dir, RELATIVE to the provider's root (the
/// `.fantastic` store) — `agents/<id>/state.yaml`, landing next to its
/// `agent.json` (no `.fantastic/.fantastic/...` double-nest). Matches Python.
/// The sidecar this agent keeps. The LOADER owns where it lands — this bundle
/// names only the file.
const STATE_BLOB: &str = "state.yaml";

fn mode_of(agent: &Agent) -> String {
    let m = agent
        .meta
        .read()
        .ok()
        .and_then(|g| g.get("mode").and_then(Value::as_str).map(str::to_string));
    match m.as_deref() {
        Some("mem") => "mem".to_string(),
        _ => "data".to_string(),
    }
}

/// Read the store THROUGH the wired provider. Unwired / missing / denied ⇒ {}.
async fn load(agent: &Agent, kernel: &Arc<Kernel>) -> Map<String, Value> {
    // THROUGH THE LOADER, not a provider of our own. The consumer names ITSELF
    // and a filename; the loader owns the path and the store. Operator
    // authority: reading our OWN sidecar is bookkeeping processed from above,
    // not this agent reaching out — so it never runs against our grant.
    let r = fantastic_kernel::send::with_operator(kernel.send(vec![Item::new(
        AgentId::from("kernel"),
        json!({
            "type": "load_blob",
            "agent_id": agent.id.0,
            "name": STATE_BLOB,
        }),
    )]))
    .await
    .remove(0)
    .result;
    let Some(text) = r.get("content").and_then(Value::as_str) else {
        return Map::new();
    };
    if text.trim().is_empty() {
        return Map::new();
    }
    match serde_yaml::from_str::<Value>(text) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn emit_yaml(doc: &Map<String, Value>) -> String {
    if doc.is_empty() {
        return String::new();
    }
    // Sorted keys (BTreeMap) — deterministic, matches the Python agent.
    let sorted: BTreeMap<&String, &Value> = doc.iter().collect();
    serde_yaml::to_string(&sorted).unwrap_or_default()
}

/// Write the store THROUGH the provider; surface a denied/failed write as an
/// error (no silent loss). Returns an error `Value`, or `None` on success. Error
/// text is byte-identical to the Python bundle.
async fn persist(
    agent: &Agent,
    kernel: &Arc<Kernel>,
    doc: &Map<String, Value>,
    verb: &str,
) -> Option<Value> {
    // Operator authority: persisting from above (see `load`).
    let w = fantastic_kernel::send::with_operator(kernel.send(vec![Item::new(
        AgentId::from("kernel"),
        json!({
            "type": "persist_blob",
            "agent_id": agent.id.0,
            "name": STATE_BLOB,
            "content": emit_yaml(doc),
        }),
    )]))
    .await
    .remove(0)
    .result;
    if !w.is_object() {
        return Some(json!({"error": format!("yaml_state.{verb}: loader gave no reply")}));
    }
    if let Some(err) = w.get("error").and_then(Value::as_str) {
        let mut out = json!({ "error": format!("yaml_state.{verb}: {err}") });
        // Carry the loader's machine field + recipe through — a sealed-edge
        // denial must stay a denial, with the open-it hint intact.
        if let Some(reason) = w.get("reason") {
            out["reason"] = reason.clone();
        }
        if let Some(hint) = w.get("hint") {
            out["hint"] = hint.clone();
        }
        return Some(out);
    }
    None
}

fn value_size(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        other => other.to_string().chars().count(),
    }
}

async fn reflect_reply(agent: &Agent, kernel: &Arc<Kernel>) -> Value {
    let mode = mode_of(agent);
    let doc = load(agent, kernel).await;
    json!({
        "id": agent.id.0,
        "sentence": if mode == "mem" { MEM_SENTENCE } else { DATA_SENTENCE },
        "mode": mode,
        "key_count": doc.len(),
        "verbs": {
            "read": "args: key?:str. Value at key (null if absent); whole doc if key omitted.",
            "keys": "args: none. List keys + value sizes — the table-of-contents.",
            "set": "args: key:str, value:any. Upsert one key. Persisted through the kernel; a write with no store wired FAILS rather than being lost.",
            "delete": "args: key:str. Remove a key.",
            "replace": "args: doc:object. Overwrite the whole store ({} clears).",
            "state_yaml": "args: none. The entire store as YAML text.",
        },
    })
}

#[async_trait]
impl Bundle for YamlStateBundle {
    fn name(&self) -> &str {
        "yaml_state"
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
            return Ok(Some(json!({"error": format!("no agent {agent_id}")})));
        };
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => reflect_reply(&agent, kernel).await,
            "boot" | "shutdown" => Value::Null,
            "read" => {
                let doc = load(&agent, kernel).await;
                match payload.get("key").and_then(Value::as_str) {
                    Some(k) => {
                        json!({"key": k, "value": doc.get(k).cloned().unwrap_or(Value::Null)})
                    }
                    None => json!({ "doc": Value::Object(doc) }),
                }
            }
            "keys" => {
                let doc = load(&agent, kernel).await;
                let mut keys: Vec<Value> = doc
                    .iter()
                    .map(|(k, v)| json!({"key": k, "size": value_size(v)}))
                    .collect();
                keys.sort_by(|a, b| {
                    a["key"]
                        .as_str()
                        .unwrap_or("")
                        .cmp(b["key"].as_str().unwrap_or(""))
                });
                json!({ "keys": keys })
            }
            "set" => {
                let Some(key) = payload
                    .get("key")
                    .and_then(Value::as_str)
                    .filter(|k| !k.is_empty())
                else {
                    return Ok(Some(
                        json!({"error": "yaml_state.set: key (non-empty str) required"}),
                    ));
                };
                let Some(value) = payload.get("value") else {
                    return Ok(Some(json!({"error": "yaml_state.set: value required"})));
                };
                let mut doc = load(&agent, kernel).await;
                doc.insert(key.to_string(), value.clone());
                if let Some(err) = persist(&agent, kernel, &doc, "set").await {
                    return Ok(Some(err));
                }
                json!({"key": key, "set": true})
            }
            "delete" => {
                let Some(key) = payload
                    .get("key")
                    .and_then(Value::as_str)
                    .filter(|k| !k.is_empty())
                else {
                    return Ok(Some(
                        json!({"error": "yaml_state.delete: key (non-empty str) required"}),
                    ));
                };
                let mut doc = load(&agent, kernel).await;
                let existed = doc.remove(key).is_some();
                if let Some(err) = persist(&agent, kernel, &doc, "delete").await {
                    return Ok(Some(err));
                }
                json!({"key": key, "deleted": existed})
            }
            "replace" => {
                // `doc` is REQUIRED ({} clears) — a missing doc is an error, not
                // a silent clear (matches Python).
                let doc = match payload.get("doc") {
                    Some(Value::Object(m)) => m.clone(),
                    None => {
                        return Ok(Some(
                            json!({"error": "yaml_state.replace: doc (object) required"}),
                        ))
                    }
                    _ => {
                        return Ok(Some(
                            json!({"error": "yaml_state.replace: doc must be an object"}),
                        ))
                    }
                };
                let n = doc.len();
                if let Some(err) = persist(&agent, kernel, &doc, "replace").await {
                    return Ok(Some(err));
                }
                json!({"replaced": true, "keys": n})
            }
            "state_yaml" => json!({ "yaml": emit_yaml(&load(&agent, kernel).await) }),
            other => json!({"error": format!("yaml_state: unknown type '{other}'")}),
        };
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests;
