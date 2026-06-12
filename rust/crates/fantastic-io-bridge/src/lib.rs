//! fantastic-io-bridge — the IO base. A **shared library, NOT a bundle** (no agent,
//! no entry point): the per-leg authorization rule registries that every io derivation
//! (`ws_bridge`/`cloud_bridge`/`web_ws`/`web_rest`/`file_bridge`) imports + calls at one
//! choke point. Mirrors `python/bundled_agents/io/io_bridge`.

pub mod authorizer;
pub mod codec;

use std::sync::Arc;

use authorizer::{Action, Decision, IngressRule};

/// The one inbound choke point: authorize an inbound `Action` against a resolved
/// ingress rule. Every derivation (bridge read-loop, web_ws frame handler, file_bridge
/// verb gate) calls THIS — sealed-by-default lives in `authorizer::ingress::resolve`
/// (absent ⇒ DenyInbound), so a leg with no rule denies here.
pub fn gate_inbound(rule: &Arc<dyn IngressRule>, action: &Action) -> Decision {
    rule.authorize(action)
}
