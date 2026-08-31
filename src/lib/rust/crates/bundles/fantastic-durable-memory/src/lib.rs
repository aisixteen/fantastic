//! `durable_memory` — a modeless durable key-value memory agent.
//!
//! One store, one discipline: keys in, values out. What the store is FOR lives in
//! the agent's `description` (the reflect sentence), not in a bundle-private `mode`
//! enum. An earlier design carried `mode: mem|data|chat`; the verbs were identical in
//! all three, so the modes were dropped.
//!
//! NO FS OF ITS OWN. The record names `file_bridge_id`, and every byte moves through
//! kernel sends to that leg over the AGNOSTIC STREAM CHANNEL — `write_stream` /
//! `read_stream`, RAW BYTES on the binary carrier, no string body anywhere. This
//! bundle never touches `std::fs`; a store with no leg wired FAILFASTS on write (no
//! silent RAM) and reads empty.
//!
//! YAML is the intermediate: the live state is one map, serialized to YAML bytes on
//! every write, parsed on read. Disk-is-truth — read fresh each call, no cache — and
//! the sidecar stays human-editable, greppable, git-diffable.
//!
//! Keys are flat namespaced strings (dotted convention: `domain.subject.attribute`).
//! Values are arbitrary JSON. `get` is a RESERVED kernel verb, so the read verb is
//! `read`. Mirrors the canonical Python `durable_memory` bundle.

