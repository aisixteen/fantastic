// `deny_inbound` — one-way / hub→spoke push: refuse every inbound `call`.

extension IngressRules {
    /// Refuse every inbound `call` (the peer can't `call`/`reflect` us). Inbound
    /// `watch`/`unwatch` are already ignored by the dispatcher ⇒ denied-by-omission.
    public struct DenyInbound: IngressRule {
        public init() {}
        public func authorize(_ action: AuthAction) -> AuthDecision {
            action.kind == "call"
                ? .deny("inbound calls denied by policy")
                : .allow
        }
    }
}
