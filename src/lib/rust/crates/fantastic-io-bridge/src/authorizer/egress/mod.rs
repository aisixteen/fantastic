//! egress_rules — the outbound-DECORATOR registry (the "upper importer").
//!
//! Each egress rule is its own module here; [`resolve`] registers them BY NAME.
//! Names are MANIFOLD-SCOPED (`*_egress`). The cross-manifold aliases are GONE: an
//! inbound-only stem reaching this manifold through the `auth` shorthand is collapsed to
//! `silent_egress` by `authorizer::manifold_spec` BEFORE resolution, so the meaning is
//! spelled out per stem rather than reached when a lookup misses.

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
        None | Some("silent_egress") => Arc::new(Silent),
        Some("password_egress") => Arc::new(Password::new(token_env)),
        Some(other) => return Err(format!("unknown egress rule type {other:?}")),
    };
    Ok(rule)
}
