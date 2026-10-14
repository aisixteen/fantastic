// `password` — kernel-GROUP membership by a shared secret (ingress side: CHECK).

import Foundation

extension IngressRules {
    /// Authorize an inbound `call` only if its envelope `auth_token` matches this
    /// leg's group token (read from `tokenEnv`, default `FANTASTIC_GROUP_TOKEN`).
    /// Fail-closed when the env var is unset/empty. Constant-time compare. The egress
    /// mirror (`EgressRules.Password`) PRESENTS the same token.
    public struct Password: IngressRule {
        public let tokenEnv: String
        public init(tokenEnv: String = DefaultGroupTokenEnv) { self.tokenEnv = tokenEnv }

        private func token() -> String? {
            let v = ProcessInfo.processInfo.environment[tokenEnv]
            return (v?.isEmpty == false) ? v : nil
        }

        /// Gate EVERY inbound kind the choke point asks about — `call` AND the
        /// data-bearing kinds (`spawn`, `watch`, `state_subscribe`, `emit`). Short-circuiting
        /// `.allow` for any kind but `call` would let a `password_ingress` exec leg
        /// admit `start` (kind `spawn`) with no credential and a web leg leak `watch`
        /// telemetry to a tokenless client. Python checks the
        /// token unconditionally; so does this.
        public func authorize(_ action: AuthAction) -> AuthDecision {
            guard let expected = token() else {
                return .deny(Denial("group token unset (\(tokenEnv))"))
            }
            if let presented = action.token, constantTimeEquals(presented, expected) {
                return .allow
            }
            return .deny(Denial("invalid or missing group token"))
        }
    }
}
