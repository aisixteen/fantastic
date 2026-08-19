//! `deny_inbound` — one-way / hub→spoke push: refuse every inbound `call`.

use super::super::{Action, Decision, IngressRule};

/// Refuse every inbound `call` (the peer can't `call`/`reflect` us). Inbound
/// `watch`/`unwatch` are already ignored by the read loop ⇒ denied-by-omission.
///
/// `unknown` carries a rule name that could not be resolved. An edge configured with
/// a rule nobody implements is NOT authorized — it seals, and every denial says which
/// name was wrong. Mirrors python's `DenyInbound(unknown=…)`.
#[derive(Default)]
pub struct DenyInbound {
    pub unknown: Option<String>,
}

impl DenyInbound {
    /// The plain seal — no rule was set at all.
    pub fn sealed() -> Self {
        Self { unknown: None }
    }

    /// Sealed BECAUSE the configured rule name is not implemented.
    pub fn unknown(name: &str) -> Self {
        Self {
            unknown: Some(name.to_string()),
        }
    }
}

impl IngressRule for DenyInbound {
    fn authorize(&self, action: &Action) -> Decision {
        // EVERY inbound kind — py-canonical. This used to deny only `call`,
        // which made the rust seal partial: `watch`/`state_subscribe` leaked
        // telemetry through a "sealed" leg, and a new kind (`spawn`, measured
        // 2026-08-07 porting exec_bridge) sailed straight through what its
        // caller believed was a closed edge. A seal that filters by kind is a
        // second truth about what "sealed" means.
        let _ = action;
        Decision::Deny(match &self.unknown {
            Some(n) => format!(
                "edge sealed — unknown ingress rule {n:?}; legal values: \
                 allow_all, deny_inbound, password"
            ),
            None => "inbound calls denied by policy".to_string(),
        })
    }
}
