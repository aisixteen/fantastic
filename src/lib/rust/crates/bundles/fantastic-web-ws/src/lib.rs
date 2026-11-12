//! WebSocket verb channel — sub-agent of a `web` host.
//!
//! The actual WS server lives inside [`fantastic-web`]; this bundle
//! declares the WS route via the duck-typed `get_routes` verb that the
//! parent `web` agent queries during boot.
//!
//! Verbs:
//! - `reflect` — identity + the URL pattern this surface mounts + leg posture
//! - `get_routes` — declares `{routes:[{kind:"websocket", path:"/{host_id}/ws"}]}`
//!   (the catch-all), or the record's `route` literal when set
//! - `inbound_call` / `inbound_emit` / `inbound_watch` / `inbound_state_subscribe`
//!   — the INBOUND half (`fantastic_io_bridge::inbound`): the socket loop hands
//!   each admitted frame to this leg, which acts AS ITSELF, gated as the leg

#![deny(missing_docs)]

pub mod inbound;
pub mod record;
pub mod verbs;

use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{AgentId, Blob, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "web_ws.tools";

/// The path pattern this leg mounts on its parent `web` host. A literal —
/// python bakes the same `{host_id}` placeholder, and the WS proxy reads the
/// matched segment as the host label. This is the catch-all a leg mounts when
/// its record names no `route`.
pub const PATH_PATTERN: &str = "/{host_id}/ws";

/// What the bundle is for — its one short man text.
const MAN: &str = "The WebSocket call surface of a web host: it mounts ws://host/<id>/ws, and every admitted frame is sent onward as this leg, gated as the leg. Emits socket_open and socket_closed on its inbox.";

/// The WS verb-channel bundle.
pub struct WebWsBundle;

/// Serve one checked call; a raw body rides the inbound verbs.
async fn serve(
    agent_id: &AgentId,
    call: verbs::Call,
    body: Option<Blob>,
    kernel: &Arc<Kernel>,
) -> (Reply, Option<Blob>) {
    match call {
        verbs::Call::Reflect(_) => (Some(reflect_reply(agent_id, kernel)), None),
        verbs::Call::GetRoutes(_) => (
            Some(json!({
                "routes": [{
                    "kind": "websocket",
                    "path": mount_path(agent_id, kernel),
                }]
            })),
            None,
        ),
        verbs::Call::InboundCall(a) => {
            let (v, b) = inbound::call(a, body, kernel).await;
            (Some(v), b)
        }
        verbs::Call::InboundEmit(a) => (Some(inbound::emit(a, body, kernel).await), None),
        verbs::Call::InboundWatch(a) => (Some(inbound::watch(a, kernel).await), None),
        verbs::Call::InboundStateSubscribe(_) => {
            (Some(inbound::state_subscribe(agent_id, kernel)), None)
        }
    }
}

#[async_trait]
impl Handler for WebWsBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        "web_ws"
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

    /// The INBOUND verbs carry a raw body: an admitted frame the transport hands in,
    /// sent onward as this leg.
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

/// The record's optional `route` — a literal mount path (its type and non-emptiness
/// are the record declaration's).
fn route_of(agent_id: &AgentId, kernel: &Kernel) -> Option<String> {
    let a = kernel.agents.get(agent_id)?;
    let meta = a.meta.read().expect("meta poisoned");
    meta.get("route")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The path this leg mounts: its `route` literal, else the catch-all.
fn mount_path(agent_id: &AgentId, kernel: &Kernel) -> String {
    route_of(agent_id, kernel).unwrap_or_else(|| PATH_PATTERN.to_string())
}

/// `reflect` — identity, what the leg is for and its auth posture; the path it
/// mounts is `get_routes`' alone.
fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), json!(agent_id.as_str()));
    out.insert("sentence".into(), json!(MAN));
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
    Value::Object(out)
}

#[cfg(test)]
mod tests;
