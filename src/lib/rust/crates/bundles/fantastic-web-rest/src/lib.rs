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
//!   item `id`); handed to this leg (`inbound_call`), which sends it onward AS
//!   ITSELF
//! - `GET  /<self_id>/_reflect` → kernel.reflect (root tree)
//! - `GET  /<self_id>/_reflect/{target_id}` → reflect on a specific agent
//!
//! GATE FIRST, THEN THE BODY. A POST is judged by the
//! leg's ingress rule on what the request says up front — its target and its
//! `X-Fantastic-Auth` credential — and only an admitted request's body is read,
//! at most 16 MiB (io_bridge's `MAX_FRAME`, the one frame cap every wire leg
//! holds); a larger body is refused 413, loud, with nothing past the bound read (a
//! declared `Content-Length` over it is refused unread). Implemented by the
//! parent web's router (`fantastic-web`'s `rest_post`).
//!
//!
//! Verbs: `reflect`, `get_routes` (the surface for mounting) and `inbound_call`.

#![deny(missing_docs)]

pub mod record;
pub mod verbs;

use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{AgentId, Blob, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "web_rest.tools";

/// What the bundle is for — its one short man text.
const MAN: &str = "The REST call surface of a web host: POST /<id>/<target> sends the body onward as this leg, gated as the leg; GET /<id>/_reflect[/<target>] reflects. Sealed by default; the token rides the X-Fantastic-Auth header.";

/// The REST verb-channel bundle.
pub struct WebRestBundle;

/// Serve one checked call.
async fn serve(
    agent_id: &AgentId,
    call: verbs::Call,
    body: Option<Blob>,
    kernel: &Arc<Kernel>,
) -> (Reply, Option<Blob>) {
    match call {
        verbs::Call::Reflect(_) => (Some(reflect_reply(agent_id, kernel)), None),
        verbs::Call::GetRoutes(_) => (Some(get_routes(agent_id)), None),
        verbs::Call::InboundCall(a) => {
            let (v, b) = fantastic_web_ws::inbound::call(a, body, kernel).await;
            (Some(v), b)
        }
    }
}

#[async_trait]
impl Handler for WebRestBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        "web_rest"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    /// The leg's `ingress_rule`, refused at the WRITE when no rule implements it
    /// — the one check every leg makes (python `validate_meta` →
    /// `io_bridge.unknown_ingress_refusal`).
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        fantastic_io_bridge::authorizer::ingress::unknown_refusal(
            meta,
            &fantastic_io_bridge::authorizer::ingress::REGISTRY,
        )
    }

    /// `inbound_call` — an admitted request the HTTP server hands in, sent onward
    /// as this leg, a raw body riding either way.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        Ok(serve(agent_id, call, body, kernel).await)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(serve(agent_id, call, None, kernel).await.0)
    }
}

/// python `web_rest/tools.py::_reflect` — identity + URL patterns
/// + this leg's auth posture, in python's key order.
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
