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
// `ingress_rule`/`egress_rule` (symmetric) or the `auth` shorthand (both directions).
//
// SWIFT SPECIFIC: there is no shared read loop. The ONLY inbound-`call` dispatcher is
// `RelayTransport.dispatch`, so an ingress rule is enforced on the relay_connector
// leg only (correct-by-construction; ws is an asymmetric client, in-memory forward is
// a direct kernel call). The relay e2e matrix covers Swift's real inbound path.
// Rules are TRANSITIONAL (inline plumbing), not invocational (agents).

import FantasticJSON
import Foundation
import OrderedCollections

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

/// Everything a refusal carries — the denial half of python's `Decision`, field
/// for field.
///
/// It used to be a bare `String`, so a rule could state a fact and nothing else:
/// every gate that received one invented its own `hint` at the call site (the fs
/// gate said "the fs edge is sealed" for whichever leg called it, web legs said
/// nothing at all) and the three runtimes drifted into three vocabularies for the
/// same `unauthorized` code. The rule KNOWS why it refused and how to open the
/// edge; that knowledge travels with the verdict instead of being guessed
/// downstream.
public struct Denial: Sendable {
    /// One line a person reads — the `error` field on the wire.
    public let reason: String
    /// The repair: what to do instead. Empty ⇒ the field is omitted.
    public let hint: String
    /// A doc pointer (readme key). Empty ⇒ the field is omitted.
    public let see: String

    public init(_ reason: String, hint: String = "", see: String = "") {
        self.reason = reason
        self.hint = hint
        self.see = see
    }

    /// The wire shape every inbound gate answers a denial with. Built HERE so no
    /// call site has to remember the key order or invent the teaching half.
    public var asRefusal: JSON {
        var out: JSON = .object([
            "error": .string(reason), "reason": .string("unauthorized"),
        ])
        // Empty ⇒ ABSENT, not `""` — the same rule `Refusal.make` follows, so a
        // caller can test for the key instead of testing for an empty string.
        if !hint.isEmpty { out["hint"] = .string(hint) }
        if !see.isEmpty { out["see"] = .string(see) }
        return out
    }
}

