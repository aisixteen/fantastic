//! Provider-agnostic chat UI front-end.
//!
//! Holds an `upstream_id` pointing at a backend that answers `send`,
//! `history`, `interrupt` (and emits `token`/`done`/`queued`/`status`).
//! Any backend matching that surface (ollama, NVIDIA NIM, …) works
//! without changes here.
//!
//! ## Verbs
//!
//! - `reflect` → `{id, sentence, upstream_id, provider, verbs}`.
//!   `provider` defaults to `"ollama"`.
//! - `boot` — **Rust phase 1**: if `upstream_id` is set on the record,
//!   returns `{ok: true, upstream_id}`. If unset, returns an error —
//!   provider auto-spawn is deferred until the ollama_backend is
//!   ported. Set `upstream_id` manually via `update_agent`.
//! - `shutdown` → no-op.
//! - `render_html` → `{html}` — the embedded chat page; `transport.js`
//!   is injected by the web bundle on serve.
//! - `get_webapp` → `{url, default_width, default_height, title}` —
//!   makes this agent canvas-eligible.
//!
//! ## AI rehaul backlog (TODO — not in scope for the current port)
//!
//! These items will need a coordinated redesign across all LLM
//! backends + ai_chat_webapp before the next major bump:
//!
//! 1. Cross-backend conversation portability — today history lives
//!    in `<backend>/chat_<client>.json`. Switching `upstream_id`
//!    starts a fresh conversation. Future: history travels with the
//!    chat tile, backends become stateless-modulo-streaming.
//! 2. Tool-call streaming protocol — current contract is one
//!    tool_call per chunk (ollama) vs argument fragments aggregated
//!    across chunks (OpenAI/NIM). Pick one and version it.
//! 3. Multi-modal binary frames — image/audio payloads currently
//!    have no defined wire shape. Needs the WS binary frame channel
//!    (also blocks terminal_backend's image-paste).
//! 4. Cost / token tracking — no per-turn cost report today.
//! 5. Context-window management — backends silently truncate; no
//!    surface to inspect or override.
//! 6. Auth — api_key sidecar is plaintext; no per-tenant scoping.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "ai_chat_webapp.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Embedded chat HTML (the JS frontend, served at `/<id>/`).
pub const CHAT_HTML: &str = include_str!("index.html");

/// The chat front-end bundle.
pub struct AiChatWebappBundle;

#[async_trait]
impl Bundle for AiChatWebappBundle {
    fn name(&self) -> &str {
        "ai_chat_webapp"
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
            "reflect" => reflect_reply(agent_id, kernel),
            "boot" => boot_reply(agent_id, kernel).await,
            "shutdown" => Value::Null,
            "render_html" => json!({"html": CHAT_HTML}),
            "get_webapp" => json!({
                "url": format!("/{}/", agent_id),
                "default_width": 480,
                "default_height": 600,
                "title": "chat",
            }),
            other => json!({"error": format!("ai_chat_webapp: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }
}

/// Read a string-typed meta field for the given agent, returning `None`
/// when the agent is missing or the field is absent / non-string.
fn meta_str(kernel: &Arc<Kernel>, agent_id: &AgentId, key: &str) -> Option<String> {
    kernel.agents.get(agent_id).and_then(|e| {
        e.meta
            .read()
            .expect("meta poisoned")
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn reflect_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let upstream = meta_str(kernel, agent_id, "upstream_id").unwrap_or_default();
    let provider = meta_str(kernel, agent_id, "provider").unwrap_or_else(|| "ollama".to_string());
    json!({
        "id": agent_id.as_str(),
        "sentence": "Chat UI fronting an upstream LLM backend.",
        "upstream_id": upstream,
        "provider": provider,
        "verbs": {
            "reflect": "Identity + upstream_id + provider. No args.",
            "boot": "Verify upstream_id is set; phase-1 no auto-spawn.",
            "shutdown": "No-op.",
            "render_html": "Return the embedded chat page.",
            "get_webapp": "Return iframeable URL + viewport defaults.",
        }
    })
}

/// Boot: idempotently spawn the provider backend (ollama or nvidia_nim
/// per `record.provider`, default `ollama`), record its id as
/// `upstream_id`, and boot it. Mirrors Python's
/// `ai_chat_webapp/tools.py::_boot`. Cascade-delete drops the pair
/// because the backend is wired as a child of this webapp.
async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let me = match kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
        Some(a) => a,
        None => return Value::Null,
    };
    let provider = meta_str(kernel, agent_id, "provider").unwrap_or_else(|| "ollama".to_string());
    let backend_hm = if provider == "ollama" {
        "ollama_backend.tools"
    } else {
        "nvidia_nim_backend.tools"
    };

    // Already paired? upstream_id pointing at a live backend → no-op.
    if let Some(up) = meta_str(kernel, agent_id, "upstream_id") {
        if !up.is_empty() && kernel.agents.contains_key(&AgentId::from(up.as_str())) {
            return json!({"ok": true, "upstream_id": up});
        }
    }
    // Or a child backend with the matching handler_module exists?
    let has_child = me.child_ids().iter().any(|cid| {
        kernel
            .agents
            .get(cid)
            .map(|e| e.handler_module.as_deref() == Some(backend_hm))
            .unwrap_or(false)
    });
    if has_child {
        return Value::Null;
    }

    let create_reply = kernel
        .send(
            agent_id,
            json!({"type": "create_agent", "handler_module": backend_hm}),
        )
        .await;
    let Some(backend_id) = create_reply.get("id").and_then(Value::as_str) else {
        return json!({"error": format!("ai_chat_webapp.boot: create backend failed: {create_reply}")});
    };
    let backend_id = backend_id.to_string();
    let update_reply = kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "update_agent",
                "id": agent_id.as_str(),
                "upstream_id": backend_id,
            }),
        )
        .await;
    if let Some(err) = update_reply.get("error").and_then(Value::as_str) {
        return json!({"error": format!("ai_chat_webapp.boot: write upstream_id failed: {err}")});
    }
    Value::Null
}

#[cfg(test)]
mod tests;
