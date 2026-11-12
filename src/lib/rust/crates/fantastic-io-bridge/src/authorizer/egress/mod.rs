//! The outbound-DECORATOR rules — one module each; [`resolve`] picks the one a leg's
//! [`Rules`] name.

use std::sync::Arc;

use super::{EgressName, EgressRule, Rules};

mod password;
mod silent;

pub use password::Password;
pub use silent::Silent;

/// The egress rule `rules` names.
pub fn resolve(rules: &Rules) -> Arc<dyn EgressRule> {
    match rules.egress_rule {
        EgressName::Silent => Arc::new(Silent),
        EgressName::Password => Arc::new(Password::new(rules.token_env.clone())),
    }
}
