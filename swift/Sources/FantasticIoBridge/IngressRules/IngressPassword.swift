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

        public func authorize(_ action: AuthAction) -> AuthDecision {
            guard action.kind == "call" else { return .allow }
            guard let expected = token() else {
                return .deny("group token unset (\(tokenEnv))")
            }
            if let presented = action.token, constantTimeEquals(presented, expected) {
                return .allow
            }
            return .deny("invalid or missing group token")
        }
    }
}
