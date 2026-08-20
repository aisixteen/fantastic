//! Call security — the substrate authorization primitive (mirror of the python
//! canonical `kernel/_authz.py`).
//!
//! The model is **fully-gated, two-sided, from-above-only**:
//!
//! - Every AGENT-originated send (`current_sender()` is `Some`) is checked TWICE:
//!   **egress** — the SENDER's `can_call` must allow `(target, verb)` — AND
//!   **ingress** — the TARGET's `callable_by` must allow `(sender, verb)`. An edge
//!   exists only if BOTH ends grant it. The gate NEVER branches on a verb name:
//!   nothing is exempt, not even `reflect`.
//! - An OPERATOR send (`current_sender()` is `None`: the CLI, cold bootstrap, a
//!   loader hydrating at boot, a test driver, or a verb completing its own side
//!   effect under `with_operator`) is the origin root of trust and never reaches
//!   this module — the gate skips it.
//!
//! Both `can_call` and `callable_by` are grant maps of the same shape,
//! `{ id | token : ["<verb>", …] | "*" }`. Relative tokens resolve at the gate,
//! relative to the grant's HOLDER (the sender for `can_call`, the target for
//! `callable_by`): `$self`, `$parent`, `$children` (set membership), `$loader`
//! (the persistence root; rarely named, NOT in the baseline).
//!
//! The structural baseline stamped + MERGED on every agent (root, created,
//! disk-loaded — in `Agent::new`), on BOTH fields, is
//! `{$self:*, $parent:*, $children:*}` — reach/accept your own subtree and your
//! creator. No `"*"` wildcard token.
//!
//! **Authority flows from ABOVE only.** Grants are set by the creator/operator at
//! `create_agent`, or a parent delegates a subset of its own `can_call`; an agent
//! can NEVER raise its own authority (enforced at the gate, not here).
//!
//! PRIVILEGED verbs (`create_agent`/`delete_agent`/`update_agent`) are never
//! covered by a wildcard — they must be named explicitly — EXCEPT when the other
//! end is the holder itself (forking your OWN subtree is baseline). This is an
//! EGRESS rule (`enforce_privileged`): on the ingress side a `$children:*` callee
//! legitimately accepts a privileged verb, since egress already guards escalation.

use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// The one authorization verdict.
#[derive(Debug, Clone)]
pub struct Decision {
    /// Whether the send is permitted.
    pub allowed: bool,
    /// On denial, a one-line statement of what was refused.
    pub reason: String,
    /// On denial, a teaching pointer: authority descends, never self-raised.
    pub hint: String,
}

impl Decision {
    fn allow() -> Self {
        Decision {
            allowed: true,
            reason: String::new(),
            hint: String::new(),
        }
    }
}

/// Structural graph mutation — privileged (a wildcard never satisfies these).
pub const PRIVILEGED_VERBS: [&str; 3] = ["create_agent", "delete_agent", "update_agent"];

/// The remedy shown on any denial: authority descends, it is never self-raised.
pub const FROM_ABOVE: &str = "authority is granted from ABOVE — set at create_agent by your \
     creator, or by the operator; a parent may delegate a subset of its own reach. You cannot \
     grant it to yourself. Agents are deny-all by default (both to call and to be called).";

/// The structural baseline stamped + merged on every agent, on `can_call` (who I
/// may call). Reach your own subtree (self + children you spawn) and your creator.
pub fn default_can_call() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("$self".into(), json!("*"));
    m.insert("$parent".into(), json!("*"));
    m.insert("$children".into(), json!("*"));
    m
}

/// The structural baseline for `callable_by` (who may call me) — the mirror of
/// [`default_can_call`]: accept your own subtree and your creator.
pub fn default_callable_by() -> Map<String, Value> {
    default_can_call()
}

/// A grant value is `"*"` (any verb) or a list of verb names (which may itself
/// contain `"*"`). A PRIVILEGED verb loses its wildcard cover — only an explicit
/// list entry names it.
fn verb_allowed(grant: &Value, verb: &str, privileged: bool) -> bool {
    if privileged {
        return grant
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(verb)));
    }
    if grant.as_str() == Some("*") {
        return true;
    }
    if let Some(a) = grant.as_array() {
        return a
            .iter()
            .any(|v| v.as_str() == Some(verb) || v.as_str() == Some("*"));
    }
    false
}

/// One matching engine, used BOTH ways. `grant` is the HOLDER's grant map
/// (`can_call` for egress, `callable_by` for ingress); `other_id` is the id at the
/// other end of the edge (the target for egress, the sender for ingress). Tokens
/// resolve relative to the holder. Absent/empty grant ⇒ deny (the default posture).
///
/// `enforce_privileged` gates the wildcard-exclusion for PRIVILEGED verbs — an
/// EGRESS concern only (the rule constrains who may REACH a mutation). The gate
/// passes `false` on the ingress check.
#[allow(clippy::too_many_arguments)]
pub fn authorize_call(
    grant: &Value,
    verb: &str,
    other_id: &str,
    holder_id: &str,
    holder_parent: Option<&str>,
    holder_children: &HashSet<String>,
    loader_id: Option<&str>,
    enforce_privileged: bool,
) -> Decision {
    // Privileged verbs lose wildcard cover UNLESS the other end is the holder
    // itself (acting on your own subtree is baseline, not escalation) — egress only.
    let privileged =
        enforce_privileged && PRIVILEGED_VERBS.contains(&verb) && other_id != holder_id;
    if let Some(map) = grant.as_object() {
        for (key, g) in map {
            let matched = match key.as_str() {
                "$self" => other_id == holder_id,
                "$parent" => holder_parent == Some(other_id),
                "$children" => holder_children.contains(other_id),
                "$loader" => loader_id == Some(other_id),
                lit if lit.starts_with('$') => false,
                literal => literal == other_id,
            };
            if matched && verb_allowed(g, verb, privileged) {
                return Decision::allow();
            }
        }
    }
    Decision {
        allowed: false,
        reason: String::new(),
        hint: FROM_ABOVE.to_string(),
    }
}

#[cfg(test)]
mod tests;
