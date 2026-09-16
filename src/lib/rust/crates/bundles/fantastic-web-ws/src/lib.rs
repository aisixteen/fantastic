//! WebSocket verb channel — sub-agent of a `web` host.
//!
//! The actual WS server lives inside [`fantastic-web`]; this bundle
//! declares the WS route via the duck-typed `get_routes` verb that the
//! parent `web` agent queries during boot.
//!
//! Verbs:
//! - `reflect` — identity + the URL pattern this surface mounts + leg posture
//! - `boot` — no-op; the parent web agent owns the route
//! - `get_routes` — declares `{routes:[{kind:"websocket", path:"/{host_id}/ws"}]}`

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::py_repr_value;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{refusal, with, UNHANDLED};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "web_ws.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// The path pattern this leg mounts on its parent `web` host. A literal —
/// python bakes the same `{host_id}` placeholder, and the WS proxy reads the
/// matched segment as the host label.
pub const PATH_PATTERN: &str = "/{host_id}/ws";

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
            "reflect" => reflect_reply(agent_id, kernel),
            // python's VERBS has no `shutdown`; an unknown type is refused.
            "boot" => Value::Null,
            "get_routes" => json!({
                "routes": [{
                    "kind": "websocket",
                    "path": PATH_PATTERN,
                }]
            }),
            _ => {
                let raw = payload.get("type").cloned().unwrap_or(Value::Null);
                let mut r = refusal(
                    UNHANDLED,
                    format!("web_ws: unknown type {}", py_repr_value(&raw)),
                );
                with(&mut r, "verb", raw);
                r
            }
        };
        Ok(Some(reply))
    }
}

/// python `web_ws/tools.py::_reflect` — identity, the mounted URL pattern and
/// this leg's auth posture, in python's key order.
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), json!(agent_id.as_str()));
    out.insert(
        "sentence".into(),
        json!("WS verb-invocation surface; mounts /<host_id>/ws on the parent web."),
    );
    out.insert("path_pattern".into(), json!(PATH_PATTERN));
    // The leg is a CHANNEL — surface its auth posture so discovery-through-denial
    // has something to discover (the door + the pointer to how to open it).
    let meta = kernel
        .agents
        .get(agent_id)
        .map(|a| a.meta.read().expect("meta poisoned").clone())
        .unwrap_or_default();
    if let Some(posture) = fantastic_io_bridge::authorizer::describe(&meta).as_object() {
        for (k, v) in posture {
            out.insert(k.clone(), v.clone());
        }
    }
    out.insert(
        "verbs".into(),
        json!({
            "reflect": "Identity + the URL pattern this surface mounts + this leg's auth posture",
            "boot": "No-op. The parent `web` agent calls `get_routes` on this bundle during its own boot and mounts the WS endpoint onto its FastAPI app.",
            "get_routes": "Declares this surface's HTTP/WS route mounts. Returns",
        }),
    );
    Value::Object(out)
}

#[cfg(test)]
mod tests;
