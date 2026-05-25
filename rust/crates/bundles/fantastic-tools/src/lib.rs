//! `tools.tools` — registrable tool calling for LLM-using agents.
//!
//! Holds a process-global map `name → {agent_id, verb, description,
//! parameters_schema, sender}`. LLM bundles (FM, ollama, …) read the
//! map on every model call and prepend the tool defs to the request.
//! Tool calls themselves go through the kernel's existing
//! [`fantastic_kernel::Kernel::send`] primitive — **send IS the tool
//! call**; this bundle is just a naming + schema layer over it.
//!
//! ### Sender attribution
//!
//! `register` / `unregister_by_sender` take an explicit `sender`
//! field in the payload. The kernel's own `send` rewraps the
//! `with_sender` task-local with the target id before invoking a
//! bundle, so bundles can't read the original sender from
//! `current_sender()` — we surface it as an explicit field instead.
//! The UniFFI sugar wrappers (`Kernel::register_tool`, etc.) take a
//! `sender_id` param and inject it into the payload transparently.
//! Every entry stores the agent id that registered it; on logout /
//! mode change the owner calls `unregister_by_sender` to drop all
//! tools it owns in one move, no name list needed.
//!
//! ### Conditionality
//!
//! Three layers, none of which need anything beyond the existing
//! kernel surface:
//! 1. **Per-arg**: JSON Schema in `parameters_schema` (FM uses guided
//!    generation against it — `if/then`, `oneOf`, `enum`, `required`).
//! 2. **Per-tool**: presence in the registry. Re-register / unregister
//!    as state changes; the LLM only ever sees currently-valid tools.
//! 3. **Runtime**: dispatch target returns
//!    `{error, reason: "precondition_failed"}` and the LLM retries.
//!
//! ### Lifecycle
//!
//! The registry is **process-global, in-memory only** (matches the
//! "controlled outside, immutable unless unset" framing). Not in
//! `kernel.save()`. App re-registers tools on every kernel boot.
//! Cascade-delete on the tools agent itself fires `Bundle::on_delete`
//! → [`clear`].

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "tools.tools";

/// readme.md auto-seeded into the agent's dir on creation (Disk mode).
pub const README: &str = include_str!("readme.md");

// ── entry + registry ──────────────────────────────────────────────

/// One registered tool. `name` is the primary key the LLM addresses;
/// `agent_id` + `verb` together form the dispatch coordinates;
/// `sender` is the cleanup key (set automatically from
/// `current_sender()` at register time).
#[derive(Debug, Clone)]
pub struct ToolEntry {
    /// Tool name as the LLM sees it. Primary key — must be unique
    /// across the whole registry; re-registering with the same name
    /// replaces the previous entry (last-write-wins).
    pub name: String,
    /// Dispatch target — any agent id reachable via
    /// `kernel.send(agent_id, …)`.
    pub agent_id: AgentId,
    /// Payload verb (`payload["type"]`) when dispatching. `None` means
    /// "use the tool name as the verb" — handy when an agent answers
    /// one verb per tool ergonomically.
    pub verb: Option<String>,
    /// LLM-facing description. The only natural-language hint about
    /// when to call this tool.
    pub description: String,
    /// JSON Schema for the args. FM uses guided generation against
    /// this; OpenAI / Anthropic use it for their `parameters` field.
    pub parameters_schema: Value,
    /// Who registered this entry. Read from `payload["sender"]` at
    /// register time; used by `unregister_by_sender` for mass cleanup.
    /// Defaults to `"anonymous"` if no `sender` was supplied.
    pub sender: AgentId,
}

type ToolMap = HashMap<String, ToolEntry>;
static TOOLS: OnceLock<RwLock<ToolMap>> = OnceLock::new();

fn tools_lock() -> &'static RwLock<ToolMap> {
    TOOLS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Insert / replace `entry`. Last write on the same name wins.
pub fn register(entry: ToolEntry) {
    tools_lock()
        .write()
        .expect("tools lock poisoned")
        .insert(entry.name.clone(), entry);
}

