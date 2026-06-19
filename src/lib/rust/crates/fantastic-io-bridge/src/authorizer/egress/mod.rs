//! egress_rules — the outbound-DECORATOR registry (the "upper importer").
//!
//! Each egress rule is its own module here; [`resolve`] registers them BY NAME.
//! Inbound-only policy names (`allow_all` / `deny_inbound`) map to `Silent` (present
//! nothing), so the legacy `auth` shorthand stays consistent (`auth:"deny_inbound"`
//! ⇒ presents nothing, `auth:"password"` ⇒ presents the group token).

use std::sync::Arc;

use serde_json::Value;

use super::{parse_spec, EgressRule};

mod password;
mod silent;

pub use password::Password;
pub use silent::Silent;

/// Resolve an egress rule spec (string | `{type, env}` | null) BY NAME. Absent ⇒
/// `Silent` (back-compat — present nothing). Unknown type ⇒ `Err`.
pub fn resolve(spec: Option<&Value>) -> Result<Arc<dyn EgressRule>, String> {
    let (name, token_env) = parse_spec(spec)?;
    let rule: Arc<dyn EgressRule> = match name.as_deref() {
        None | Some("silent") | Some("allow_all") | Some("deny_inbound") => Arc::new(Silent),
        Some("password") => Arc::new(Password::new(token_env)),
        Some(other) => return Err(format!("unknown egress rule type {other:?}")),
    };
    Ok(rule)
}
