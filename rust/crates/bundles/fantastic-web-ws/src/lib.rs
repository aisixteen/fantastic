//! WebSocket verb channel — sub-agent of a `web` host.
//!
//! The actual WS server lives inside [`fantastic-web`]; this bundle
//! declares the WS route via the duck-typed `get_routes` verb that the
//! parent `web` agent queries during boot (and again on any
//! `routes_changed` emit). Adding / removing a `web_ws` child of a
//! running `web` agent hot-(un)mounts the endpoint without restarting
//! axum.
//!
//! Verbs:
//! - `reflect` — `{id, sentence, mounted_on, path_pattern}`
//! - `boot` / `shutdown` — no-op; the parent web agent owns the route
//! - `get_routes` — declares `{routes:[{kind:"websocket", path}]}`
//!   where `path` is `/<parent_id>/ws`

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "web_ws.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// The WS verb-channel bundle.
pub struct WebWsBundle;

#[async_trait]
impl Bundle for WebWsBundle {
    fn name(&self) -> &str {
        "web_ws"
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
                let mounted_on = parent_id(agent_id, kernel);
                let path_pattern = ws_path(&mounted_on);
                json!({
                    "id": agent_id.as_str(),
                    "sentence": "WS verb-invocation surface; mounts /<parent_id>/ws on the parent web.",
                    "mounted_on": mounted_on,
                    "path_pattern": path_pattern,
                    "verbs": {
                        "reflect": "Identity + parent web agent id + URL pattern. No args.",
                        "boot": "No-op (the WS endpoint comes up with the parent web agent).",
                        "shutdown": "No-op.",
                        "get_routes": "Returns {routes:[{kind:'websocket', path:'/<parent>/ws'}]}.",
                    }
                })
            }
            "boot" | "shutdown" => Value::Null,
            "get_routes" => {
                let mounted_on = parent_id(agent_id, kernel);
                json!({
                    "routes": [{
                        "kind": "websocket",
                        "path": ws_path(&mounted_on),
                    }]
                })
            }
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }
}

/// Resolve this surface's parent agent id (the `web` host). Empty
/// string if the agent has no parent_id (shouldn't happen in practice
/// — surface bundles only make sense as children of a `web` agent —
/// but we tolerate it so `reflect` returns gracefully).
fn parent_id(agent_id: &AgentId, kernel: &Kernel) -> String {
    kernel
        .agents
        .get(agent_id)
        .and_then(|e| e.parent_id.clone())
        .map(|p| p.0)
        .unwrap_or_default()
}

/// Compose the WS path from the parent web agent's id. Format matches
/// Python's `/<host_id>/ws`. The parent agent id is the path's host
/// WS route is a wildcard: `/{host_id}/ws` matches any path segment +
/// `/ws`. This mirrors Python's web_ws — transport.js running on any
/// served page (e.g. `/cw/`, `/chat/`, `/tw/`) connects to
/// `/<that-page-id>/ws` and the same handler accepts it. The path
/// param is used as a synthetic host_id label for telemetry; the
/// underlying WS proxy doesn't care which agent the page was for.
///
/// `parent` argument retained so reflect can still report `mounted_on`
/// — actual mount path is the wildcard.
fn ws_path(_parent: &str) -> String {
    "/{host_id}/ws".to_string()
}

#[cfg(test)]
mod tests;