/// Drop the entry with `name`. Returns true if something was removed.
pub fn unregister(name: &str) -> bool {
    tools_lock()
        .write()
        .expect("tools lock poisoned")
        .remove(name)
        .is_some()
}

/// Drop every entry whose `sender` matches. Returns the number of
/// entries removed.
pub fn unregister_by_sender(sender: &AgentId) -> usize {
    let mut g = tools_lock().write().expect("tools lock poisoned");
    let before = g.len();
    g.retain(|_, e| &e.sender != sender);
    before - g.len()
}

/// Drop every entry. Primarily for tests + the bundle's
/// `on_delete` cascade hook.
pub fn clear() -> usize {
    let mut g = tools_lock().write().expect("tools lock poisoned");
    let n = g.len();
    g.clear();
    n
}

/// Snapshot every entry, sorted by name for deterministic output.
pub fn snapshot() -> Vec<ToolEntry> {
    let g = tools_lock().read().expect("tools lock poisoned");
    let mut out: Vec<ToolEntry> = g.values().cloned().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Look up one entry by name.
pub fn lookup(name: &str) -> Option<ToolEntry> {
    tools_lock()
        .read()
        .expect("tools lock poisoned")
        .get(name)
        .cloned()
}

/// Current count. Cheap probe used by `reflect`.
pub fn count() -> usize {
    tools_lock().read().expect("tools lock poisoned").len()
}

// ── bundle ────────────────────────────────────────────────────────

/// The tool-registry bundle. Stateless — the registry is a separate
/// `OnceLock` static so multiple kernels in one process share it
/// (matches `proxy_agent`'s pattern).
#[derive(Debug, Default)]
pub struct ToolsBundle;

impl ToolsBundle {
    /// Construct a fresh bundle. Stateless — `Default` works too.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Bundle for ToolsBundle {
    fn name(&self) -> &str {
        "tools"
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
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => reflect(agent_id),
            "boot" => json!({"ok": true}),
            "shutdown" => json!({"ok": true}),
            "register" => register_verb(payload),
            "unregister" => unregister_verb(payload),
            "unregister_by_sender" => unregister_by_sender_verb(payload),
            "clear" => json!({"ok": true, "removed": clear()}),
            "list" => list_verb(),
            "list_for_llm" => list_for_llm_verb(),
            "dispatch" => dispatch_verb(kernel, payload).await,
            other => json!({
                "error": format!("unknown verb {other:?}"),
                "reason": "unknown_verb",
            }),
        };
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        _agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        // Cascade-delete on the tools agent wipes the registry — the
        // app's intent when deleting the agent is "no more tools".
        clear();
        Ok(())
    }
}

// ── verb impls ────────────────────────────────────────────────────

fn reflect(agent_id: &AgentId) -> Value {
    json!({
        "id": agent_id.as_str(),
        "sentence": "Tool registry for LLM tool calling. Maps name → {agent_id, verb, schema, sender}; LLM-using bundles read list_for_llm before every model call. Dispatch is kernel.send(entry.agent_id, ...).",
        "kind": "tools",
        "tool_count": count(),
        "verbs": {
            "reflect": "Identity + tool_count probe.",
            "register": "args: name, agent_id, verb?, description, parameters_schema, sender?. Returns {ok, name}.",
            "unregister": "args: name. Returns {ok} or {error, reason: not_found}.",
            "unregister_by_sender": "args: sender. Drops every entry whose sender matches. Returns {ok, removed}.",
            "clear": "Drops every entry. Returns {ok, removed}. Tests / admin only.",
            "list": "Returns {tools: [full entries, including sender]}. Debug / inspection.",
            "list_for_llm": "Returns {tools: [{name, description, parameters}]} — Apple-FM / OpenAI compatible shape. Read this before every LLM call.",
            "dispatch": "args: name, arguments. Looks up name → does kernel.send(entry.agent_id, {type: verb_or_name, ...arguments}). Returns raw reply from the dispatch target.",
        },
        "emits": {},
    })
}

