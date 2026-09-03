// The `password` ingress rule gates EVERY kind — not just `call`.

import FantasticIoBridge
import Foundation
import Testing

@Suite("io_bridge password ingress rule", .serialized)
struct IngressPasswordTests {
    /// It used to short-circuit `.allow` for any kind but `call`, so a
    /// `password_ingress` exec leg admitted `start` (kind `spawn`) with no
    /// credential and a web leg leaked `watch` telemetry to a tokenless client (#7).
    @Test func passwordRuleGatesEveryKind() {
        let env = "FANTASTIC_GROUP_TOKEN_PW_UNIT_EVERY_KIND"
        setenv(env, "s3cret", 1)
        defer { unsetenv(env) }
        let rule = IngressRules.Password(tokenEnv: env)
        let act = { (kind: String, token: String?) in
            AuthAction(kind: kind, target: "x", verb: "start", token: token)
        }
        for kind in ["call", "spawn", "watch", "unwatch", "state_subscribe", "emit"] {
            guard case .deny = rule.authorize(act(kind, nil)) else {
                Issue.record("\(kind) must be denied without a token")
                continue
            }
            guard case .deny = rule.authorize(act(kind, "nope")) else {
                Issue.record("\(kind) must be denied with a wrong token")
                continue
            }
            guard case .allow = rule.authorize(act(kind, "s3cret")) else {
                Issue.record("\(kind) must be admitted with the group token")
                continue
            }
        }
        // Fail-closed: an unset env var refuses every kind, token or not.
        unsetenv(env)
        guard case .deny = rule.authorize(act("watch", "s3cret")) else {
            Issue.record("unset env must fail closed")
            return
        }
    }
}
