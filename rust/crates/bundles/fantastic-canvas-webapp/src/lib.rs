//! Spatial UI front-end as an agent.
//!
//! Pairs with a `canvas_backend` (the `upstream_id` on the record) and
//! serves a single HTML page that renders the backend's members as
//! positioned DOM iframes + optional GL views.
//!
//! ## Verbs
//!
//! - `reflect` → `{id, sentence, upstream_id}`
//! - `render_html` → `{html}` — the embedded canvas page;
//!   `transport.js` is injected by the web bundle on serve.
//! - `get_webapp` → `{url, default_width, default_height, title}` —
//!   makes this agent itself canvas-eligible.
//! - `boot` / `shutdown` → no-op.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "canvas_webapp.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Embedded canvas HTML (the JS frontend, served at `/<id>/`).
pub const CANVAS_HTML: &str = include_str!("canvas.html");

/// The canvas front-end bundle.
pub struct CanvasWebappBundle;

#[async_trait]
impl Bundle for CanvasWebappBundle {
    fn name(&self) -> &str {
        "canvas_webapp"
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
                    "sentence": "Spatial UI front-end. Embeds members as iframes.",
                    "upstream_id": upstream,
                    "verbs": {
                        "reflect": "Identity + upstream_id. No args.",
                        "render_html": "Return the embedded canvas page.",
                        "get_webapp": "Return iframeable URL + viewport defaults.",
                        "boot": "No-op.",
                        "shutdown": "No-op.",
                    }
                })
            }
            "boot" => boot_reply(agent_id, kernel).await,
            "shutdown" => Value::Null,
            "render_html" => json!({"html": CANVAS_HTML}),
            "get_webapp" => json!({
                "url": format!("/{}/", agent_id),
                "default_width": 800,
                "default_height": 600,
                "title": "canvas",
            }),
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

/// Boot: idempotently ensure a `canvas_backend` exists as a child of
/// this webapp + bind its id into `upstream_id` meta. Mirrors
/// `python/bundled_agents/canvas/canvas_webapp/.../tools.py::_boot`
/// and the symmetric `terminal_webapp::boot_reply` we already ship.
///
/// Without this, dropping a fresh `canvas_webapp` into a workdir
/// gives the user a canvas page that can't accept members — `cw`'s
/// dblclick handler routes through the missing upstream.
async fn boot_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    const BACKEND_HM: &str = "canvas_backend.tools";

    let me = match kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
        Some(a) => a,
        None => return Value::Null,
    };
    // Already bound? upstream_id pointing at a live canvas_backend → no-op.
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
    // Or a backend already attached as a child (rehydrated from disk)?
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
    // Spawn one as our child.
    let create_reply = kernel
        .send(
            agent_id,
            json!({"type": "create_agent", "handler_module": BACKEND_HM}),
        )
        .await;
    let Some(backend_id) = create_reply.get("id").and_then(Value::as_str) else {
        return json!({"error": format!("canvas_webapp.boot: create backend failed: {create_reply}")});
    };
    let backend_id = backend_id.to_string();
    // Record the binding on this webapp's record so the page + canvas
    // chrome can discover the pair without walking the children dict.
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
        return json!({"error": format!("canvas_webapp.boot: write upstream_id failed: {err}")});
    }
    Value::Null
}

#[cfg(test)]
mod tests;
