//! `password` — kernel-GROUP membership by a shared secret (ingress side: CHECK).

use super::super::{constant_time_eq, Action, Decision, IngressRule, DEFAULT_TOKEN_ENV};

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
    fn authorize(&self, action: &Action) -> Decision {
        if action.kind != "call" {
            return Decision::Allow; // watch/unwatch already ignored by the read loop
        }
        let Some(expected) = self.token() else {
            return Decision::Deny(format!("group token unset ({})", self.token_env));
        };
        match action.token {
            Some(p) if constant_time_eq(p.as_bytes(), expected.as_bytes()) => Decision::Allow,
            _ => Decision::Deny("invalid or missing group token".to_string()),
        }
    }
}