use async_trait::async_trait;
use fantastic_bundle as _; // keep the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::send::with_operator;
use fantastic_kernel::{Agent, AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const README: &str = include_str!("readme.md");

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "durable_memory.tools";

/// Where the store lands inside the bound leg's root.
const STATE_PATH: &str = "state.yaml";

/// The durable_memory bundle.
pub struct DurableMemoryBundle;

/// The bound `file_bridge` provider id off this agent's record — empty/absent ⇒ None.
fn fbid(agent: &Agent) -> Option<String> {
    agent.meta.read().ok().and_then(|g| {
        g.get("file_bridge_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

/// Serialize the store to YAML bytes (sorted keys — deterministic, git-diffable).
/// Empty store ⇒ empty bytes, matching python's `b""`.
fn emit_yaml(doc: &Map<String, Value>) -> Vec<u8> {
    if doc.is_empty() {
        return Vec::new();
    }
    let sorted: BTreeMap<&String, &Value> = doc.iter().collect();
    serde_yaml::to_string(&sorted)
        .unwrap_or_default()
        .into_bytes()
}

/// The whole store, via `read_stream` chunks off the bound leg (RAW BYTES on the
/// binary carrier). Missing leg, missing file, or unparseable content ⇒ {} — reads
/// are lenient; writes are where loss would hide, and they failfast instead.
async fn load(agent: &Agent, kernel: &Arc<Kernel>) -> Map<String, Value> {
    let Some(fid) = fbid(agent) else {
        return Map::new();
    };
    let leg = AgentId::from(fid.as_str());
    let mut raw: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    // Persistence is processed from ABOVE: reading/writing your own sidecar is
    // operator bookkeeping (a side effect of your authorized verb), not a lateral
    // call — so it runs ungated, like create_agent's child-boot.
    loop {
        let (r, body) = with_operator(kernel.send_with_binary(
            &leg,
            json!({"type": "read_stream", "path": STATE_PATH, "offset": offset}),
            Vec::new(),
        ))
        .await;
        if r.get("error").is_some() {
            return Map::new();
        }
        if !body.is_empty() {
            raw.extend_from_slice(&body);
        }
        let eof = r.get("eof").and_then(Value::as_bool).unwrap_or(true);
        if eof || body.is_empty() {
            break;
        }
        offset = r
            .get("next_offset")
            .and_then(Value::as_u64)
            .unwrap_or(offset + body.len() as u64);
    }
    if raw.is_empty() {
        return Map::new();
    }
    match serde_yaml::from_slice::<Value>(&raw) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// Write the whole store as ONE truncating `write_stream` chunk to the bound leg. An
/// unwired or refusing leg is surfaced as the verb's error — never silent RAM.
/// Returns an error `Value`, or `None` on success. Text is byte-identical to python.
async fn persist(
    agent: &Agent,
    kernel: &Arc<Kernel>,
    doc: &Map<String, Value>,
    verb: &str,
) -> Option<Value> {
    let Some(fid) = fbid(agent) else {
        return Some(json!({"error": format!(
            "durable_memory.{verb}: no file_bridge_id wired — writes have nowhere \
             durable to land, refusing rather than keeping state in RAM"
        )}));
    };
    let leg = AgentId::from(fid.as_str());
    let (w, _) = with_operator(kernel.send_with_binary(
        &leg,
        json!({"type": "write_stream", "path": STATE_PATH, "truncate": true}),
        emit_yaml(doc),
    ))
    .await;
    if !w.is_object() {
        return Some(json!({"error": format!("durable_memory.{verb}: sink gave no reply")}));
    }
    if let Some(err) = w.get("error").and_then(Value::as_str) {
        let mut out = json!({ "error": format!("durable_memory.{verb}: {err}") });
        // A sealed-edge denial must stay a denial, with the open-it hint intact.
        for k in ["reason", "hint"] {
            if let Some(v) = w.get(k) {
                out[k] = v.clone();
            }
        }
        return Some(out);
    }
    None
}

async fn reflect_reply(agent: &Agent, kernel: &Arc<Kernel>) -> Value {
    let doc = load(agent, kernel).await;
    let sentence = agent
        .meta
        .read()
        .ok()
        .and_then(|g| {
            g.get("description")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            "A durable key/value store. `set`/`read`/`append` named keys; state \
             survives restarts through its bound file leg."
                .to_string()
        });
    let mut keys: Vec<&String> = doc.keys().collect();
    keys.sort();
    json!({
        "id": agent.id.0,
        "sentence": sentence,
        "file_bridge_id": fbid(agent),
        "keys": keys,
        "verbs": {
            "read": "args: key:str — {key, value} (value null when absent)",
            "set": "args: key:str, value:any — stores it (overwrite)",
            "append": "args: key:str, value:any — appends to the LIST at key (created if absent; refuses a non-list)",
            "keys": "no args — {keys: [...]}",
            "delete": "args: key:str — {key, deleted:bool}",
        },
    })
}

/// The `key` field as a non-empty string, or the verb's error.
fn key_of(payload: &Value, verb: &str) -> Result<String, Value> {
    match payload.get("key").and_then(Value::as_str) {
        Some(k) if !k.is_empty() => Ok(k.to_string()),
        _ => Err(json!({"error": format!("durable_memory.{verb}: key (str) required")})),
    }
}

#[async_trait]
impl Bundle for DurableMemoryBundle {
    fn name(&self) -> &str {
        "durable_memory"
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
                let key = match key_of(payload, "read") {
                    Ok(k) => k,
                    Err(e) => return Ok(Some(e)),
                };
                let doc = load(&agent, kernel).await;
                json!({"key": key, "value": doc.get(&key).cloned().unwrap_or(Value::Null)})
            }
            "keys" => {
                let doc = load(&agent, kernel).await;
                let mut keys: Vec<&String> = doc.keys().collect();
                keys.sort();
                json!({ "keys": keys })
            }
            "set" => {
                let key = match key_of(payload, "set") {
                    Ok(k) => k,
                    Err(e) => return Ok(Some(e)),
                };
                let Some(value) = payload.get("value") else {
                    return Ok(Some(json!({"error": "durable_memory.set: value required"})));
                };
                let mut doc = load(&agent, kernel).await;
                doc.insert(key.clone(), value.clone());
                if let Some(err) = persist(&agent, kernel, &doc, "set").await {
                    return Ok(Some(err));
                }
                json!({"key": key, "ok": true})
            }
            "append" => {
                let key = match key_of(payload, "append") {
                    Ok(k) => k,
                    Err(e) => return Ok(Some(e)),
                };
                let Some(value) = payload.get("value") else {
                    return Ok(Some(
                        json!({"error": "durable_memory.append: value required"}),
                    ));
                };
                let mut doc = load(&agent, kernel).await;
                let mut cur = match doc.get(&key) {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(a)) => a.clone(),
                    Some(_) => {
                        return Ok(Some(json!({"error": format!(
                            "durable_memory.append: key {key:?} holds a non-list — \
                             `set` it to a list first, or pick another key"
                        )})))
                    }
                };
                cur.push(value.clone());
                let length = cur.len();
                doc.insert(key.clone(), Value::Array(cur));
                if let Some(err) = persist(&agent, kernel, &doc, "append").await {
                    return Ok(Some(err));
                }
                json!({"key": key, "ok": true, "length": length})
            }
            "delete" => {
                let key = match key_of(payload, "delete") {
                    Ok(k) => k,
                    Err(e) => return Ok(Some(e)),
                };
                let mut doc = load(&agent, kernel).await;
                let existed = doc.remove(&key).is_some();
                if existed {
                    if let Some(err) = persist(&agent, kernel, &doc, "delete").await {
                        return Ok(Some(err));
                    }
                }
                json!({"key": key, "deleted": existed})
            }
            other => json!({"error": format!("durable_memory: unknown type '{other}'")}),
        };
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests;
