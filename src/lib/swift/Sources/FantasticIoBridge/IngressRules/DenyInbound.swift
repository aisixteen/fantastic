// `deny_inbound` — one-way / hub→spoke push: refuse every inbound `call`.

extension IngressRules {
    /// Refuse every inbound `call` (the peer can't `call`/`reflect` us). Inbound
    /// `watch`/`unwatch` are already ignored by the dispatcher ⇒ denied-by-omission.
    /// `unknown` carries a rule name that could not be resolved. An edge configured
    /// with a rule nobody implements is NOT authorized — it seals, and every denial
    /// says which name was wrong. Mirrors py `DenyInbound(unknown:)` / rust
    /// `DenyInbound::unknown`.
    public struct DenyInbound: IngressRule {
        public let unknown: String?
        public init(unknown: String? = nil) { self.unknown = unknown }
        public func authorize(_ action: AuthAction) -> AuthDecision {
            // EVERY inbound kind — py-canonical. Call-only sealing made the seal
            // partial (watch/state_subscribe leaked, and a new kind like `spawn`
            // sailed through); measured on rust 2026-08-07, fixed in all runtimes.
            _ = action
            if let name = unknown {
                return .deny(
                    "edge sealed — unknown ingress rule \"\(name)\"; it is not an "
                        + "INGRESS rule name — the legal values are allow_all_ingress, "
                        + "deny_ingress, password_ingress (names are manifold-scoped: an "
                        + "egress name here is a typo, not a policy)")
            }
            return .deny("inbound calls denied by policy")
        }
    }
}
