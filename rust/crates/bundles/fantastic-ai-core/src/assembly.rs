//! Prompt assembly: render_reflect, build/render menu, the SEND tool
//! def + SEND_HOWTO, and the per-turn message list.

use crate::history::load_history;
use crate::state::BackendState;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// Render a reflect reply into a one-line `sentence  k=v  k=v` blurb.
pub fn render_reflect(v: &Value) -> String {
    let mut obj = match v.as_object() {
        Some(o) => o.clone(),
        None => return String::new(),
    };
    let sentence = obj
        .remove("sentence")
        .and_then(|s| s.as_str().map(str::to_string))
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    for (k, val) in obj.iter() {
        let rendered = match val.as_str() {
            Some(s) => s.to_string(),
            None => serde_json::to_string(val).unwrap_or_default(),
        };
        parts.push(format!("{k}={rendered}"));
    }
    let fields = parts.join("  ");
    format!("{sentence}  {fields}").trim().to_string()
}

/// Reflect on every running agent (skip self) and collect their
/// one-line sentence + verb names — the model's "menu of capabilities".
pub async fn build_menu(self_id: &AgentId, kernel: &Arc<Kernel>) -> Vec<Value> {
    let online = kernel
        .send(&AgentId::from("core"), json!({"type": "list_agents"}))
        .await;
    let Some(agents) = online.get("agents").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for a in agents {
        let Some(id) = a.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id == self_id.as_str() {
            continue;
        }
        let r = kernel
            .send(&AgentId::from(id), json!({"type": "reflect"}))
            .await;
        let sentence = r
            .get("sentence")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let verb_names: Vec<String> = match r.get("verbs") {
            Some(Value::Object(m)) => m.keys().cloned().collect(),
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        items.push(json!({
            "id": id,
            "sentence": sentence,
            "verbs": verb_names,
        }));
    }
    items
}

/// Format the menu as bullet lines for the system prompt.
pub fn render_menu(menu: &[Value]) -> String {
    if menu.is_empty() {
        return "## Available agents\n(none — only `core` and `self`)".to_string();
    }
    let mut lines = vec![
        "## Available agents (reflect on any for full verb signatures + arg shapes)".to_string(),
    ];
    for m in menu {
        let id = m.get("id").and_then(Value::as_str).unwrap_or("");
        let sentence = m.get("sentence").and_then(Value::as_str).unwrap_or("");
        let verbs: Vec<String> = m
            .get("verbs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let head = if verbs.len() > 10 {
            format!("{} …", verbs[..10].join(", "))
        } else {
            verbs.join(", ")
        };
        let head_display = if head.is_empty() {
            "(none)".to_string()
        } else {
            head
        };
        lines.push(format!("- `{id}` — {sentence} — verbs: {head_display}"));
    }
    lines.join("\n")
}

/// The universal `send` tool how-to, appended to the system prompt.
pub const SEND_HOWTO: &str = r#"## How to use the `send` tool
You have ONE tool: `send(target_id, payload)`. EVERY action goes through it.
- To do something concrete (read a file, run python, list agents, etc.), pick
  an agent from the menu above whose verbs cover what you need, then build
  `{type:'<verb>', ...args}` and pass it as `payload`.
- To learn an agent's full verb signatures (arg names, types):
  `send('<id>', {type:'reflect'})` returns `{verbs: {name: 'doc'}, ...}`.
- To rebuild your menu of agents (useful right after you create one):
  `send('<your_own_id>', {type:'refresh_menu'})` — next turn shows the fresh menu.
- NEVER claim "I don't have access" without trying the menu first. The
  send tool reaches every agent in the system.
"#;

/// The universal `send` tool definition handed to the provider.
pub fn send_tool_def() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "send",
            "description": "Send a message to any agent in the Fantastic substrate. Universal verb on every agent: reflect (returns identity + state). Discover agents by sending list_agents to the core agent.",
            "parameters": {
                "type": "object",
                "properties": {
                    "target_id": {
                        "type": "string",
                        "description": "Agent id to send the payload to (e.g. 'core', 'cli', 'terminal_xxx').",
                    },
                    "payload": {
                        "type": "object",
                        "description": "{\"type\": \"<verb>\", ...fields}. Universal verb: reflect.",
                    },
                },
                "required": ["target_id", "payload"],
            },
        },
    })
}

/// Build the per-turn message list: rebuilt system block (primer + self
/// reflect + lazy menu + howto), persisted history, then the user turn.
pub async fn assemble_messages(
    self_id: &AgentId,
    state: &Arc<BackendState>,
    user_text: &str,
    kernel: &Arc<Kernel>,
    client_id: &str,
) -> Vec<Value> {
    let primer = kernel
        .send(&AgentId::from("kernel"), json!({"type": "reflect"}))
        .await;
    let me = kernel.send(self_id, json!({"type": "reflect"})).await;

    // Lazy menu rebuild.
    let needs_rebuild = state.menu.lock().expect("menu poisoned").is_none();
    if needs_rebuild {
        let menu = build_menu(self_id, kernel).await;
        *state.menu.lock().expect("menu poisoned") = Some(menu);
    }
    let menu = state
        .menu
        .lock()
        .expect("menu poisoned")
        .clone()
        .unwrap_or_default();

    let sys_blocks = [
        render_reflect(&primer),
        format!("You are `{}`. {}", self_id, render_reflect(&me)),
        render_menu(&menu),
        SEND_HOWTO.to_string(),
    ];
    let system_content = sys_blocks.join("\n\n");
    let mut messages = vec![json!({"role": "system", "content": system_content})];
    messages.extend(load_history(self_id, kernel, client_id).await);
    messages.push(json!({"role": "user", "content": user_text}));
    messages
}
