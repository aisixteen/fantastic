//! `silent` — the default egress rule: present no credential (today's wire shape).

use super::super::EgressRule;

/// Attach nothing to outbound calls — the back-compat default and what every
/// non-credential-bearing policy (allow_all / deny_inbound) presents.
pub struct Silent;

impl EgressRule for Silent {
    fn credential(&self) -> Option<String> {
        None
    }
}
