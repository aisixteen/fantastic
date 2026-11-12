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
//! The record carries ONE declared shape, [`Rules`]: `ingress_rule` and `egress_rule`
//! (a name from the closed [`IngressName`] / [`EgressName`] lists) and `token_env`.
//! A name no list spells is refused by the record's schema. The [`ingress`] and
//! [`egress`] modules each hold one rule per file and resolve a [`Rules`] to the rule.
//! Rules are TRANSITIONAL (inline plumbing), not invocational (agents).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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

/// Everything a refusal carries — the denial half of python's `Decision`,
/// field for field.
///
/// Not a bare `String`: a rule states the fact AND the repair, so no gate
/// invents its own `hint` at the call site and the runtimes share one vocabulary
/// for `unauthorized`. The rule KNOWS why it refused and how to open the edge;
/// that knowledge travels with the verdict instead of being guessed downstream.
#[derive(Debug, Clone, Default)]
pub struct Denial {
    /// One line a person reads — the `error` field on the wire.
    pub reason: String,
    /// The repair: what to do instead. Empty ⇒ omit the field.
    pub hint: String,
    /// A doc pointer (readme key). Empty ⇒ omit the field.
    pub see: String,
}

impl Denial {
    /// A denial that states only the reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            hint: String::new(),
            see: String::new(),
        }
    }

    /// …plus the repair. A refusal that cannot be acted on costs the same as no
    /// reply.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }

    /// …plus a doc pointer.
    #[must_use]
    pub fn with_see(mut self, see: impl Into<String>) -> Self {
        self.see = see.into();
        self
    }

    /// The wire shape every inbound gate answers a denial with. Built HERE so no
    /// call site has to remember the key order or invent the teaching half.
    #[must_use]
    pub fn to_refusal(&self) -> Value {
        let mut out = json!({ "error": self.reason, "reason": "unauthorized" });
        let obj = out.as_object_mut().expect("json! object");
        // Empty ⇒ ABSENT, not `""` — the same rule the kernel's `refusal`
        // follows, so a caller tests for the key, not for an empty string.
        if !self.hint.is_empty() {
            obj.insert("hint".to_string(), Value::String(self.hint.clone()));
        }
        if !self.see.is_empty() {
            obj.insert("see".to_string(), Value::String(self.see.clone()));
        }
        out
    }
}

/// An ingress rule's verdict on an [`Action`].
pub enum Decision {
    /// Permit the inbound action — dispatch `kernel.send`.
    Allow,
    /// Refuse it; the gate replies `{error: reason, reason:"unauthorized",
    /// hint?, see?}` — forwarding the [`Denial`]'s fields, never inventing its
    /// own.
    Deny(Denial),
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

/// Default env var for the `password` rules: the declared default of a leg's `token_env`.
pub const DEFAULT_TOKEN_ENV: &str = "FANTASTIC_GROUP_TOKEN";

/// The declared default of `token_env`, as a record field's default.
#[must_use]
pub fn default_token_env() -> String {
    DEFAULT_TOKEN_ENV.to_string()
}

/// The inbound FILTER a leg's record names (`ingress_rule`). A name no variant spells
/// is refused by the record's schema; the leg is SEALED until a rule opens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub enum IngressName {
    /// Permit every inbound action.
    #[serde(rename = "allow_all_ingress")]
    AllowAll,
    /// Refuse every inbound action (the default).
    #[default]
    #[serde(rename = "deny_ingress")]
    Deny,
    /// Permit an inbound action carrying this leg's group token.
    #[serde(rename = "password_ingress")]
    Password,
}

impl IngressName {
    /// The name as a record spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowAll => "allow_all_ingress",
            Self::Deny => "deny_ingress",
            Self::Password => "password_ingress",
        }
    }
}

/// The inbound filters an edge with no wire envelope takes (the file and process
/// edges): the password rule reads a token off an envelope, which such an edge never
/// has, so it is not a name there. The names spell as [`IngressName`]'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum EnvelopelessIngress {
    /// Permit every inbound action.
    #[serde(rename = "allow_all_ingress")]
    AllowAll,
    /// Refuse every inbound action.
    #[serde(rename = "deny_ingress")]
    Deny,
}

