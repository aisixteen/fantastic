//! Call security — the substrate authorization primitive (mirror of the python
//! canonical `kernel/_authz.py`).
//!
//! The model is MINIMAL: **two-sided, deny-all,
//! explicit grants only.**
//!
//! - Every AGENT-originated send (`current_sender()` is `Some`) is checked TWICE:
//!   **egress** — the SENDER's `can_call` must grant `(target, verb)` — AND
//!   **ingress** — the TARGET's `callable_by` must grant `(sender, verb)`. An edge
//!   exists only if BOTH ends grant it. Kernel verbs answered by the root take the
//!   same check against the root. The gate never branches on a verb name.
//! - An OPERATOR send (`current_sender()` is `None`: a send made outside every
//!   dispatch — the CLI, the cold bootstrap, a test driver, a transport handing an
//!   inbound frame to its leg) and the kernel's own bookkeeping call are the origin
//!   root of trust and never reach this module — the gate skips them. Identity
//!   comes ONLY from dispatch: the sends a handler makes (and the tasks it spawns)
//!   are that agent's.
//!
//! Both fields are grant maps of one shape, `{key: "*" | ["<verb>", …]}`; `"*"`
//! covers EVERY verb. A key is one of:
//!
//! - a literal local agent id;
//! - `$self`, `$parent`, `$subtree` (every agent BELOW the holder, any depth) —
//!   resolved at the gate
//!   relative to the grant's HOLDER (the sender for `can_call`, the target for
//!   `callable_by`);
//! - `<guid>|<agent>` — one agent of a connected kernel, or `<guid>|*` — every
//!   agent of that kernel: `can_call` ONLY (an OUTBOUND peer address). A caller is
//!   always local — a call a bridge leg serves is judged as the LEG — so such a
//!   key in `callable_by` could never match and is refused at the write.
//!
//! The baseline stamped + MERGED on every agent (root, created, disk-loaded — in
//! `Agent::new`), on BOTH fields, is `{"$self": "*"}` and nothing else. Every
//! other edge is an explicit grant.

use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// The one authorization verdict.
#[derive(Debug, Clone)]
pub struct Decision {
    /// Whether the send is permitted.
    pub allowed: bool,
    /// On denial, a one-line statement of what was refused.
    pub reason: String,
    /// On denial, a teaching pointer: grants are explicit.
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

/// The relative tokens a grant key may be.
pub const TOKENS: [&str; 3] = ["$self", "$parent", "$subtree"];

/// The legal keys of `callable_by`, in the words its shape refusal uses.
pub const LEGAL_CALLABLE_BY_KEYS: &str = "an agent id, $self, $parent, $subtree";

/// The legal keys of `can_call`, in the words its shape refusal uses.
pub const LEGAL_CAN_CALL_KEYS: &str =
    "an agent id, $self, $parent, $subtree, <guid>|<agent> or <guid>|*";

/// The legal-key words for grant `field` (`can_call` or `callable_by`).
#[must_use]
pub fn legal_keys(field: &str) -> &'static str {
    if field == "can_call" {
        LEGAL_CAN_CALL_KEYS
    } else {
        LEGAL_CALLABLE_BY_KEYS
    }
}

/// Is `key` a legal key of grant `field`? Legal on both: a token, a literal local
/// agent id (non-empty, no `$` prefix, no `|`, not `*`). `can_call` also takes an
/// outbound peer address `<guid>|<agent>` / `<guid>|*` (exactly one `|`, both
/// halves non-empty); in `callable_by` such a key could never match a caller.
#[must_use]
pub fn is_grant_key(field: &str, key: &str) -> bool {
    if TOKENS.contains(&key) {
        return true;
    }
    if key.is_empty() || key.starts_with('$') || key == "*" {
        return false;
    }
    match key.split_once('|') {
        None => true,
        Some((guid, agent)) => {
            field == "can_call" && !guid.is_empty() && !agent.is_empty() && !agent.contains('|')
        }
    }
}

/// The remedy shown on any denial (python `_FROM_ABOVE`, verbatim).
pub const FROM_ABOVE: &str = "grants come from above — an ancestor (update_agent on an agent in \
     its subtree) or the operator writes can_call and callable_by; an agent never raises its \
     own";

/// The baseline stamped + merged on every agent, on `can_call` (who I may call):
/// myself, every verb.
pub fn default_can_call() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("$self".into(), json!("*"));
    m
}

/// The baseline for `callable_by` (who may call me) — the mirror of
/// [`default_can_call`].
pub fn default_callable_by() -> Map<String, Value> {
    default_can_call()
}

/// A grant value is `"*"` (every verb) or a list of verb names (which may itself
/// contain `"*"`).
fn verb_allowed(grant: &Value, verb: &str) -> bool {
    if grant.as_str() == Some("*") {
        return true;
    }
    grant.as_array().is_some_and(|a| {
        a.iter()
            .any(|v| v.as_str() == Some(verb) || v.as_str() == Some("*"))
    })
}

/// One matching engine, used BOTH ways. `grant` is the HOLDER's grant map
/// (`can_call` for egress, `callable_by` for ingress); `other_id` is the id at the
/// other end of the edge (the target for egress, the sender for ingress). Tokens
/// resolve relative to the holder. Absent/empty grant ⇒ deny (the default posture).
/// `other_ancestors` is every id above the other end — `$subtree` matches when the
/// holder is one of them.
pub fn authorize_call(
    grant: &Value,
    verb: &str,
    other_id: &str,
    holder_id: &str,
    holder_parent: Option<&str>,
    other_ancestors: &HashSet<String>,
) -> Decision {
    if let Some(map) = grant.as_object() {
        for (key, g) in map {
            let matched = match key.as_str() {
                "$self" => other_id == holder_id,
                "$parent" => holder_parent == Some(other_id),
                "$subtree" => other_ancestors.contains(holder_id),
                lit if lit.starts_with('$') => false,
                // every agent of ONE peer kernel: `<guid>|*` (a local id never
                // carries `|`, so this only ever matches an outbound peer target)
                star if star.ends_with("|*") => other_id.starts_with(&star[..star.len() - 1]),
                // a local id, or an outbound peer `<guid>|<agent>`
                literal => literal == other_id,
            };
            if matched && verb_allowed(g, verb) {
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
