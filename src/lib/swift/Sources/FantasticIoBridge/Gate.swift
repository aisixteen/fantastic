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

/// Gate an inbound WEB-leg action (web_ws / web_rest) at the shared choke point.
/// Unlike `gateVerb` there is NO reflect/boot bypass — a sealed web leg refuses
/// a browser/REST client entirely (the io-leg-as-surface case), so `deny_inbound`
/// denies every verb. `token` is the leg's credential (frame `auth_token` /
/// `X-Fantastic-Auth` header). Returns `nil` to ADMIT, or a deny JSON.
///
/// `kind` is the FRAME KIND — `call`, `emit`, `watch` or `state_subscribe`
/// (python `gate_inbound(rule, kind, frame)`, C41). Only a `call` carries a verb
/// of its own; for every other kind the verb IS the kind. This built `call`
/// unconditionally, so a `watch`/`emit`/`state_subscribe` frame was authorized
/// against a rule clause that names calls.
///
/// A MISSING LEG RECORD IS SEALED, not admitted: `resolveIngress` on an absent
/// rule denies, and a leg that is not in the registry has no rule at all. This
/// returned `nil` (admit) for it, so deleting the leg record OPENED the socket.
public func gateWebLeg(
    kernel: Kernel, legId: AgentId, kind: String, target: String, verb: String, token: String?
) -> JSON? {
    let leg = kernel.agent(legId)
    let rule: IngressRule
    do {
        rule = try resolveIngress(
            ingressRule: leg?.metaValue(forKey: "ingress_rule"),
            auth: leg?.metaValue(forKey: "auth"))
    } catch {
        return .object(["error": .string("\(error)")])
    }
    let action = AuthAction(
        kind: kind, target: target, verb: kind == "call" ? verb : kind, token: token)
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
