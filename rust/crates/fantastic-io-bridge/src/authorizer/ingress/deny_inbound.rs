//! `deny_inbound` — one-way / hub→spoke push: refuse every inbound `call`.

use super::super::{Action, Decision, IngressRule};

/// Refuse every inbound `call` (the peer can't `call`/`reflect` us). Inbound
/// `watch`/`unwatch` are already ignored by the read loop ⇒ denied-by-omission.
pub struct DenyInbound;

impl IngressRule for DenyInbound {
    fn authorize(&self, action: &Action) -> Decision {
        if action.kind == "call" {
            Decision::Deny("inbound calls denied by policy".to_string())
        } else {
            Decision::Allow
        }
    }
}
