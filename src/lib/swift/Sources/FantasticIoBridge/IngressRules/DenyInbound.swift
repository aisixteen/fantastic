// `deny_inbound` — the SEAL: refuse every inbound action, and teach how to open.
//
// A sealed edge denies ALL inbound kinds (`call` / `watch` / `state_subscribe` /
// `emit` / `open`), not only `call` — a sealed leg leaks neither dispatch nor
// telemetry. Every denial carries a teaching pointer (`hint`) so an automated
// builder hitting the locked door learns to open it (set an `ingress_rule`, read
// the io_bridge readme) instead of dead-ending. Discovery-through-denial is the
// design.

extension IngressRules {
    /// Refuse every inbound `call` (the peer can't `call`/`reflect` us). Inbound
    /// `watch`/`unwatch` are already ignored by the dispatcher ⇒ denied-by-omission.
    /// `unknown` carries a rule name that could not be resolved. An edge configured
    /// with a rule nobody implements is NOT authorized — it seals, and every denial
    /// says which name was wrong. Mirrors py `DenyInbound(unknown:)` / rust
    /// `DenyInbound::unknown`.
    public struct DenyInbound: IngressRule {
        /// The repair, WORD FOR WORD as python's `DenyInbound` states it
        /// (`io_bridge/ingress_rules/deny_inbound.py`). The same locked door has
        /// to say the same thing in every runtime: a model taught the python hint
        /// got nothing it could act on from the ports, which said `inbound calls
        /// denied by policy` and carried no hint at all — while the fs gate
        /// invented one naming "the fs edge" for whichever leg had called it.
        public static let openIt =
            "this edge is sealed — open it: update_agent <id> "
            + "ingress_rule=allow_all_ingress (or password); reflect readme=true on "
            + "this agent for the channel model"

        public let unknown: String?
        public init(unknown: String? = nil) { self.unknown = unknown }
        public func authorize(_ action: AuthAction) -> AuthDecision {
            // EVERY inbound kind — py-canonical. Call-only sealing made the seal
            // partial (watch/state_subscribe leaked, and a new kind like `spawn`
            // sailed through); measured on rust 2026-08-07, fixed in all runtimes.
            _ = action
            // PYTHON'S WORDS, both variants. The reason says WHAT happened, the
            // hint says how to open it, and the unknown-rule variant appends the
            // legal names — so a caller that followed a wrong name is handed the
            // right ones instead of the same wall twice.
            if let name = unknown {
                return .deny(
                    Denial(
                        "edge sealed — unknown ingress rule '\(name)'",
                        hint: Self.openIt + ". '\(name)' is not an INGRESS rule name — "
                            + "the legal values are allow_all_ingress, deny_ingress, "
                            + "password_ingress (names are manifold-scoped: an egress "
                            + "name here is a typo, not a policy)"))
            }
            return .deny(Denial("edge sealed by policy", hint: Self.openIt))
        }
    }
}
