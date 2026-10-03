//! `password` — kernel-GROUP membership by a shared secret (ingress side: CHECK).

use super::super::{constant_time_eq, Action, Decision, Denial, IngressRule, DEFAULT_TOKEN_ENV};

/// Authorize an inbound `call` only if its envelope `auth_token` matches this leg's
/// group token, read from an env var (`token_env`, default `FANTASTIC_GROUP_TOKEN`)
/// so the secret never touches the portable `.fantastic` workdir. Fail-closed: an
/// unset/empty env var refuses every inbound `call`. Constant-time compare. The
/// egress mirror (`egress::password`) PRESENTS the same token.
pub struct Password {
    token_env: String,
}

impl Password {
    /// Build from an optional `token_env` (default `FANTASTIC_GROUP_TOKEN`).
    pub fn new(token_env: Option<String>) -> Self {
        Self {
            token_env: token_env.unwrap_or_else(|| DEFAULT_TOKEN_ENV.to_string()),
        }
    }

    fn token(&self) -> Option<String> {
        std::env::var(&self.token_env)
            .ok()
            .filter(|s| !s.is_empty())
    }
}

impl IngressRule for Password {
    /// Gate EVERY inbound kind the choke point asks about — `call` AND the
    /// data-bearing kinds (`spawn`, `watch`, `state_subscribe`, `emit`). It used
    /// to short-circuit `Allow` for any kind but `call`, so a `password_ingress`
    /// exec leg admitted `start` (kind `spawn`) with no credential and a web leg
    /// leaked `watch` telemetry to a tokenless client (#7). Python checks the
    /// token unconditionally; so does this.
    fn authorize(&self, action: &Action) -> Decision {
        let Some(expected) = self.token() else {
            return Decision::Deny(Denial::new(format!(
                "group token unset ({})",
                self.token_env
            )));
        };
        match action.token {
            Some(p) if constant_time_eq(p.as_bytes(), expected.as_bytes()) => Decision::Allow,
            _ => Decision::Deny(Denial::new("invalid or missing group token")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act<'a>(kind: &'a str, token: Option<&'a str>) -> Action<'a> {
        Action {
            kind,
            target: "x",
            verb: "start",
            token,
        }
    }

    #[test]
    fn password_rule_gates_every_kind() {
        // A private env var so parallel tests cannot race on the default one.
        let env = "FANTASTIC_GROUP_TOKEN_PW_UNIT_EVERY_KIND";
        std::env::set_var(env, "s3cret");
        let rule = Password::new(Some(env.to_string()));
        for kind in [
            "call",
            "spawn",
            "watch",
            "unwatch",
            "state_subscribe",
            "emit",
        ] {
            assert!(
                matches!(rule.authorize(&act(kind, None)), Decision::Deny(_)),
                "{kind} must be denied without a token"
            );
            assert!(
                matches!(rule.authorize(&act(kind, Some("nope"))), Decision::Deny(_)),
                "{kind} must be denied with a wrong token"
            );
            assert!(
                matches!(rule.authorize(&act(kind, Some("s3cret"))), Decision::Allow),
                "{kind} must be admitted with the group token"
            );
        }
        std::env::remove_var(env);
        // Fail-closed: an unset env var refuses every kind, token or not.
        assert!(matches!(
            rule.authorize(&act("watch", Some("s3cret"))),
            Decision::Deny(_)
        ));
    }

    /// IO-6 (audit 2026-10-03): the compare is over UTF-8 BYTES — any presented
    /// token is a plain allow/deny, and a non-ASCII group token admits its own
    /// holders. Python raised TypeError on `str` here (`hmac.compare_digest`);
    /// this runtime always compared bytes — the python twin is
    /// `test_ingress_password_compares_utf8_bytes_so_any_token_is_a_plain_answer`.
    #[test]
    fn password_compares_utf8_bytes_so_any_token_is_a_plain_answer() {
        let env = "FANTASTIC_GROUP_TOKEN_PW_UNIT_UTF8";
        std::env::set_var(env, "s3cret");
        let rule = Password::new(Some(env.to_string()));
        for odd in ["é", "s3cr€t", "été"] {
            match rule.authorize(&act("call", Some(odd))) {
                Decision::Deny(d) => {
                    assert_eq!(d.reason, "invalid or missing group token", "{odd}")
                }
                Decision::Allow => panic!("{odd} admitted"),
            }
        }
        std::env::set_var(env, "päss-wört");
        assert!(matches!(
            rule.authorize(&act("call", Some("päss-wört"))),
            Decision::Allow
        ));
        assert!(matches!(
            rule.authorize(&act("call", Some("pass-wort"))),
            Decision::Deny(_)
        ));
        std::env::remove_var(env);
    }
}
