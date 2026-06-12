//! `allow_all` — the default ingress rule: full symmetric duplex, a true no-op.

use super::super::{Action, Decision, IngressRule};

/// Permit every inbound action. The engine default (absent rule ⇒ this).
pub struct AllowAll;

impl IngressRule for AllowAll {
    fn authorize(&self, _action: &Action) -> Decision {
        Decision::Allow
    }
}
