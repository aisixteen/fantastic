//! `deny_inbound` — the SEAL: refuse every inbound action, and teach how to open.
//!
//! A sealed edge denies ALL inbound kinds (`call` / `watch` / `state_subscribe` /
//! `emit` / `open`), not only `call` — a sealed leg leaks neither dispatch nor
//! telemetry. Every denial carries a teaching pointer (`hint`) so an automated
//! builder hitting the locked door learns to open it (set an `ingress_rule`, read
//! the io_bridge readme) instead of dead-ending. Discovery-through-denial is the
//! design.

use fantastic_bundle::py_repr;

use super::super::{Action, Decision, Denial, IngressRule};

/// The repair, WORD FOR WORD as python's `DenyInbound` states it
/// (`io_bridge/ingress_rules/deny_inbound.py`). The same locked door has to say
/// the same thing in every runtime: a model taught the python hint got nothing
/// it could act on from the ports, which had `inbound calls denied by policy`
/// and no hint at all.
const OPEN_IT: &str = "this edge is sealed — open it: update_agent <id> \
     ingress_rule=allow_all_ingress (or password); reflect readme=true on this \
     agent for the channel model";

/// Refuse every inbound `call` (the peer can't `call`/`reflect` us). Inbound
/// `watch`/`unwatch` are already ignored by the read loop ⇒ denied-by-omission.
///
/// `unknown` carries a rule name that could not be resolved. An edge configured with
/// a rule nobody implements is NOT authorized — it seals, and every denial says which
/// name was wrong. Mirrors python's `DenyInbound(unknown=…)`.
#[derive(Default)]
pub struct DenyInbound {
    pub unknown: Option<String>,
}

impl DenyInbound {
    /// The plain seal — no rule was set at all.
    pub fn sealed() -> Self {
        Self { unknown: None }
    }

    /// Sealed BECAUSE the configured rule name is not implemented.
    pub fn unknown(name: &str) -> Self {
        Self {
            unknown: Some(name.to_string()),
        }
    }
}

impl IngressRule for DenyInbound {
    fn authorize(&self, action: &Action) -> Decision {
        // EVERY inbound kind — py-canonical. This used to deny only `call`,
        // which made the rust seal partial: `watch`/`state_subscribe` leaked
        // telemetry through a "sealed" leg, and a new kind (`spawn`, measured
        // 2026-08-07 porting exec_bridge) sailed straight through what its
        // caller believed was a closed edge. A seal that filters by kind is a
        // second truth about what "sealed" means.
        let _ = action;
        // PYTHON'S WORDS, both variants. The reason says WHAT happened, the hint
        // says how to open it, and the unknown-rule variant appends the legal
        // names — so a caller that followed a wrong name is handed the right ones
        // instead of the same wall twice.
        // The name is python's `{unknown!r}` — a malformed spec's name is its
        // repr, quotes and all — and an EMPTY name is python-falsy: a plain seal.
        Decision::Deny(match self.unknown.as_deref().filter(|n| !n.is_empty()) {
            Some(n) => {
                let n = py_repr(n);
                Denial::new(format!("edge sealed — unknown ingress rule {n}")).with_hint(format!(
                    "{OPEN_IT}. {n} is not an INGRESS rule name — the legal values \
                     are allow_all_ingress, deny_ingress, password_ingress (names are \
                     manifold-scoped: an egress name here is a typo, not a policy)"
                ))
            }
            None => Denial::new("edge sealed by policy").with_hint(OPEN_IT),
        })
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
        let Decision::Deny(d) = DenyInbound::sealed().authorize(&act("call")) else {
            panic!("a sealed leg must deny");
        };
        assert_eq!(d.reason, "edge sealed by policy");
        assert_eq!(
            d.hint,
            "this edge is sealed — open it: update_agent <id> \
             ingress_rule=allow_all_ingress (or password); reflect readme=true on \
             this agent for the channel model"
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
                    DenyInbound::sealed().authorize(&act(kind)),
                    Decision::Deny(_)
                ),
                "{kind} must be denied by a sealed leg"
            );
        }
    }

    /// An unresolvable rule name seals AND names the legal values, so following
    /// the hint cannot land on the same refusal twice.
    #[test]
    fn unknown_rule_names_the_legal_values() {
        let Decision::Deny(d) = DenyInbound::unknown("allow_all").authorize(&act("call")) else {
            panic!("an unknown ingress rule must never allow");
        };
        assert_eq!(d.reason, "edge sealed — unknown ingress rule 'allow_all'");
        assert!(d.hint.starts_with("this edge is sealed — open it:"));
        assert!(d.hint.contains("'allow_all' is not an INGRESS rule name"));
        assert!(d
            .hint
            .contains("allow_all_ingress, deny_ingress, password_ingress"));
    }

    /// A MALFORMED spec seals under its python repr — `{unknown!r}`, so a name
    /// holding `'` is double-quoted exactly as python prints it — and an empty
    /// name is a plain seal (python-falsy). Audit W-8.
    #[test]
    fn a_malformed_spec_seals_under_its_python_repr() {
        let rule = super::super::resolve(Some(&serde_json::json!({"type": ["x"]})));
        let Decision::Deny(d) = rule.authorize(&act("call")) else {
            panic!("a malformed rule must never allow");
        };
        assert_eq!(
            d.reason,
            "edge sealed — unknown ingress rule \"{'type': ['x']}\""
        );
        assert!(d
            .hint
            .contains("\"{'type': ['x']}\" is not an INGRESS rule name"));
        let Decision::Deny(d) = DenyInbound::unknown("").authorize(&act("call")) else {
            panic!("an empty rule name must never allow");
        };
        assert_eq!(d.reason, "edge sealed by policy");
    }
}