fn register_verb(payload: &Value) -> Value {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return json!({"error": "register requires name", "reason": "invalid_args"});
    };
    let Some(agent_id_str) = payload.get("agent_id").and_then(Value::as_str) else {
        return json!({"error": "register requires agent_id", "reason": "invalid_args"});
    };
    let Some(description) = payload.get("description").and_then(Value::as_str) else {
        return json!({"error": "register requires description", "reason": "invalid_args"});
    };
    let parameters_schema = match payload.get("parameters_schema") {
        Some(v) => coerce_schema(v),
        None => {
            return json!({"error": "register requires parameters_schema", "reason": "invalid_args"})
        }
    };
    let verb = payload
        .get("verb")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let sender = payload
        .get("sender")
        .and_then(Value::as_str)
        .map(AgentId::from)
        .unwrap_or_else(|| AgentId::from("anonymous"));

    let entry = ToolEntry {
        name: name.to_string(),
        agent_id: AgentId::from(agent_id_str),
        verb,
        description: description.to_string(),
        parameters_schema,
        sender,
    };
    register(entry);
    json!({"ok": true, "name": name})
}

fn unregister_verb(payload: &Value) -> Value {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return json!({"error": "unregister requires name", "reason": "invalid_args"});
    };
    if unregister(name) {
        json!({"ok": true, "name": name})
    } else {
        json!({"error": format!("no tool named {name:?}"), "reason": "not_found"})
    }
}

fn unregister_by_sender_verb(payload: &Value) -> Value {
    let Some(sender_str) = payload.get("sender").and_then(Value::as_str) else {
        return json!({
            "error": "unregister_by_sender requires sender",
            "reason": "invalid_args",
        });
    };
    let sender = AgentId::from(sender_str);
    let removed = unregister_by_sender(&sender);
    json!({"ok": true, "removed": removed, "sender": sender.as_str()})
}

fn list_verb() -> Value {
    let tools: Vec<Value> = snapshot()
        .into_iter()
        .map(|e| {
            json!({
                "name": e.name,
                "agent_id": e.agent_id.as_str(),
                "verb": e.verb,
                "description": e.description,
                "parameters_schema": e.parameters_schema,
                "sender": e.sender.as_str(),
            })
        })
        .collect();
    json!({"tools": tools, "count": tools_lock().read().expect("tools lock poisoned").len()})
}

fn list_for_llm_verb() -> Value {
    let tools: Vec<Value> = snapshot()
        .into_iter()
        .map(|e| {
            json!({
                "name": e.name,
                "description": e.description,
                "parameters": e.parameters_schema,
            })
        })
        .collect();
    json!({"tools": tools})
}

async fn dispatch_verb(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return json!({"error": "dispatch requires name", "reason": "invalid_args"});
    };
    let Some(entry) = lookup(name) else {
        return json!({
            "error": format!("no tool named {name:?}"),
            "reason": "tool_not_found",
        });
    };
    // Build the payload to send to the dispatch target. The
    // arguments object's keys flatten into the payload alongside the
    // generated `type` field. Empty arguments are fine.
    let mut out: Map<String, Value> = match payload.get("arguments") {
        Some(Value::Object(m)) => m.clone(),
        Some(other) => {
            return json!({
                "error": format!("dispatch arguments must be an object, got {other}"),
                "reason": "invalid_args",
            });
        }
        None => Map::new(),
    };
    let verb_str = entry.verb.clone().unwrap_or_else(|| entry.name.clone());
    out.insert("type".to_string(), Value::String(verb_str));
    kernel.send(&entry.agent_id, Value::Object(out)).await
}

// ── helpers ───────────────────────────────────────────────────────

/// Accept `parameters_schema` as either an object (idiomatic) or a
/// JSON string (CLI ergonomics — `key=value` parser produces strings).
/// Falls back to the raw value if parsing fails so the caller gets a
/// stored entry instead of a hard rejection.
fn coerce_schema(v: &Value) -> Value {
    if let Some(s) = v.as_str() {
        match serde_json::from_str::<Value>(s) {
            Ok(parsed) => return parsed,
            Err(_) => return v.clone(),
        }
    }
    v.clone()
}

#[cfg(test)]
mod tests;
