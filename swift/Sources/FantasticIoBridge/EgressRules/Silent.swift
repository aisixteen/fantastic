// `silent` — the default egress rule: present no credential (today's wire shape).

extension EgressRules {
    /// Attach nothing to outbound calls — the back-compat default and what every
    /// non-credential-bearing policy (allow_all / deny_inbound) presents.
    public struct Silent: EgressRule {
        public init() {}
        public func credential() -> String? { nil }
    }
}
