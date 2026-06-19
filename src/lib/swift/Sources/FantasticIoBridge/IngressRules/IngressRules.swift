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
    /// Mirrors py/rust's seal-default flip. Unknown type ⇒ throws.
    public static func resolve(_ spec: JSON?) throws -> IngressRule {
        let (name, env) = try parseSpec(spec)
        switch name {
        case nil, .some("deny_inbound"): return DenyInbound()
        case .some("allow_all"): return AllowAll()
        case .some("password"): return Password(tokenEnv: env ?? DefaultGroupTokenEnv)
        case .some(let other): throw AuthPolicyError("unknown ingress rule type \"\(other)\"")
        }
    }
}
