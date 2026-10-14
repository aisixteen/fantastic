//! ingress_rules — the inbound-FILTER registry (the "upper importer").
//!
//! Each ingress rule is its own module here; [`resolve`] registers them BY NAME.
//! Names are MANIFOLD-SCOPED (`*_ingress`) — see `authorizer::manifold_spec` for why an
//! egress name written here must be a loud error, not a silent seal.
//! Add a rule = drop a module + one match arm; the read-loop choke point never
//! changes (it is rule-agnostic, calling `authorize` through `Arc<dyn IngressRule>`).

use std::sync::Arc;

use fantastic_bundle::py_repr_value;
use serde_json::{Map, Value};

use super::{parse_spec, IngressRule};

mod allow_all;
mod deny_inbound;
mod password;

pub use allow_all::AllowAll;
pub use deny_inbound::DenyInbound;
pub use password::Password;

/// Resolve an ingress rule spec (string | `{type, env}` | null) BY NAME. Absent ⇒
/// `DenyInbound` — **SEALED by default** (every io leg + the fs edge denies until
/// opened with `ingress_rule=allow_all_ingress`/`password_ingress`).
///
/// UNKNOWN TYPE ⇒ `DenyInbound::unknown(name)` — seal, and name the bad rule in every
/// denial. Not `Err`: resolution runs PER CALL, and in python an error there
/// surfaces as an unhandled exception out of the handler, killing the caller's
/// whole chain with no reply. An unresolvable rule is not an authorization, so
/// deny. A MALFORMED spec is named by its repr (`parse_spec`) and seals the same
/// way, so resolution cannot fail. Mirrors py.
pub fn resolve(spec: Option<&Value>) -> Arc<dyn IngressRule> {
    let (name, token_env) = parse_spec(spec);
    match name.as_deref() {
        None | Some("deny_ingress") => Arc::new(DenyInbound::sealed()),
        Some("allow_all_ingress") => Arc::new(AllowAll),
        Some("password_ingress") => Arc::new(Password::new(token_env)),
        Some(other) => Arc::new(DenyInbound::unknown(other)),
    }
}

/// The rule name in `spec` that this registry does not implement, if any —
/// python's `getattr(resolve_ingress(...), "unknown", None)`, truthiness
/// included (an empty name is a plain seal, not an unknown one).
#[must_use]
pub fn unknown(spec: Option<&Value>) -> Option<String> {
    let name = parse_spec(spec).0?;
    (!name.is_empty() && !REGISTRY.contains(&name.as_str())).then_some(name)
}

/// THE write-time check every leg's `validate_meta` makes — python
/// `ingress_rules.unknown_ingress_refusal`: an `ingress_rule` this registry does
/// not implement (an unknown name, or a malformed spec) is refused at the WRITE,
/// next to the typo. The gate would seal on it anyway, but `{"updated": true}`
/// for a control that does not exist is its own failure. `legal` is the leg's own
/// list — an in-process edge (fs/process) has no wire envelope to carry a token,
/// so it names no `password_ingress`; a wire leg passes [`REGISTRY`]. Only a
/// PRESENT `ingress_rule` is judged: the `auth` stem's ingress half may
/// legitimately name no rule here.
#[must_use]
pub fn unknown_refusal(meta: &Map<String, Value>, legal: &[&str]) -> Option<String> {
    let spec = meta.get("ingress_rule")?;
    unknown(Some(spec))?;
    Some(format!(
        "ingress_rule={} is not a rule this bundle implements — legal values: {}",
        py_repr_value(spec),
        legal.join(", ")
    ))
}

/// name -> ingress rule, in python's `REGISTRY` order. New ingress rule types
/// register HERE and in `resolve`.
pub const REGISTRY: [&str; 3] = ["allow_all_ingress", "deny_ingress", "password_ingress"];
