// `password` — kernel-GROUP membership by a shared secret (egress side: PRESENT).

import Foundation

extension EgressRules {
    /// Present this leg's group token (read from `tokenEnv`, default
    /// `FANTASTIC_GROUP_TOKEN`) on every outbound `call`, so a paired group member's
    /// ingress `password` rule accepts it. The symmetric mirror of
    /// `IngressRules.Password`. Presents nothing when the env var is unset/empty.
    public struct Password: EgressRule {
        public let tokenEnv: String
        public init(tokenEnv: String = DefaultGroupTokenEnv) { self.tokenEnv = tokenEnv }

        public func credential() -> String? {
            let v = ProcessInfo.processInfo.environment[tokenEnv]
            return (v?.isEmpty == false) ? v : nil
        }
    }
}
