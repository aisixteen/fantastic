//! ingress_rules — the inbound-FILTER registry (the "upper importer").
//!
//! Each ingress rule is its own module here; [`resolve`] registers them BY NAME.
//! Add a rule = drop a module + one match arm; the read-loop choke point never
//! changes (it is rule-agnostic, calling `authorize` through `Arc<dyn IngressRule>`).

use std::sync::Arc;

use serde_json::Value;

use super::{parse_spec, IngressRule};

mod allow_all;
mod deny_inbound;
mod password;

pub use allow_all::AllowAll;
pub use deny_inbound::DenyInbound;
pub use password::Password;

/// Resolve an ingress rule spec (string | `{type, env}` | null) BY NAME. Absent ⇒
/// `DenyInbound` — **SEALED by default** (every io leg + the fs edge denies until
/// opened with `ingress_rule=allow_all`/`password`).
///
/// UNKNOWN TYPE ⇒ `DenyInbound::unknown(name)` — seal, and name the bad rule in every
/// denial. It used to be `Err`, on the theory of failing loudly; but resolution runs
/// PER CALL, and in python that `Err` surfaced as an unhandled exception out of the
/// handler which killed the caller's whole chain with no reply. An unresolvable rule
/// is not an authorization, so deny. Mirrors py.
pub fn resolve(spec: Option<&Value>) -> Result<Arc<dyn IngressRule>, String> {
    let (name, token_env) = parse_spec(spec)?;
    let rule: Arc<dyn IngressRule> = match name.as_deref() {
        None | Some("deny_inbound") => Arc::new(DenyInbound::sealed()),
        Some("allow_all") => Arc::new(AllowAll),
        Some("password") => Arc::new(Password::new(token_env)),
        Some(other) => Arc::new(DenyInbound::unknown(other)),
    };
    Ok(rule)
}
