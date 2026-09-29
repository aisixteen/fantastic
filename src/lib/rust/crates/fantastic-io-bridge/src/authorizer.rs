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
//! `ingress_rule`/`egress_rule` (symmetric), or the `auth` shorthand (sets
//! both sides). Rules are TRANSITIONAL (inline plumbing), not invocational (agents).

use fantastic_bundle::py_truthy;
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
/// It used to be a bare `String`, so a rule could state a fact and nothing
/// else: every gate that received one invented its own `hint` at the call site
/// (the fs gate said "the fs edge is sealed" for whichever leg called it) and
/// the three runtimes drifted into three vocabularies for the same `unauthorized`
/// code. The rule KNOWS why it refused and how to open the edge; that knowledge
/// travels with the verdict instead of being guessed downstream.
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
    /// call site has to remember the key order or invent the teaching half — the
    /// fs gate used to bolt on a hint claiming "the fs edge" for whichever leg
    /// had called it.
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

/// Default env var for the `password` rule when the record names none.
pub(crate) const DEFAULT_TOKEN_ENV: &str = "FANTASTIC_GROUP_TOKEN";

/// Normalize a rule spec `Value` to `(type_name, secret_env_key)`. python
/// `io_bridge._base.parse_spec`: a FALSY spec (absent, null, `""`, `0`, `[]`,
/// `{}`) is `(None, None)`, a string is `(name, None)`, an object is
/// `(type, env)`.
///
/// `type` and `env` are the only spellings.
///
/// An object with no `type` is `(None, env)` — a plain SEAL, as python resolves
/// it. It used to be an `Err` that every caller turned into its own shape: the
/// file gate answered a bare `{error:"rule object missing 'type'"}` and refused
/// the write, and exec_bridge threw the same string out of `handle`.
pub(crate) fn parse_spec(spec: Option<&Value>) -> Result<(Option<String>, Option<String>), String> {
    let Some(spec) = spec else {
        return Ok((None, None));
    };
    if !py_truthy(spec) {
        return Ok((None, None));
    }
    match spec {
        Value::String(s) => Ok((Some(s.clone()), None)),
        Value::Object(o) => {
            let name = o.get("type").and_then(Value::as_str).map(str::to_string);
            let env = o.get("env").and_then(Value::as_str).map(str::to_string);
            Ok((name, env))
        }
        other => Err(format!("unsupported rule spec {other:?}")),
    }
}

/// The effective spec for ONE manifold, with the `auth` shorthand already SCOPED.
///
/// Rule names are manifold-scoped: a name binds to exactly one direction, so a name
/// written into the wrong field is a loud error instead of a silent policy change.
/// Measured 2026-08-21 (python, and this runtime had the identical defect): with bare
/// names, `auth:"silent"` — a legitimate EGRESS policy — resolved ingress to
/// `DenyInbound(unknown="silent")`, SEALING the leg while saying nothing; and
/// `auth:"allow_all"` handed egress a `Silent` nobody asked for. One unscoped name
/// against two registries with different membership is a fallback, and this kernel does
/// not have fallbacks.
///
/// The per-direction field WINS and is taken as written (the author already scoped it).
/// `auth` carries a bare STEM, and each manifold appends its own suffix — except that an
/// inbound-only stem presents nothing outbound, which is a MEANING spelled out per stem,
/// not a lookup miss. Doing the scoping HERE keeps `resolve` and `rule_name` unchanged:
/// they only ever see an already-scoped spec.
pub fn manifold_spec(primary: Option<&Value>, auth: Option<&Value>, suffix: &str) -> Option<Value> {
    if let Some(v) = primary {
        return Some(v.clone());
    }
    let spec = auth?;
    let (name, env) = parse_spec(Some(spec)).ok()?;
    let stem = name?;
    if suffix == "_egress" && (stem == "allow_all" || stem == "deny") {
        return Some(Value::String("silent_egress".to_string()));
    }
    let scoped = format!("{stem}{suffix}");
    match env {
        Some(e) => Some(json!({ "type": scoped, "env": e })),
        None => Some(Value::String(scoped)),
    }
}

/// The rule TYPE name for reflect — never surfaces the rule's config. Absent,
/// falsy or type-less ⇒ `default`, which the CALLER supplies: `deny_ingress` for
/// ingress (IO legs seal by default) and `silent_egress` for egress. python
/// `rule_name`: `parse_spec(spec)[0] or default`.
pub fn rule_name(spec: Option<&Value>, default: &str) -> String {
    match parse_spec(spec) {
        Ok((Some(name), _)) if !name.is_empty() => name,
        _ => default.to_string(),
    }
}

/// A leg's auth POSTURE for `reflect` — the rule NAMES (never their config) plus
/// whether the leg is sealed: `{ingress_rule, egress_rule, sealed}`. python
/// `io_bridge._base.describe`.
///
/// Keyed by the field you SET (read-key == write-key), so a readme-only LLM reads
/// `ingress_rule: deny_ingress` and opens the leg with
/// `ingress_rule: allow_all_ingress` — a direct mirror, no translation. The name
/// is reported SCOPED whichever field it came from, EXCEPT that a name which
/// already carries the suffix is left alone: `auth: allow_all_ingress` used to
/// reflect as `allow_all_ingress_ingress` here, a name that resolves to nothing
/// if written back.
#[must_use]
pub fn describe(meta: &serde_json::Map<String, Value>) -> Value {
    let ingress_scoped = meta.contains_key("ingress_rule");
    let egress_scoped = meta.contains_key("egress_rule");
    let ing_spec = if ingress_scoped {
        meta.get("ingress_rule")
    } else {
        meta.get("auth")
    };
    let egr_spec = if egress_scoped {
        meta.get("egress_rule")
    } else {
        meta.get("auth")
    };
    let ingress = scoped(
        &rule_name(ing_spec, "deny_ingress"),
        "_ingress",
        ingress_scoped,
    );
    let egress = scoped(
        &rule_name(egr_spec, "silent_egress"),
        "_egress",
        egress_scoped,
    );
    json!({
        "ingress_rule": ingress,
        "egress_rule": egress,
        "sealed": ingress != "allow_all_ingress",
    })
}

/// python `_scoped`: report the SCOPED rule name whichever field it came from —
/// `auth` carries a bare stem and each manifold appends its own suffix, while a
/// name that already ends in the suffix (or was written into the scoped field) is
/// reported as written.
fn scoped(name: &str, suffix: &str, was_scoped: bool) -> String {
    if was_scoped || name.ends_with(suffix) {
        return name.to_string();
    }
    if suffix == "_egress" && (name == "allow_all" || name == "deny") {
        return "silent_egress".to_string();
    }
    format!("{name}{suffix}")
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
