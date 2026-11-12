//! `deny_inbound` — the SEAL: refuse every inbound action, and teach how to open.
//!
//! A sealed edge denies ALL inbound kinds (`call` / `watch` / `state_subscribe` /
//! `emit` / `open`), not only `call` — a sealed leg leaks neither dispatch nor
//! telemetry. Every denial carries a teaching pointer (`hint`) so an automated
//! builder hitting the locked door learns to open it (set an `ingress_rule`, read
//! the io_bridge readme) instead of dead-ending. Discovery-through-denial is the
//! design.

use super::super::{Action, Decision, Denial, IngressRule};

/// The repair, WORD FOR WORD as python's `DenyInbound` states it
/// (`io_bridge/ingress_rules/deny_inbound.py`). The same locked door has to say
/// the same thing in every runtime: a model taught the python hint got nothing
/// it could act on from the ports, which had `inbound calls denied by policy`
/// and no hint at all.
const OPEN_IT: &str = "this edge is sealed — open it: update_agent <id> \
     ingress_rule=allow_all_ingress (or password); `reflect` on this agent \
     lists its verbs and record fields";

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
        Decision::Deny(Denial::new("edge sealed by policy").with_hint(OPEN_IT))
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

    /// The SEALED denial reaches the caller with python's words — reason AND the
    /// repair. Asserted verbatim: the whole point of #71/#115 is that the same
    /// locked door says the same thing in every runtime, so a paraphrase here is
    /// the regression.
    #[test]
    fn sealed_leg_teaches_python_s_repair() {
        let Decision::Deny(d) = DenyInbound.authorize(&act("call")) else {
            panic!("a sealed leg must deny");
        };
        assert_eq!(d.reason, "edge sealed by policy");
        assert_eq!(
            d.hint,
            "this edge is sealed — open it: update_agent <id> \
             ingress_rule=allow_all_ingress (or password); `reflect` on this agent \
             lists its verbs and record fields"
        );
        assert_eq!(d.see, "");
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
