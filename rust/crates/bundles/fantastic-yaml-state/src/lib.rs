//! `yaml_state` — a durable YAML key-value memory agent.
//!
//! One bundle, instantiated as N agents. The `mode` meta ("mem" | "data")
//! sets the *discipline* (and the reflect sentence) — the verbs are
//! identical:
//!
//! - data → current scratch-state, overwrite-in-place.
//! - mem  → durable keyed facts, accrete + prune at LLM discretion.
//!
//! Disk-is-truth: its state is a YAML file (`state.yaml`) in the agent's
//! own dir — human-editable, git-diffable, atomic-write (temp + rename).
//! The single-agent inbox serializes writes, so no locking. Cascade-
//! delete removes the agent dir (and the file) for free — no `on_delete`.
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
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

pub const README: &str = include_str!("readme.md");

const MEM_SENTENCE: &str = "Your durable memory. Facts you must remember across sessions live here — auto-loaded into your context on boot. `set` a descriptive key the moment the user tells you something worth keeping (a name, a preference, a decision). Your current facts are already in your context — read them, don't re-fetch.";
const DATA_SENTENCE: &str = "Your durable scratch-state (UI state, hyperparams, current selection). One value per key, overwrite-in-place; auto-loaded into your context on boot.";

pub struct YamlStateBundle;

fn state_path(agent: &Agent) -> PathBuf {
    agent.root_path.join("state.yaml")
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

fn load(agent: &Agent) -> Map<String, Value> {
    let Ok(text) = fs::read_to_string(state_path(agent)) else {
        return Map::new();
    };
    if text.trim().is_empty() {
        return Map::new();
    }
    match serde_yaml::from_str::<Value>(&text) {
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

fn dump(agent: &Agent, doc: &Map<String, Value>) -> Result<(), String> {
    let path = state_path(agent);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("yaml.tmp");
    fs::write(&tmp, emit_yaml(doc)).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

fn value_size(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        other => other.to_string().chars().count(),
    }
}

fn reflect_reply(agent: &Agent) -> Value {
    let mode = mode_of(agent);
    let doc = load(agent);
    json!({
        "id": agent.id.0,
        "sentence": if mode == "mem" { MEM_SENTENCE } else { DATA_SENTENCE },
        "mode": mode,
        "key_count": doc.len(),
        "verbs": {
            "read": "args: key?:str. Value at key (null if absent); whole doc if key omitted.",
            "keys": "args: none. List keys + value sizes — the table-of-contents.",
            "set": "args: key:str, value:any. Upsert one key.",
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
            "reflect" => reflect_reply(&agent),
            "boot" | "shutdown" => Value::Null,
            "read" => {
                let doc = load(&agent);
                match payload.get("key").and_then(Value::as_str) {
                    Some(k) => {
                        json!({"key": k, "value": doc.get(k).cloned().unwrap_or(Value::Null)})
                    }
                    None => json!({ "doc": Value::Object(doc) }),
                }
            }
            "keys" => {
                let doc = load(&agent);
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
                let mut doc = load(&agent);
                doc.insert(key.to_string(), value.clone());
                if let Err(e) = dump(&agent, &doc) {
                    return Ok(Some(json!({ "error": e })));
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
                let mut doc = load(&agent);
                let existed = doc.remove(key).is_some();
                if let Err(e) = dump(&agent, &doc) {
                    return Ok(Some(json!({ "error": e })));
                }
                json!({"key": key, "deleted": existed})
            }
            "replace" => {
                let doc = match payload.get("doc") {
                    Some(Value::Object(m)) => m.clone(),
                    None => Map::new(),
                    _ => {
                        return Ok(Some(
                            json!({"error": "yaml_state.replace: doc must be an object"}),
                        ))
                    }
                };
                let n = doc.len();
                if let Err(e) = dump(&agent, &doc) {
                    return Ok(Some(json!({ "error": e })));
                }
                json!({"replaced": true, "keys": n})
            }
            "state_yaml" => json!({ "yaml": emit_yaml(&load(&agent)) }),
            other => json!({"error": format!("yaml_state: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests;
