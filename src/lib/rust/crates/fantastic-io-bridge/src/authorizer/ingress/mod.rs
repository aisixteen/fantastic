//! The inbound-FILTER rules — one module each; [`resolve`] picks the one a leg's
//! [`Rules`] name. A leg with no rule set is SEALED. Add a rule = a module, a variant
//! of [`super::IngressName`] and one arm here; the read-loop choke point never changes
//! (it is rule-agnostic, calling `authorize` through `Arc<dyn IngressRule>`).

use std::sync::Arc;

use super::{IngressName, IngressRule, Rules};

mod allow_all;
mod deny_inbound;
mod password;

pub use allow_all::AllowAll;
pub use deny_inbound::DenyInbound;
pub use password::Password;

/// The ingress rule `rules` names.
pub fn resolve(rules: &Rules) -> Arc<dyn IngressRule> {
    match rules.ingress_rule {
        IngressName::Deny => Arc::new(DenyInbound),
        IngressName::AllowAll => Arc::new(AllowAll),
        IngressName::Password => Arc::new(Password::new(rules.token_env.clone())),
    }
}
