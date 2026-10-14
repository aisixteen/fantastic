// The `password` ingress rule gates EVERY kind — not just `call`.

import FantasticIoBridge
import Foundation
import Testing

@Suite("io_bridge password ingress rule", .serialized)
struct IngressPasswordTests {
    /// Short-circuiting `.allow` for any kind but `call` would let a
    /// `password_ingress` exec leg admit `start` (kind `spawn`) with no
    /// credential and a web leg leak `watch` telemetry to a tokenless client.
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

    /// The compare is over UTF-8 BYTES, as in python. Any token is a plain answer, and a non-ASCII group token
    /// admits its own holders. (A lone surrogate, python's fourth case, is not a
    /// Swift `String`.)
    @Test func passwordRuleComparesUTF8BytesSoAnyTokenIsAPlainAnswer() {
        let env = "FANTASTIC_GROUP_TOKEN_PW_UNIT_UTF8"
        setenv(env, "s3cret", 1)
        defer { unsetenv(env) }
        let rule = IngressRules.Password(tokenEnv: env)
        func call(_ token: String) -> AuthDecision {
            rule.authorize(AuthAction(kind: "call", target: "t", verb: "reflect", token: token))
        }
        for odd in ["é", "s3cr€t", "été"] {
            guard case .deny(let d) = call(odd) else {
                Issue.record("\(odd) must be denied")
                continue
            }
            #expect(d.reason == "invalid or missing group token", "\(odd)")
        }
        setenv(env, "päss-wört", 1)
        guard case .allow = call("päss-wört") else {
            Issue.record("a non-ASCII group token must admit its holder")
            return
        }
        guard case .deny = call("pass-wort") else {
            Issue.record("a near miss must be denied")
            return
        }
    }
}
