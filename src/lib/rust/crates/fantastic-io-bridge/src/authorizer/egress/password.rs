//! `password` — kernel-GROUP membership by a shared secret (egress side: PRESENT).

use super::super::{EgressRule, DEFAULT_TOKEN_ENV};

/// Present this leg's group token (read from `token_env`, default
/// `FANTASTIC_GROUP_TOKEN`) on every outbound `call`, so a paired group member's
/// ingress `password` rule accepts it. The symmetric mirror of `ingress::password`.
/// Presents nothing when the env var is unset/empty.
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
}

impl EgressRule for Password {
    fn credential(&self) -> Option<String> {
        std::env::var(&self.token_env)
            .ok()
            .filter(|s| !s.is_empty())
    }
}
