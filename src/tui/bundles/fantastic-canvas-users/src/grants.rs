//! WHO A CALL IS, and WHAT THAT USER SEES — read from the user agent's own
//! record, never from the call.
//!
//! - A USER is an agent under the gateway (`canvas_user.tools`), or the
//!   tokenless default `user`. Its record holds `token_sha256` (optional, hex)
//!   and EVERY right it has, as `can_call`: host agents by id, page agents as
//!   `<tree guid>|<id>` (or `<tree guid>|*`; `*|*` for every tree).
//! - A page grant covers that page agent AND everything under it.
//! - Seeing a page agent takes any verb on it; WRITING its record takes a verb
//!   other than `reflect`.

use std::sync::Arc;

use fantastic_kernel::{AgentId, Kernel};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The tokenless default: the shell's own identity.
pub const DEFAULT_USER: &str = "user";
/// A user record's handler.
pub const USER_MODULE: &str = "canvas_user.tools";

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

/// The user a call acts as. No token → [`DEFAULT_USER`], unless it carries a
/// `token_sha256` (then a token is required). A token → the user (a child of
/// `gateway`, or the default) whose `token_sha256` matches it.
pub fn resolve(kernel: &Kernel, gateway: &AgentId, token: Option<&str>) -> Result<AgentId, String> {
    match token.filter(|t| !t.is_empty()) {
        None => {
            let m = meta(kernel, DEFAULT_USER).ok_or("no default user agent `user` in this kernel")?;
            if m.get("token_sha256").and_then(Value::as_str).is_some_and(|h| !h.is_empty()) {
                return Err("a token is required".into());
            }
            Ok(AgentId::from(DEFAULT_USER))
        }
        Some(t) => {
            let h = hash(t);
            let mut candidates: Vec<String> = kernel
                .agents
                .iter()
                .filter(|e| e.value().parent_id.as_ref() == Some(gateway))
                .map(|e| e.key().0.clone())
                .collect();
            candidates.push(DEFAULT_USER.to_string());
            // every candidate is compared, so the time does not tell which matched
            let mut found = None;
            for id in candidates {
                let ok = meta(kernel, &id)
                    .and_then(|m| m.get("token_sha256").and_then(Value::as_str).map(str::to_string))
                    .is_some_and(|stored| same(&stored, &h));
                if ok && found.is_none() {
                    found = Some(id);
                }
            }
            found.map(|id| AgentId::from(id.as_str())).ok_or_else(|| "unknown token".to_string())
        }
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

/// Whether these verbs let the user see / write a page record.
#[must_use]
pub fn sees(v: &[String]) -> bool {
    !v.is_empty()
}

/// See [`sees`].
#[must_use]
pub fn writes(v: &[String]) -> bool {
    v.iter().any(|x| x != "reflect")
}

/// Whether the user may see HOST agent `id` (the state stream, reflects): any
/// grant naming it.
#[must_use]
pub fn sees_host(cc: &Map<String, Value>, id: &str) -> bool {
    cc.get(id).is_some_and(|v| !verbs(v).is_empty())
}

/// Every user agent under `gateway`, and the default.
#[must_use]
pub fn users(kernel: &Arc<Kernel>, gateway: &AgentId) -> Vec<AgentId> {
    let mut v: Vec<AgentId> = kernel
        .agents
        .iter()
        .filter(|e| e.value().parent_id.as_ref() == Some(gateway))
        .map(|e| e.key().clone())
        .collect();
    v.push(AgentId::from(DEFAULT_USER));
    v
}
