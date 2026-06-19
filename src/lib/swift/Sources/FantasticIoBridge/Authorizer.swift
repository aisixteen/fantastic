// Bridge authorization — base types + the two rule registries (ingress/egress).
//
// Swift mirror of the canonical Python `bridge_core` (ingress_rules / egress_rules
// packages) and the Rust `authorizer` module. Two independent, TYPED rules govern a
// leg, mirrored on the wire and enforced on the RECEIVER:
//   - an INGRESS rule (the inbound FILTER): `authorize(action) -> AuthDecision`.
//   - an EGRESS rule (the outbound DECORATOR): `credential() -> token?`, stamped on
//     the frame ENVELOPE by `forward` (never the dispatched payload).
//
// Each rule is typed in the record (`{"type": <name>, "env": <var>}`) and resolved
// BY NAME from a registry — the `IngressRules` / `EgressRules` namespaces (a folder
// of one-rule-per-file, the `resolve` switch is the importer). The record carries
// `ingress_rule`/`egress_rule` (symmetric) or the legacy `auth` shorthand (both).
//
// SWIFT SPECIFIC: there is no shared read loop. The ONLY inbound-`call` dispatcher is
// `RelayTransport.dispatch`, so an ingress rule is enforced on the relay_connector
// leg only (correct-by-construction; ws is an asymmetric client, in-memory forward is
// a direct kernel call). The relay e2e matrix covers Swift's real inbound path.
// Rules are TRANSITIONAL (inline plumbing), not invocational (agents).

import FantasticJSON
import Foundation

/// Default env var for the `password` rule when the record names none.
public let DefaultGroupTokenEnv = "FANTASTIC_GROUP_TOKEN"

/// One inbound request the peer is asking this leg to perform locally.
public struct AuthAction: Sendable {
    /// `"call"` (gated) | `"watch"` | `"unwatch"`.
    public let kind: String
    /// The local agent id the peer addressed.
    public let target: String
    /// `payload["type"]` — the verb requested (e.g. `"reflect"`).
    public let verb: String
    /// The `auth_token` the peer attached on the frame ENVELOPE, if any (read by the
    /// `password` rule; the dispatched payload never carries it).
    public let token: String?

    public init(kind: String, target: String, verb: String, token: String? = nil) {
        self.kind = kind
        self.target = target
        self.verb = verb
        self.token = token
    }
}

/// An ingress rule's verdict on an `AuthAction`.
public enum AuthDecision: Sendable {
    /// Permit the inbound action — dispatch `kernel.send`.
    case allow
    /// Refuse it; the dispatcher replies `{error, reason:"unauthorized"}`.
    case deny(String)
}

/// The inbound FILTER — decides whether the peer may perform an inbound action.
public protocol IngressRule: Sendable {
    func authorize(_ action: AuthAction) -> AuthDecision
}

/// The outbound DECORATOR — the token this leg PRESENTS on its own outbound `call`s
/// (stamped on the frame envelope by `forward`). `nil` ⇒ present nothing.
public protocol EgressRule: Sendable {
    func credential() -> String?
}

/// Thrown when a rule spec names an unknown type or is malformed — fails the boot
/// loudly rather than silently mis-securing.
public struct AuthPolicyError: Error, CustomStringConvertible {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}

/// Normalize a rule spec to `(type name, env var)`. Absent/null/empty ⇒ `(nil, nil)`.
/// String ⇒ `(name, nil)`. Object ⇒ `(type|policy, env|token_env)`.
func parseSpec(_ spec: JSON?) throws -> (name: String?, tokenEnv: String?) {
    guard let spec else { return (nil, nil) }
    switch spec {
    case .null:
        return (nil, nil)
    case .string(let s):
        return s.isEmpty ? (nil, nil) : (s, nil)
    case .object(let o):
        guard let name = o["type"]?.asString ?? o["policy"]?.asString else {
            throw AuthPolicyError("rule object missing 'type'")
        }
        return (name, o["env"]?.asString ?? o["token_env"]?.asString)
    default:
        throw AuthPolicyError("unsupported rule spec")
    }
}

/// The rule TYPE name for reflect — never surfaces the rule's config. Absent ⇒
/// `def` (`allow_all` for ingress, `silent` for egress).
public func ruleName(_ spec: JSON?, default def: String) -> String {
    switch spec {
    case .some(.string(let s)) where !s.isEmpty: return s
    case .some(.object(let o)): return o["type"]?.asString ?? o["policy"]?.asString ?? def
    default: return def
    }
}

/// Content-blind constant-time compare given equal length (length is not secret —
/// same posture as Python's `hmac.compare_digest`).
func constantTimeEquals(_ a: String, _ b: String) -> Bool {
    let x = Array(a.utf8)
    let y = Array(b.utf8)
    if x.count != y.count { return false }
    var diff: UInt8 = 0
    for i in 0..<x.count { diff |= x[i] ^ y[i] }
    return diff == 0
}

/// Resolve the leg's INGRESS rule: `ingress_rule` if present, else the legacy `auth`
/// shorthand. Absent ⇒ AllowAll (back-compat).
public func resolveIngress(ingressRule: JSON?, auth: JSON?) throws -> IngressRule {
    try IngressRules.resolve(ingressRule ?? auth)
}

/// Resolve the leg's EGRESS rule: `egress_rule` if present, else the legacy `auth`
/// shorthand (so `auth:"password"` presents the group token). Absent ⇒ Silent.
public func resolveEgress(egressRule: JSON?, auth: JSON?) throws -> EgressRule {
    try EgressRules.resolve(egressRule ?? auth)
}