/// The outbound DECORATOR a leg's record names (`egress_rule`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub enum EgressName {
    /// Present no credential (the default).
    #[default]
    #[serde(rename = "silent_egress")]
    Silent,
    /// Present this leg's group token on every outbound call.
    #[serde(rename = "password_egress")]
    Password,
}

impl EgressName {
    /// The name as a record spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Silent => "silent_egress",
            Self::Password => "password_egress",
        }
    }
}

/// The rule fields of a leg's record, declared once for every leg: the ingress rule,
/// the egress rule and the env var the `password` rules read the group token from.
/// One shape: a record that does not carry them takes the declared defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Rules {
    /// The inbound filter. Sealed (`deny_ingress`) until set.
    #[serde(default)]
    pub ingress_rule: IngressName,
    /// The outbound decorator.
    #[serde(default)]
    pub egress_rule: EgressName,
    /// The env var the group token is read from (`password_ingress` / `password_egress`).
    #[serde(default = "default_token_env")]
    pub token_env: String,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            ingress_rule: IngressName::default(),
            egress_rule: EgressName::default(),
            token_env: default_token_env(),
        }
    }
}

impl Rules {
    /// Read the rule fields off a leg's record. `Err` names what the record holds that
    /// no rule is (a record edited past its schema); the caller treats the leg as sealed.
    ///
    /// # Errors
    /// The decode error text.
    pub fn of(record: &serde_json::Map<String, Value>) -> Result<Rules, String> {
        let mut own = serde_json::Map::new();
        for k in ["ingress_rule", "egress_rule", "token_env"] {
            if let Some(v) = record.get(k) {
                own.insert(k.to_string(), v.clone());
            }
        }
        serde_json::from_value(Value::Object(own)).map_err(|e| e.to_string())
    }

    /// A leg's auth POSTURE for `reflect` — the rule NAMES (never the env var) plus
    /// whether the leg is sealed: `{ingress_rule, egress_rule, sealed}`.
    #[must_use]
    pub fn describe(&self) -> Value {
        json!({
            "ingress_rule": self.ingress_rule.as_str(),
            "egress_rule": self.egress_rule.as_str(),
            "sealed": self.ingress_rule != IngressName::AllowAll,
        })
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

#[cfg(test)]
mod rules_tests {
    use super::*;

    fn rules(meta: Value) -> Result<Rules, String> {
        Rules::of(meta.as_object().expect("a record"))
    }

    /// An absent field takes its declared default; the posture reads the names.
    #[test]
    fn a_record_without_rules_is_sealed_and_silent() {
        assert_eq!(
            rules(json!({})).unwrap().describe(),
            json!({"ingress_rule": "deny_ingress", "egress_rule": "silent_egress", "sealed": true})
        );
        assert_eq!(rules(json!({})).unwrap().token_env, DEFAULT_TOKEN_ENV);
        assert_eq!(
            rules(json!({"ingress_rule": "allow_all_ingress"})).unwrap().describe(),
            json!({"ingress_rule": "allow_all_ingress", "egress_rule": "silent_egress", "sealed": false})
        );
        assert_eq!(
            rules(json!({"ingress_rule": "password_ingress", "egress_rule": "password_egress", "token_env": "MY_TOKEN"}))
                .unwrap()
                .describe(),
            json!({"ingress_rule": "password_ingress", "egress_rule": "password_egress", "sealed": true})
        );
    }

    /// One shape: a name no rule spells, a stem, an object or a wrong type is refused,
    /// never read as a seal or as another rule.
    #[test]
    fn only_a_declared_name_is_a_rule() {
        for bad in [
            json!({"ingress_rule": "allow_all"}),
            json!({"ingress_rule": "silent_egress"}),
            json!({"ingress_rule": {"type": "allow_all_ingress"}}),
            json!({"ingress_rule": 7}),
            json!({"egress_rule": "deny"}),
            json!({"egress_rule": ["password_egress"]}),
            json!({"token_env": 7}),
        ] {
            assert!(rules(bad.clone()).is_err(), "{bad}");
        }
    }
}
