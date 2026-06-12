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
//! deny-all by default), referenced by `file_bridge_id` on this agent's record.
//! This bundle owns NO disk surface of its own and never touches `std::fs`: it
//! `send`s `read` / `write` verbs to its provider, exactly like the Python bundle
//! (and rust's `ai_core`). `set` / `delete` / `replace` **failfast** until
//! `file_bridge_id` is set (and surface a denied write rather than losing it).
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
use fantastic_kernel::{Agent, AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const README: &str = include_str!("readme.md");

const MEM_SENTENCE: &str = "Your durable memory. Facts you must remember across sessions live here — auto-loaded into your context on boot. `set` a descriptive key the moment the user tells you something worth keeping (a name, a preference, a decision). Your current facts are already in your context — read them, don't re-fetch.";
const DATA_SENTENCE: &str = "Your durable scratch-state (component state, config, run params, current selection). One value per key, overwrite-in-place; auto-loaded into your context on boot.";

pub struct YamlStateBundle;

/// The bound file_bridge provider id, off this agent's record.
fn file_bridge_id(agent: &Agent) -> Option<String> {
    agent.meta.read().ok().and_then(|g| {
        g.get("file_bridge_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// `state.yaml` in the agent's own dir, RELATIVE to the provider's root (the
/// `.fantastic` store) — `agents/<id>/state.yaml`, landing next to its
/// `agent.json` (no `.fantastic/.fantastic/...` double-nest). Matches Python.
fn state_path(agent: &Agent) -> String {
    format!("agents/{}/state.yaml", agent.id.0)
}

/// Failfast if no provider is wired — persistence needs an opened file_bridge.
/// Error text is byte-identical to the Python bundle.
fn need_file_bridge(agent: &Agent, verb: &str) -> Option<Value> {
    if file_bridge_id(agent).is_none() {
        return Some(json!({
            "error": format!(
                "yaml_state.{verb}: file_bridge_id required — wire (and open) a file_bridge to persist"
            )
        }));
    }
    None
}

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
    let Some(fid) = file_bridge_id(agent) else {
        return Map::new();
    };
    let r = kernel
        .send(
            &AgentId::from(fid.as_str()),
            json!({"type": "read", "path": state_path(agent)}),
        )
        .await;
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
    let Some(fid) = file_bridge_id(agent) else {
        return Some(json!({"error": format!("yaml_state.{verb}: file_bridge_id required")}));
    };
    let w = kernel
        .send(
            &AgentId::from(fid.as_str()),
            json!({"type": "write", "path": state_path(agent), "content": emit_yaml(doc)}),
        )
        .await;
    if !w.is_object() {
        return Some(json!({"error": format!("yaml_state.{verb}: provider gave no reply")}));
    }
    let reason = w.get("error").and_then(Value::as_str).or_else(|| {
        if w.get("reason").and_then(Value::as_str) == Some("unauthorized") {
            Some("unauthorized")
        } else {
            None
        }
    });
    if let Some(reason) = reason {
        return Some(json!({
            "error": format!("yaml_state.{verb}: provider refused write — {reason}")
        }));
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
        "file_bridge_id": file_bridge_id(agent),
        "verbs": {
            "read": "args: key?:str. Value at key (null if absent); whole doc if key omitted.",
            "keys": "args: none. List keys + value sizes — the table-of-contents.",
            "set": "args: key:str, value:any. Upsert one key. Persisted through file_bridge_id; failfast if unwired.",
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
                // FAILFAST first — persistence needs an opened file_bridge.
                if let Some(err) = need_file_bridge(&agent, "set") {
                    return Ok(Some(err));
                }
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
                if let Some(err) = need_file_bridge(&agent, "delete") {
                    return Ok(Some(err));
                }
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
                if let Some(err) = need_file_bridge(&agent, "replace") {
                    return Ok(Some(err));
                }
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
