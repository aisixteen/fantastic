// The SEAL says python's words, and carries the repair with it.
//
// The same locked door has to say the same thing in every runtime: python's
// `edge sealed by policy` plus a hint naming
// `update_agent <id> ingress_rule=allow_all_ingress`, so a model taught the
// python hint can act on it here.

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

    /// A MALFORMED spec (not a string or an object, or a `type`/`env`
    /// that is not a string) is named by its python repr — a name no registry
    /// holds — so ingress seals on it, egress refuses it, reflect reports it, in
    /// python's words. It threw `unsupported rule spec`, read a non-string `type`
    /// as no rule, and a non-string `env` as the DEFAULT token variable.
    @Test func aMalformedSpecIsNamedByItsPythonRepr() {
        let cases: [(JSON, String)] = [
            (.integer(7), "7"),
            (.array([.string("allow_all_ingress")]), "['allow_all_ingress']"),
            (.object(["type": .array([.string("x")])]), "{'type': ['x']}"),
            (.object(["type": .object(["a": .integer(1)])]), "{'type': {'a': 1}}"),
            (
                .object(["type": .string("password_ingress"), "env": .integer(7)]),
                "{'type': 'password_ingress', 'env': 7}"
            ),
        ]
        for (spec, name) in cases {
            #expect(IngressRules.unknown(spec) == name)
            guard
                case .deny(let d) = IngressRules.resolve(spec).authorize(
                    AuthAction(kind: "call", target: "t", verb: "reflect", token: "x"))
            else {
                Issue.record("a malformed rule must never allow: \(name)")
                continue
            }
            #expect(d.reason == "edge sealed — unknown ingress rule \(pyRepr(name))")
            #expect(d.hint.contains("\(pyRepr(name)) is not an INGRESS rule name"))
            do {
                _ = try EgressRules.resolve(spec)
                Issue.record("egress must refuse a malformed spec: \(name)")
            } catch {
                #expect("\(error)" == "unknown egress rule type \(pyRepr(name))")
            }
            #expect(describe(["ingress_rule": spec])["ingress_rule"] == .string(name))
            #expect(
                IngressRules.unknownRefusal(["ingress_rule": spec])
                    == "ingress_rule=\(pyRepr(spec)) is not a rule this bundle implements — "
                    + "legal values: allow_all_ingress, deny_ingress, password_ingress")
        }
        // null inside a spec is ABSENT, as it is for the whole spec; an empty name
        // is a plain seal (python-falsy).
        #expect(IngressRules.unknown(.object(["type": .null])) == nil)
        let pw = IngressRules.resolve(
            .object(["type": .string("password_ingress"), "env": .null]))
        #expect(pw is IngressRules.Password)
        guard case .deny(let plain) = IngressRules.DenyInbound(unknown: "").authorize(act("call"))
        else {
            Issue.record("an empty rule name must never allow")
            return
        }
        #expect(plain.reason == "edge sealed by policy")
    }
}
