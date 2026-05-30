//! xterm UI front-end as an agent.
//!
//! Pairs with a `terminal_backend` (tracked via `upstream_id` on the
//! record) and serves a single HTML page that runs xterm.js in the
//! browser. The Python version auto-creates the backend as a child on
//! first `boot`; the Rust port leaves boot as a no-op for now —
//! `terminal_backend` is not yet ported, so the UI runs dormant when
//! `upstream_id` is unset and the page renders a configuration hint.
//!
//! ## Verbs
//!
//! - `reflect` → `{id, sentence, upstream_id, verbs}`
//! - `render_html` → `{html}` — the embedded xterm page;
//!   `transport.js` is injected by the web bundle on serve.
//! - `get_webapp` → `{url, default_width, default_height, title,
//!   header_buttons}` — makes this agent canvas-eligible and declares
//!   the autoscroll chip the iframe drives via the browser bus.
//! - `boot` / `shutdown` → no-op. The terminal_backend (when ported)
//!   owns its own PTY lifecycle.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "terminal_webapp.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Embedded xterm HTML (the JS frontend, served at `/<id>/`).
pub const TERMINAL_HTML: &str = include_str!("index.html");

/// The xterm front-end bundle.
pub struct TerminalWebappBundle;

#[async_trait]
impl Bundle for TerminalWebappBundle {
    fn name(&self) -> &str {
        "terminal_webapp"
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
            "reflect" => {
                let upstream = kernel
                    .agents
                    .get(agent_id)
                    .and_then(|e| {
                        e.meta
                            .read()
                            .expect("meta poisoned")
                            .get("upstream_id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                json!({
                    "id": agent_id.as_str(),
                    "sentence": "xterm UI fronting an upstream terminal backend.",
                    "upstream_id": upstream,
                    "verbs": {
                        "reflect": "Identity + upstream_id binding. No args.",
                        "render_html": "Return the embedded xterm page.",
                        "get_webapp": "Return iframeable URL + viewport defaults + header chips.",
                        "boot": "No-op (terminal_backend owns PTY lifecycle).",
                        "shutdown": "No-op.",
                    }
                })
            }
            "boot" => boot_reply(agent_id, kernel).await,
            "shutdown" => Value::Null,
            "render_html" => json!({"html": TERMINAL_HTML}),
            "get_webapp" => json!({
                "url": format!("/{}/", agent_id),
                "default_width": 600,
                "default_height": 400,
                "title": "xterm",
                "header_buttons": [
                    {
                        "id": "autoscroll",
                        "glyph": "\u{21e3}",
                        "title": "Toggle autoscroll",
                        "toggle": true,
                    },
                ],
            }),
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

/// Boot: idempotently ensure a `terminal_backend` exists as a child of
/// this webapp. If one is already attached (rehydrated from disk after
/// a kernel restart, or created in a prior boot), no-op. Otherwise
/// `create_agent` a fresh backend + record its id in this webapp's
/// `upstream_id` meta field so the page + canvas chrome can locate the
/// pair without traversing the children dict.
///
/// Mirrors Python's terminal_webapp._boot (paired-backend wiring).
async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    const BACKEND_HM: &str = "terminal_backend.tools";

    // Already paired? Either via meta.upstream_id pointing at a live
    // backend, OR a direct child whose handler_module matches.
    let me = match kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
        Some(a) => a,
        None => return Value::Null,
    };
    let existing_upstream = me
        .meta
        .read()
        .expect("meta poisoned")
        .get("upstream_id")
        .and_then(Value::as_str)
        .map(AgentId::from);
    if let Some(up) = existing_upstream.as_ref() {
        if kernel.agents.contains_key(up) {
            return Value::Null;
        }
    }
    let has_backend_child = me.child_ids().iter().any(|cid| {
        kernel
            .agents
            .get(cid)
            .map(|e| e.handler_module.as_deref() == Some(BACKEND_HM))
            .unwrap_or(false)
    });
    if has_backend_child {
        return Value::Null;
    }

    // Create the backend as a child of this webapp.
    let create_reply = kernel
        .send(
            agent_id,
            json!({"type": "create_agent", "handler_module": BACKEND_HM}),
        )
        .await;
    let Some(backend_id) = create_reply.get("id").and_then(Value::as_str) else {
        return json!({"error": format!("terminal_webapp.boot: create backend failed: {create_reply}")});
    };
    let backend_id = backend_id.to_string();

    // Record the pair on this webapp's record.
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
        return json!({"error": format!("terminal_webapp.boot: write upstream_id failed: {err}")});
    }

    Value::Null
}

#[cfg(test)]
mod tests;
