// I-1 — the SEAL says python's words, and carries the repair with it.
//
// The same locked door has to say the same thing in every runtime. Before this,
// python answered `edge sealed by policy` plus a hint naming
// `update_agent <id> ingress_rule=allow_all_ingress`, while swift answered
// `inbound calls denied by policy` with no hint at all — and `gateVerb` bolted on
// one that claimed "the fs edge" for whichever leg had called it. A model taught
// the python hint got nothing it could act on here.

import FantasticIoBridge
import FantasticJSON
import Testing

@Suite("io_bridge deny_inbound: python's seal, verbatim")
struct DenyInboundTests {
    private func act(_ kind: String) -> AuthAction {
        AuthAction(kind: kind, target: "leg", verb: "reflect", token: nil)
    }

    /// Asserted VERBATIM. A paraphrase is the regression this closes.
    @Test func sealedLegTeachesPythonsRepair() {
        guard case .deny(let d) = IngressRules.DenyInbound().authorize(act("call")) else {
            Issue.record("a sealed leg must deny")
            return
        }
        #expect(d.reason == "edge sealed by policy")
        #expect(
            d.hint == "this edge is sealed — open it: update_agent <id> "
                + "ingress_rule=allow_all_ingress (or password); reflect readme=true on "
                + "this agent for the channel model")
        #expect(d.see == "")
    }

    /// And the whole denial reaches the CALLER — reason, code and repair — with
    /// no gate re-wording it on the way out.
    @Test func theDenialReachesTheCallerWhole() {
        guard case .deny(let d) = IngressRules.DenyInbound().authorize(act("call")) else {
            Issue.record("a sealed leg must deny")
            return
        }
        let wire = d.asRefusal
        #expect(wire["error"].asString == "edge sealed by policy")
        #expect(wire["reason"].asString == "unauthorized")
        #expect((wire["hint"].asString ?? "").contains("ingress_rule=allow_all_ingress"))
        // Empty ⇒ absent, so a caller tests for the key, not for "".
        #expect(wire.asObject?["see"] == nil)
    }

    /// EVERY inbound kind — a seal that filters by kind is a second truth about
    /// what "sealed" means.
    @Test func everyInboundKindIsSealed() {
        for kind in ["call", "spawn", "watch", "unwatch", "state_subscribe", "emit"] {
            guard case .deny = IngressRules.DenyInbound().authorize(act(kind)) else {
                Issue.record("\(kind) must be denied by a sealed leg")
                return
            }
        }
    }

    /// An unresolvable rule name seals AND names the legal values, so following
    /// the hint cannot land on the same refusal twice.
    @Test func unknownRuleNamesTheLegalValues() {
        let rule = IngressRules.DenyInbound(unknown: "allow_all")
        guard case .deny(let d) = rule.authorize(act("call")) else {
            Issue.record("an unknown ingress rule must never allow")
            return
        }
        #expect(d.reason == "edge sealed — unknown ingress rule 'allow_all'")
        #expect(d.hint.hasPrefix("this edge is sealed — open it:"))
        #expect(d.hint.contains("'allow_all' is not an INGRESS rule name"))
        #expect(d.hint.contains("allow_all_ingress, deny_ingress, password_ingress"))
    }
}
