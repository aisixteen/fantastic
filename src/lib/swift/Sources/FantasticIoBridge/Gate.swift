// The one inbound choke point + the file_bridge-style verb gate.
//
// Swift mirror of py/rust `gate_inbound`: every io derivation (ws_bridge /
// relay_connector / web_ws / web_rest / file_bridge) calls THIS at one place to
// authorize an inbound action. Sealed-by-default lives in
// `IngressRules.resolve` (absent ⇒ DenyInbound), so a leg with no rule denies.

import FantasticJSON
import FantasticKernel
import Foundation

/// Authorize an inbound `AuthAction` against a resolved ingress rule.
public func gateInbound(rule: IngressRule, action: AuthAction) -> AuthDecision {
    rule.authorize(action)
}

/// Gate an inbound WEB-leg call (web_ws / web_rest) at the shared choke point.
/// Unlike `gateVerb` there is NO reflect/boot bypass — a sealed web leg refuses
/// a browser/REST client entirely (the io-leg-as-surface case), so `deny_inbound`
/// denies every verb. `token` is the leg's credential (frame `auth_token` /
/// `X-Fantastic-Auth` header). Returns `nil` to ADMIT, or a deny JSON.
public func gateWebLeg(
    kernel: Kernel, legId: AgentId, target: String, verb: String, token: String?
) -> JSON? {
    guard let leg = kernel.agent(legId) else { return nil }
    let rule: IngressRule
    do {
        rule = try resolveIngress(
            ingressRule: leg.metaValue(forKey: "ingress_rule"), auth: leg.metaValue(forKey: "auth"))
    } catch {
        return .object(["error": .string("\(error)")])
    }
    let action = AuthAction(kind: "call", target: target, verb: verb, token: token)
    // FORWARDED, not re-worded. The rule states the reason AND the repair; a web
    // leg used to drop the repair entirely, so a browser/REST client hitting a
    // sealed leg learned only that it was refused.
    if case .deny(let d) = gateInbound(rule: rule, action: action) {
        return d.asRefusal
    }
    return nil
}

/// The fs-edge GATE — gate a verb on a leg given its rule meta (`ingress_rule` /
/// the legacy `auth` shorthand + the envelope `auth_token`). `reflect` — and ONLY
/// `reflect` — bypasses the gate, so a SEALED bridge is still discoverable:
/// python's `UNGATED` set holds that one verb, so a sealed `boot` is denied like
/// any other call. Returns `nil` to ADMIT, or a deny JSON
/// `{error, reason:"unauthorized", hint}`. Used by `file_bridge` (and the web
/// legs) — the same sealed-by-default choke point as the cross-kernel bridges.
public func gateVerb(
    ingressRule: JSON?,
    auth: JSON?,
    authToken: String?,
    agentId: String,
    verb: String
) -> JSON? {
    if verb == "reflect" { return nil }
    let rule: IngressRule
    do {
        rule = try resolveIngress(ingressRule: ingressRule, auth: auth)
    } catch {
        return .object(["error": .string("\(error)")])
    }
    let action = AuthAction(kind: "call", target: agentId, verb: verb, token: authToken)
    // FORWARDED, not re-worded. This hint was hardcoded to say "the fs edge" for
    // whichever leg called `gateVerb` — a web leg's denial claimed a filesystem
    // edge — and it named a repair the rule itself already knows how to state.
    if case .deny(let d) = gateInbound(rule: rule, action: action) {
        return d.asRefusal
    }
    return nil
}
