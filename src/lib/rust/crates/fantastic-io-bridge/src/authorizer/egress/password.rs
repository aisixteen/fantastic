//! `password` — kernel-GROUP membership by a shared secret (egress side: PRESENT).

use super::super::EgressRule;

/// Present this leg's group token (read from its `token_env`) on every outbound `call`, so a paired group member's
/// ingress `password` rule accepts it. The symmetric mirror of `ingress::password`.
/// Presents nothing when the env var is unset/empty.
pub struct Password {
    token_env: String,
}

impl Password {
    /// Build from the leg's `token_env`.
    pub fn new(token_env: String) -> Self {
        Self { token_env }
    }
}

impl EgressRule for Password {
    fn credential(&self) -> Option<String> {
        std::env::var(&self.token_env)
            .ok()
            .filter(|s| !s.is_empty())
    }
}
