//! Call security — the substrate authorization primitive (mirror of the python
//! canonical `kernel/_authz.py`).
//!
//! Deny-all is the kernel law for an AGENT-originated send (`current_sender` is
//! `Some`): it is permitted only by an explicit grant on the SENDER's record,
//! `can_call: { target | token : ["<verb>", …] | "*" }`. An OPERATOR send
//! (`current_sender` is `None`: the CLI, cold bootstrap, a loader hydrating at
//! boot, a test driver calling `kernel.send` directly) is the root of trust and
//! never reaches this module — the gate skips it. `reflect` is never gated.
//!
//! Relative tokens resolve at the gate:
//!   `$self`     → the sender's own id
//!   `$parent`   → the sender's parent id
//!   `$children` → the sender's direct children (set membership)
//!   `$loader`   → the persistence root
//!
//! The Unix baseline the kernel stamps + MERGES on every `create_agent` is
//! `{$self:*, $parent:*, $children:*}` — command your own subtree and answer your
//! creator. NOTE the divergence from python, which also carries
//! `$loader:[persist_blob,load_blob]`: python centralizes persistence through an
//! agent→loader `persist_blob` call, but the rust substrate persists INLINE
//! (kernel-native writes to a discovered `file_bridge` store, run as operator), so
//! there is no agent→loader persist call to grant. A bundle that persists through
//! its own `file_bridge_id` gets an explicit store grant instead. This is a
//! deliberate per-runtime difference, like the root-id asymmetry.
//!
//! PRIVILEGED verbs (`create_agent`/`delete_agent`/`update_agent`) are never
//! covered by a wildcard — they must be named explicitly — EXCEPT when the target
//! is the sender itself (forking/editing your OWN subtree is baseline, not
//! escalation). This plugs the depth-1 hole: a root child's `$parent:*` cannot
//! mutate the root.

use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// The one authorization verdict.
#[derive(Debug, Clone)]
pub struct Decision {
    /// Whether the send is permitted.
    pub allowed: bool,
    /// On denial, a one-line statement of what was refused.
    pub reason: String,
    /// On denial, a teaching pointer: how to open the edge (operator only).
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

/// The Unix baseline stamped + merged on every `create_agent`. Command your own
/// subtree (self + children you spawn) and answer your creator, nothing else.
pub fn default_can_call() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("$self".into(), json!("*"));
    m.insert("$parent".into(), json!("*"));
    m.insert("$children".into(), json!("*"));
    m
}

fn is_privileged(verb: &str) -> bool {
    PRIVILEGED_VERBS.contains(&verb)
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

/// Deny-all + egress grants. Pure — the gate resolves the tree facts (parent,
/// children, loader) and hands them in.
///
/// `can_call` is the sender's `meta["can_call"]` (a JSON object, or `Null`/absent
/// ⇒ empty ⇒ deny-all). `target_id` is the send target; the privileged-self
/// exemption is keyed on it (mirrors python: create_agent to self = fork).
#[allow(clippy::too_many_arguments)]
pub fn authorize_call(
    can_call: &Value,
    verb: &str,
    target_id: &str,
    sender_id: &str,
    parent_id: Option<&str>,
    child_ids: &HashSet<String>,
    loader_id: Option<&str>,
) -> Decision {
    let privileged = is_privileged(verb) && target_id != sender_id;
    if let Some(map) = can_call.as_object() {
        for (key, grant) in map {
            let matched = match key.as_str() {
                "$self" => sender_id == target_id,
                "$parent" => parent_id == Some(target_id),
                "$children" => child_ids.contains(target_id),
                "$loader" => loader_id == Some(target_id),
                literal => literal == target_id,
            };
            if matched && verb_allowed(grant, verb, privileged) {
                return Decision::allow();
            }
        }
    }
    Decision {
        allowed: false,
        reason: format!("{sender_id} may not call '{verb}' on {target_id}"),
        hint: format!(
            "grant it on {sender_id}'s can_call — set it at create_agent, ask the \
             operator, or open your OWN reach with update_agent on yourself \
             (id={sender_id}, can_call.{target_id}=['{verb}'], or '*' for all verbs). \
             Agents are deny-all by default."
        ),
    }
}

#[cfg(test)]
mod tests;
