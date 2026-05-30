//! REST verb channel — sub-agent of a `web` host.
//!
//! The actual REST routes (POST + `_reflect` GET helpers) are handled
//! inside [`fantastic-web`]'s axum router; this bundle declares the
//! routes via the duck-typed `get_routes` verb that the parent `web`
//! agent queries during boot. The `self_id` is baked into the path
//! literal so several `web_rest` agents under the same `web` parent
//! don't collide.
//!
//! Routes mounted:
//!
//! - `POST /<self_id>/{target_id}` body=payload → kernel.send → JSON
//! - `GET  /<self_id>/_reflect` → kernel.reflect (root tree)
//! - `GET  /<self_id>/_reflect/{target_id}` → reflect on a specific agent
//!
//! Verbs:
//! - `reflect` — `{id, sentence, mounted_on, path_pattern, ...}`
//! - `boot` / `shutdown` — no-op
//! - `get_routes` — declares the surface for mounting

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "web_rest.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// The REST verb-channel bundle.
pub struct WebRestBundle;

#[async_trait]
impl Bundle for WebRestBundle {
    fn name(&self) -> &str {
        "web_rest"
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
        let id_str = agent_id.as_str();
        let reply = match verb {
            "reflect" => {
                let mounted_on = parent_id(agent_id, kernel);
                json!({
                    "id": id_str,
                    "sentence": "REST verb-invocation surface; POST /<self>/<target_id> body=payload.",
                    "mounted_on": mounted_on,
                    "path_pattern": format!("/{id_str}/{{target_id}}"),
                    "method": "POST",
                    "reflect_url": format!("/{id_str}/_reflect"),
                    "reflect_pattern": format!("/{id_str}/_reflect/{{target_id}}"),
                    "verbs": {
                        "reflect": "Identity + parent web agent id + URL patterns. No args.",
                        "boot": "No-op (REST routes come up with the parent web agent).",
                        "shutdown": "No-op.",
                        "get_routes": "Returns {routes:[…]} for the POST + _reflect surface.",
                    }
                })
            }
            "boot" | "shutdown" => Value::Null,
            "get_routes" => json!({
                "routes": [
                    {
                        "kind": "post",
                        "path": format!("/{id_str}/{{target_id}}"),
                    },
                    {
                        "kind": "get",
                        "path": format!("/{id_str}/_reflect"),
                    },
                    {
                        "kind": "get",
                        "path": format!("/{id_str}/_reflect/{{target_id}}"),
                    },
                ]
            }),
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

/// Resolve this surface's parent agent id (the `web` host). Empty
/// string if not parented — see analogous helper in `fantastic-web-ws`.
fn parent_id(agent_id: &AgentId, kernel: &Kernel) -> String {
    kernel
        .agents
        .get(agent_id)
        .and_then(|e| e.parent_id.clone())
        .map(|p| p.0)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
