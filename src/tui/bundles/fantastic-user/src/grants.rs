//! THE TOKEN RULE on `user`, and what an agent's page grants let it see. Read
//! from records, never from the call.
//!
//! - `user` may carry `token_sha256` (hex): then a call must carry the token.
//! - An agent's page rights are `can_call` entries `<tree guid>|<id>` (or
//!   `<tree guid>|*`; `*|*` for every tree); one covers that page agent and
//!   everything under it, and seeing takes any verb.

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;


fn meta(kernel: &Kernel, id: &str) -> Option<Map<String, Value>> {
    kernel.agents.get(&AgentId::from(id)).map(|a| a.value().record().meta)
}

/// `sha256(token)`, lowercase hex.
#[must_use]
pub fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// The token rule on user agent `me`: no `token_sha256` on its record — every
/// caller passes; one set — the token must hash to it (constant time).
pub fn resolve(kernel: &Kernel, me: &AgentId, token: Option<&str>) -> Result<AgentId, String> {
    let m = meta(kernel, &me.0).ok_or_else(|| format!("no agent `{}` in this kernel", me.0))?;
    let stored = m.get("token_sha256").and_then(Value::as_str).filter(|h| !h.is_empty());
    match (stored, token.filter(|t| !t.is_empty())) {
        (None, _) => Ok(me.clone()),
        (Some(_), None) => Err("a token is required".into()),
        (Some(h), Some(t)) if same(h, &hash(t)) => Ok(me.clone()),
        (Some(_), Some(_)) => Err("unknown token".into()),
    }
}

/// The user's `can_call`.
#[must_use]
pub fn can_call(kernel: &Kernel, user: &AgentId) -> Map<String, Value> {
    meta(kernel, &user.0).and_then(|m| m.get("can_call").and_then(Value::as_object).cloned()).unwrap_or_default()
}

fn verbs(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Value::Object(o) => o.get("verbs").map(verbs).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The verbs a user holds on PAGE agent `id`, whose ancestors (root last) are
/// `up`: its own entry, any ancestor's, and `<guid>|*` — each also under the
/// any-tree guid `*` (`*|*` = every page agent of every tree).
#[must_use]
pub fn page_verbs(cc: &Map<String, Value>, guid: &str, id: &str, up: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for g in [guid, "*"] {
        for k in std::iter::once(id).chain(up.iter().map(String::as_str)).chain(["*"]) {
            if let Some(v) = cc.get(&format!("{g}|{k}")) {
                out.extend(verbs(v));
            }
        }
    }
    out
}

/// Whether these verbs let the caller see a page agent.
#[must_use]
pub fn sees(v: &[String]) -> bool {
    !v.is_empty()
}
