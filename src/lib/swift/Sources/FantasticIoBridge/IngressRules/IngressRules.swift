// IngressRules — the inbound-FILTER registry (the "upper importer").
//
// Each ingress rule is its own file in this folder (an `extension IngressRules`);
// `resolve` registers them BY NAME. Add a rule = drop a file + one switch arm; the
// relay_connector dispatcher never changes (it is rule-agnostic).

import FantasticJSON

/// Namespace + registry for inbound-filter rules (Swift has a flat module namespace,
/// so the rules are nested here rather than colliding with the egress `Password`).
public enum IngressRules {
    /// Resolve an ingress rule spec (string | `{type, env}` | null) BY NAME.
    /// **SEALED BY DEFAULT** — absent ⇒ `DenyInbound` (an io leg with no rule
    /// refuses every inbound call until opened with `ingress_rule=allow_all`).
    /// Mirrors py/rust's seal-default flip.
    ///
    /// UNKNOWN TYPE ⇒ `DenyInbound(unknown:)` — seal, and name the bad rule in every
    /// denial. It used to throw; but resolution runs PER CALL, not at boot, and in
    /// python that raise surfaced as an unhandled exception out of the handler which
    /// killed the caller's whole chain with no reply. An unresolvable rule is not an
    /// authorization, so deny.
    public static func resolve(_ spec: JSON?) throws -> IngressRule {
        let (name, env) = try parseSpec(spec)
        switch name {
        case nil, .some("deny_ingress"): return DenyInbound()
        case .some("allow_all_ingress"): return AllowAll()
        case .some("password_ingress"): return Password(tokenEnv: env ?? DefaultGroupTokenEnv)
        case .some(let other): return DenyInbound(unknown: other)
        }
    }
}