/// An ingress rule's verdict on an `AuthAction`.
public enum AuthDecision: Sendable {
    /// Permit the inbound action — dispatch `kernel.send`.
    case allow
    /// Refuse it; the dispatcher replies with the `Denial`'s own fields
    /// (`{error, reason:"unauthorized", hint?, see?}`), never its own wording.
    case deny(Denial)
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

/// Normalize a rule spec to `(type name, env var)` — python `parse_spec`.
///
/// Python-falsy (absent, null, `""`, `false`, `0`, `[]`, `{}`) ⇒ `(nil, nil)`. A
/// string ⇒ `(name, nil)`. An object ⇒ `(type, env)`; an object with no `type`
/// has no name, which every resolver reads as a plain seal — python seals it
/// (`edge sealed by policy`), where this used to throw out of `handle`.
func parseSpec(_ spec: JSON?) throws -> (name: String?, tokenEnv: String?) {
    guard let spec, pyTruthy(spec) else { return (nil, nil) }
    switch spec {
    case .string(let s):
        return (s, nil)
    case .object(let o):
        // `type` and `env` are the only spellings. The `policy` / `token_env` aliases
        // were swift+rust only — python's parse_spec never accepted them — so they gave
        // one wire key two names and are deleted.
        return (o["type"]?.asString, o["env"]?.asString)
    default:
        throw AuthPolicyError("unsupported rule spec")
    }
}

/// The effective spec for ONE manifold, with the `auth` shorthand already SCOPED.
///
/// Rule names are manifold-scoped: a name binds to exactly one direction, so a name
/// written into the wrong field is a loud error instead of a silent policy change.
/// Measured 2026-08-21 (python, and this runtime had the identical defect): with bare
/// names, `auth:"silent"` — a legitimate EGRESS policy — resolved ingress to
/// `DenyInbound(unknown: "silent")`, SEALING the leg while saying nothing; and
/// `auth:"allow_all"` handed egress a `Silent` nobody asked for. One unscoped name
/// against two registries with different membership is a fallback, and this kernel does
/// not have fallbacks.
///
/// The per-direction field WINS and is taken as written. `auth` carries a bare STEM and
/// each manifold appends its own suffix — except that an inbound-only stem presents
/// nothing outbound, a MEANING spelled out per stem rather than a lookup miss. Scoping
/// HERE keeps `resolve` and `ruleName` unchanged: they only ever see a scoped spec.
public func manifoldSpec(_ primary: JSON?, _ auth: JSON?, _ suffix: String) -> JSON? {
    // KEY PRESENCE decides, as python's `"ingress_rule" in record`: a present
    // `null` is a seal, never a fall-through to `auth`.
    if let primary { return primary }
    guard let auth, let (name, env) = try? parseSpec(auth), let stem = name else { return nil }
    if suffix == "_egress" && (stem == "allow_all" || stem == "deny") {
        return .string("silent_egress")
    }
    let scoped = stem + suffix
    if let env { return .object(["type": .string(scoped), "env": .string(env)]) }
    return .string(scoped)
}

/// The rule TYPE name for reflect — python `rule_name`: the spec's name, or `def`
/// when it has none (never the rule's config). The CALLER supplies `def`:
/// `deny_ingress` for ingress (IO legs seal by default) and `silent_egress` for
/// egress.
public func ruleName(_ spec: JSON?, default def: String) -> String {
    guard let name = (try? parseSpec(spec))?.name, !name.isEmpty else { return def }
    return name
}

/// A leg's auth posture for `reflect` — python `describe(record)`:
/// `{ingress_rule, egress_rule, sealed}`, rule NAMES only.
///
/// Keyed by the field a caller WRITES (read-key == write-key). The per-direction
/// field wins by key presence; `auth` carries a bare stem, reported as the scoped
/// rule it resolves to — python `_base._scoped` suffixes it whatever it already
/// ends in. Legs and bridges alike reflect with it. `manifoldSpec` stays as it
/// is: the gate uses it.
public func describe(_ meta: OrderedDictionary<String, JSON>) -> OrderedDictionary<String, JSON> {
    let auth = meta["auth"]
    let hasIngress = meta.keys.contains("ingress_rule")
    let hasEgress = meta.keys.contains("egress_rule")
    // A present stem is scoped the way the gate scopes it; an ABSENT spec is the
    // manifold's default, which already carries its suffix (python 2026-09-16).
    let ingName = ruleName(hasIngress ? meta["ingress_rule"] : auth, default: "")
    let egrName = ruleName(hasEgress ? meta["egress_rule"] : auth, default: "")
    let ingress = ingName.isEmpty ? "deny_ingress" : scopedName(ingName, "_ingress", wasScoped: hasIngress)
    let egress = egrName.isEmpty ? "silent_egress" : scopedName(egrName, "_egress", wasScoped: hasEgress)
    return [
        "ingress_rule": .string(ingress),
        "egress_rule": .string(egress),
        "sealed": .bool(ingress != "allow_all_ingress"),
    ]
}

/// Python `_scoped`: a per-direction field is taken as written; an `auth` stem
/// gets this manifold's suffix — the GATE's exact rule, so `reflect` never shows
/// a name that resolves differently when written back. An inbound-only stem
/// presents nothing outbound.
private func scopedName(_ name: String, _ suffix: String, wasScoped: Bool) -> String {
    if wasScoped { return name }
    if suffix == "_egress" && (name == "allow_all" || name == "deny") { return "silent_egress" }
    return name + suffix
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

/// Resolve the leg's INGRESS rule: `ingress_rule` if present, else the `auth`
/// both-directions shorthand. Absent ⇒ **DenyInbound — IO legs seal by default**
/// (`IngressRules.resolve` maps `nil` to it).
///
/// This doc used to read "Absent ⇒ AllowAll (back-compat)", which is the pre-seal-flip
/// behaviour and the opposite of what the code does. The code was right; the comment
/// was wrong in the dangerous direction — an operator reading it would believe a fresh
/// leg admits traffic.
public func resolveIngress(ingressRule: JSON?, auth: JSON?) throws -> IngressRule {
    try IngressRules.resolve(manifoldSpec(ingressRule, auth, "_ingress"))
}

/// Resolve the leg's EGRESS rule: `egress_rule` if present, else the `auth`
/// both-directions shorthand (so `auth:"password"` presents the group token).
/// Absent ⇒ Silent.
public func resolveEgress(egressRule: JSON?, auth: JSON?) throws -> EgressRule {
    try EgressRules.resolve(manifoldSpec(egressRule, auth, "_egress"))
}
