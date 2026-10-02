//! REST verb channel — sub-agent of a `web` host.
//!
//! The actual REST routes (POST + `_reflect` GET helpers) are handled
//! inside [`fantastic-web`]'s router; this bundle declares the
//! routes via the duck-typed `get_routes` verb that the parent `web`
//! agent queries during boot. The `self_id` is baked into the path
//! literal so several `web_rest` agents under the same `web` parent
//! don't collide.
//!
//! Routes mounted:
//!
//! - `POST /<self_id>/{target_id}` body=payload → kernel.send → JSON;
//!   headers `X-Fantastic-Auth` (credential), `X-Fantastic-Execution-Id` (the
//!   item `id`), `X-Fantastic-Secret` (the item `secret`, a JSON array of paths)
//! - `GET  /<self_id>/_reflect` → kernel.reflect (root tree)
//! - `GET  /<self_id>/_reflect/{target_id}` → reflect on a specific agent
//!
//! Verbs:
//! - `reflect` — identity + URL patterns + curl examples + leg posture
//! - `boot` — no-op
//! - `get_routes` — declares the surface for mounting

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle::py_repr_value;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{refusal, with, UNHANDLED};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
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
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, kernel),
            // python's VERBS has no `shutdown`; an unknown type is refused.
            "boot" => Value::Null,
            "get_routes" => get_routes(agent_id),
            _ => {
                let raw = payload.get("type").cloned().unwrap_or(Value::Null);
                let mut r = refusal(
                    UNHANDLED,
                    format!("web_rest: unknown type {}", py_repr_value(&raw)),
                );
                with(&mut r, "verb", raw);
                r
            }
        };
        Ok(Some(reply))
    }
}

/// python `web_rest/tools.py::_reflect` — identity + URL patterns + curl
/// examples + this leg's auth posture, in python's key order.
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let id = agent_id.as_str();
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    out.insert(
        "sentence".into(),
        json!("REST verb-invocation surface; POST /<self>/<target_id> body=payload."),
    );
    out.insert("path_pattern".into(), json!(format!("/{id}/{{target_id}}")));
    out.insert("method".into(), json!("POST"));
    out.insert("reflect_url".into(), json!(format!("/{id}/_reflect")));
    out.insert(
        "reflect_pattern".into(),
        json!(format!("/{id}/_reflect/{{target_id}}")),
    );
    out.insert("auth_header".into(), json!("X-Fantastic-Auth"));
    out.insert(
        "curl_post".into(),
        json!(format!(
            "curl -X POST -H \"content-type: application/json\" -d '{{\"type\":\"reflect\"}}' http://<host>/{id}/<target_id>"
        )),
    );
    out.insert(
        "curl_reflect".into(),
        json!(format!(
            "curl http://<host>/{id}/_reflect           # root reflect (tree)"
        )),
    );
    out.insert(
        "curl_reflect_target".into(),
        json!(format!(
            "curl http://<host>/{id}/_reflect/<target_id>  # any agent"
        )),
    );
    // The REST face is a CHANNEL — surface its auth posture so a denied client
    // can find the door (discovery-through-denial).
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
            "reflect": "Identity + URL patterns + curl examples + this leg's auth posture",
            "boot": "No-op. The parent `web` agent calls `get_routes` on this bundle during its own boot and mounts the HTTP endpoint onto its FastAPI app.",
            "get_routes": "Declares this surface's HTTP routes. No args. Returns",
        }),
    );
    Value::Object(out)
}

/// python `web_rest/tools.py::_get_routes` — the duck-typed mount surface.
/// Python's specs also carry an `endpoint` callable; rust mounts by
/// `(kind, method, path)` so the callable has no wire spelling.
fn get_routes(agent_id: &AgentId) -> Value {
    let id = agent_id.as_str();
    json!({
        "routes": [
            {
                "kind": "http",
                "method": "POST",
                "path": format!("/{id}/{{target_id}}"),
            },
            {
                "kind": "http",
                "method": "GET",
                "path": format!("/{id}/_reflect"),
            },
            {
                "kind": "http",
                "method": "GET",
                "path": format!("/{id}/_reflect/{{target_id}}"),
            },
        ]
    })
}

#[cfg(test)]
mod tests;
