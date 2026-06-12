// EgressRules — the outbound-DECORATOR registry (the "upper importer").
//
// Each egress rule is its own file (an `extension EgressRules`); `resolve` registers
// them BY NAME. Inbound-only policy names (`allow_all` / `deny_inbound`) map to
// `Silent` (present nothing), so the legacy `auth` shorthand stays consistent.

import FantasticJSON

/// Namespace + registry for outbound-decorator rules.
public enum EgressRules {
    /// Resolve an egress rule spec (string | `{type, env}` | null) BY NAME. Absent ⇒
    /// `Silent` (back-compat — present nothing). Unknown type ⇒ throws.
    public static func resolve(_ spec: JSON?) throws -> EgressRule {
        let (name, env) = try parseSpec(spec)
        switch name {
        case nil, .some("silent"), .some("allow_all"), .some("deny_inbound"):
            return Silent()
        case .some("password"):
            return Password(tokenEnv: env ?? DefaultGroupTokenEnv)
        case .some(let other):
            throw AuthPolicyError("unknown egress rule type \"\(other)\"")
        }
    }
}
