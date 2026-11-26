//! `deny_inbound` — the SEAL of a wire leg: refuse every inbound action an
//! EXTERNAL client (an HTTP/WS client, a remote kernel's frame) asks for.
//!
//! A sealed leg denies ALL inbound kinds (`call` / `watch` / `state_subscribe` /
//! `emit` / `open`), not only `call` — it leaks neither dispatch nor telemetry.
//! It never answers an in-kernel agent: who may call an agent inside the kernel
//! is the kernel's `can` lines alone, so the denial teaches no record edit.

use super::super::{Action, Decision, Denial, IngressRule};

/// Refuse every inbound action (the peer can't `call`/`reflect` us): the SEAL.
pub struct DenyInbound;

impl IngressRule for DenyInbound {
    fn authorize(&self, action: &Action) -> Decision {
        // EVERY inbound kind — py-canonical. Denying only `call` would make the
        // seal partial: `watch`/`state_subscribe` would leak telemetry through a
        // "sealed" leg, and a new kind (`spawn`) would sail straight through what
        // its caller believes is a closed edge. A seal that filters by kind is a
        // second truth about what "sealed" means.
        let _ = action;
        Decision::Deny(Denial::new("edge sealed by policy"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(kind: &str) -> Action<'_> {
        Action {
            kind,
            target: "leg",
            verb: "reflect",
            token: None,
        }
    }

    /// The SEALED denial reaches the external caller as one reason, the same
    /// words in every runtime.
    #[test]
    fn a_sealed_leg_states_its_reason() {
        let Decision::Deny(d) = DenyInbound.authorize(&act("call")) else {
            panic!("a sealed leg must deny");
        };
        assert_eq!(d.reason, "edge sealed by policy");
        assert_eq!(d.to_refusal(), serde_json::json!({"error": "edge sealed by policy", "reason": "unauthorized"}));
    }

    /// EVERY kind, not just `call` — a seal that filters by kind is a second
    /// truth about what "sealed" means.
    #[test]
    fn every_inbound_kind_is_sealed() {
        for kind in [
            "call",
            "spawn",
            "watch",
            "unwatch",
            "state_subscribe",
            "emit",
        ] {
            assert!(
                matches!(
                    DenyInbound.authorize(&act(kind)),
                    Decision::Deny(_)
                ),
                "{kind} must be denied by a sealed leg"
            );
        }
    }
}
