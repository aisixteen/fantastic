// EgressRules — the outbound-DECORATOR registry (the "upper importer").
//
// Each egress rule is its own file (an `extension EgressRules`); `resolve` registers
// them BY NAME. Inbound-only policy names (`allow_all` / `deny_inbound`) map to
// `Silent` (present nothing), so the `auth` both-directions shorthand stays consistent.

import FantasticJSON

/// Namespace + registry for outbound-decorator rules.
public enum EgressRules {
    /// Resolve an egress rule spec (string | `{type, env}` | null) BY NAME. Absent ⇒
    /// `Silent` (present nothing). Unknown type ⇒ throws. Names are MANIFOLD-SCOPED;
    /// an inbound-only stem reaching here through `auth` was already collapsed to
    /// `silent_egress` by `manifoldSpec`, so there are no cross-manifold aliases.
    public static func resolve(_ spec: JSON?) throws -> EgressRule {
        let (name, env) = parseSpec(spec)
        switch name {
        case nil, .some("silent_egress"):
            return Silent()
        case .some("password_egress"):
            return Password(tokenEnv: env ?? DefaultGroupTokenEnv)
        case .some(let other):
            // python `f"unknown egress rule type {name!r}"`
            throw AuthPolicyError("unknown egress rule type \(pyRepr(other))")
        }
    }
}
