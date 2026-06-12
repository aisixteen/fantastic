//! Bridge authorization — base types + the two rule registries (`ingress`/`egress`).
//!
//! Two independent, TYPED rules govern a leg, mirrored on the wire and enforced on
//! the RECEIVER:
//!   - an INGRESS rule (the inbound FILTER) — [`ingress::resolve`] → `authorize`,
//!     consulted at the read-loop choke point before an inbound `call` dispatches.
//!   - an EGRESS rule (the outbound DECORATOR) — [`egress::resolve`] → `credential`,
//!     consulted by `forward` to stamp this leg's token on the frame ENVELOPE (never
//!     the dispatched payload — the target never sees it).
//!
//! Each rule is typed in the record (`{"type": <name>, "env": <var>}`) and resolved
//! BY NAME from a registry — the [`ingress`] and [`egress`] modules, each a folder
//! of one-rule-per-file plus a `mod.rs` importer that registers names. Add a rule =
//! drop a file + one registry arm; the engine never changes. The record carries
//! `ingress_rule`/`egress_rule` (symmetric), or the legacy `auth` shorthand (sets
//! both sides). Rules are TRANSITIONAL (inline plumbing), not invocational (agents).

use serde_json::Value;

pub mod egress;
pub mod ingress;

/// One inbound request the peer is asking this leg to perform locally.
pub struct Action<'a> {
    /// `"call"` (gated) | `"watch"` | `"unwatch"`.
    pub kind: &'a str,
    /// The local agent id the peer addressed.
    pub target: &'a str,
    /// `payload["type"]` — the verb requested (e.g. `"reflect"`).
    pub verb: &'a str,
    /// The `auth_token` the peer attached to this call on the frame ENVELOPE, if any.
    pub token: Option<&'a str>,
}

/// An ingress rule's verdict on an [`Action`].
pub enum Decision {
    /// Permit the inbound action — dispatch `kernel.send`.
    Allow,
    /// Refuse it; the read loop replies `{error, reason:"unauthorized"}`.
    Deny(String),
}

/// The inbound FILTER — decides whether the peer may perform an inbound action.
/// Object-safe so the gate stays rule-agnostic (`Arc<dyn IngressRule>`).
pub trait IngressRule: Send + Sync {
    /// Authorize one inbound action.
    fn authorize(&self, action: &Action) -> Decision;
}

/// The outbound DECORATOR — the token this leg PRESENTS on its own outbound `call`s
/// (stamped on the frame envelope by `forward`). `None` ⇒ present nothing.
pub trait EgressRule: Send + Sync {
    /// The credential to present, or `None`.
    fn credential(&self) -> Option<String>;
}

/// Default env var for the `password` rule when the record names none.
pub(crate) const DEFAULT_TOKEN_ENV: &str = "FANTASTIC_GROUP_TOKEN";

/// Normalize a rule spec `Value` to `(type_name, token_env)`. Absent/null/empty ⇒
/// `(None, None)`. String ⇒ `(name, None)`. Object ⇒ `(type|policy, env|token_env)`.
pub(crate) fn parse_spec(spec: Option<&Value>) -> Result<(Option<String>, Option<String>), String> {
    let Some(spec) = spec else {
        return Ok((None, None));
    };
    match spec {
        Value::Null => Ok((None, None)),
        Value::String(s) if s.is_empty() => Ok((None, None)),
        Value::String(s) => Ok((Some(s.clone()), None)),
        Value::Object(o) => {
            let name = o
                .get("type")
                .or_else(|| o.get("policy"))
                .and_then(Value::as_str)
                .ok_or_else(|| "rule object missing 'type'".to_string())?;
            let env = o
                .get("env")
                .or_else(|| o.get("token_env"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok((Some(name.to_string()), env))
        }
        other => Err(format!("unsupported rule spec {other:?}")),
    }
}

/// The rule TYPE name for reflect — never surfaces the rule's config. Absent ⇒
/// `default` (`allow_all` for ingress, `silent` for egress).
pub fn rule_name(spec: Option<&Value>, default: &str) -> String {
    match spec {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Object(o)) => o
            .get("type")
            .or_else(|| o.get("policy"))
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_string(),
        _ => default.to_string(),
    }
}

/// Content-blind constant-time compare given equal length (length is not secret —
/// same posture as Python's `hmac.compare_digest`). Avoids a timing oracle without a
/// crypto dependency.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
