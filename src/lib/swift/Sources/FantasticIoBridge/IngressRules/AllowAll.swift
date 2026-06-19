// `allow_all` — the default ingress rule: full symmetric duplex, a true no-op.

extension IngressRules {
    /// Permit every inbound action. The engine default (absent rule ⇒ this).
    public struct AllowAll: IngressRule {
        public init() {}
        public func authorize(_ action: AuthAction) -> AuthDecision { .allow }
    }
}
