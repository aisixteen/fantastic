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
//! (the persistence root; rarely named, NOT in the baseline), and
//! `$subtree:<X>` — X or any LIVE descendant of X, resolved at call time
//! (`callable_by` ONLY: a target accepting a whole subtree that is spawned
//! dynamically, so nobody has to name each member; never a foreign id).
//!
//! A `can_call` value may also be a HERITABLE entry,
//! `{<target id>: {"verbs": [...], "for": "descendants"}}`: it reaches for the
//! holder's DESCENDANTS, never for the holder itself — the egress half for agents
//! spawned under an owner that must not hold the reach itself. The egress check
//! walks the sender's ancestors for one when the sender's own `can_call` refuses.
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
//! An id in a grant is a local agent id (never containing `|`) or a FOREIGN agent
//! `<guid>|<agent>` — a call relayed by a bridge leg pinned to that peer;
//! `<guid>|*` names every agent of that one kernel. A foreign caller is judged on
//! the TARGET's `callable_by` by its qualified id; the structural tokens never
//! match it, so the baseline gives foreign agents nothing.
//!
//! PRIVILEGED verbs (the tree mutators, the bulk reads, the kernel's lifecycle and
//! task controls, the loader's record verbs) are never covered by a wildcard under
//! a STRUCTURAL token (`$self`/`$parent`/`$children`/`$loader`/`$subtree:<X>`) or
//! in a heritable entry — EXCEPT when the other end is the holder itself (acting on
//! your OWN subtree is baseline). A NAMED verb covers one. A `*` under a NAMED id
//! is the operator's own statement and covers everything. This is an EGRESS rule
//! (`enforce_privileged`): on the ingress side a `$children:*` callee legitimately
//! accepts a privileged verb, since egress already guards escalation.

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

/// Verbs a STRUCTURAL wildcard never covers (`$parent: "*"` and the other tokens —
/// the baseline the kernel stamps on every agent). Under a token they must be
/// named explicitly, because the baseline is a statement about REACH and these are
/// statements about AUTHORITY or about BULK READS: "talk to your parent" can never
/// silently become "mutate the graph / stop the kernel / kill any task through your
/// parent". A `*` under a NAMED id covers them — that grant is the operator's own.
///
/// The mutators are here because they change the tree. `state_subscribe` is here
/// because its operand is the WHOLE tree: the stream carries every send's
/// sender, target, verb and payload summary, for every agent. Without it, the
/// structural baseline (`$parent: "*"`) silently handed the entire stream to
/// every direct child of the root — which is how a leaf browser leg could tap it
/// while holding no grant that names it (dataroom audit, 2026-08-27).
pub const PRIVILEGED_VERBS: [&str; 16] = [
    "create_agent",
    "delete_agent",
    "update_agent",
    "state_subscribe",
    // A file_bridge's EXPOSURE: `allow` widens what the bridge serves,
    // `disallow` narrows it. Under the baseline alone any parent or child
    // could `allow .` and reopen the deny-all (audit 2026-09-28).
    "allow",
    "disallow",
    // The kernel's LIFECYCLE and TASK CONTROLS, answered by the root. Under the
    // baseline alone, every direct child of the root could stop the kernel or
    // kill ANY task (audit 2026-09-28). `ledger` is the bulk read of every
    // execution; `ack_deadletter` retires one.
    "kernel_start",
    "kernel_stop",
    "shutdown_agents",
    "ps",
    "cancel",
    "ledger",
    "ack_deadletter",
    // The LOADER's record verbs — they write, delete and read the persisted tree,
    // grants included.
    "persist_record",
    "forget_record",
    "load_tree",
];

/// The structural tokens — the baseline's vocabulary. A privileged verb loses a
/// wildcard's cover only under one of these.
pub const STRUCTURAL_TOKENS: [&str; 4] = ["$self", "$parent", "$children", "$loader"];

/// `$subtree:<X>` — X or any live descendant of X. `callable_by` ONLY.
pub const SUBTREE_PREFIX: &str = "$subtree:";

/// The live-tree resolver `authorize_call` asks for `$subtree:<X>`:
/// `(other_id, x)` → is other_id X or a live descendant of X.
pub type InSubtree<'a> = &'a dyn Fn(&str, &str) -> bool;

/// A heritable `can_call` entry's one audience.
pub const HERITABLE_FOR: &str = "descendants";

/// A heritable `can_call` entry: exactly `{"verbs": [...], "for": "descendants"}`,
/// `verbs` a non-empty list of strings.
pub fn is_heritable(value: &Value) -> bool {
    let Some(m) = value.as_object() else {
        return false;
    };
    m.len() == 2
        && m.get("for").and_then(Value::as_str) == Some(HERITABLE_FOR)
        && m.get("verbs")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_string))
}

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
    if grant.is_object() {
        return false; // a heritable entry never grants its HOLDER
    }
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
///
/// `in_subtree(other_id, x)` answers "is other_id x or a live descendant of x" —
/// the caller's live tree, asked at call time; `None` ⇒ `$subtree:` keys match
/// nothing.
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
    in_subtree: Option<InSubtree<'_>>,
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
                sub if sub.starts_with(SUBTREE_PREFIX) => {
                    let x = &sub[SUBTREE_PREFIX.len()..];
                    !x.is_empty()
                        && !other_id.contains('|')
                        && in_subtree.is_some_and(|f| f(other_id, x))
                }
                lit if lit.starts_with('$') => false,
                // every agent of ONE foreign kernel: `<guid>|*` (a local id never
                // carries `|`, so this can only ever match a foreign caller)
                star if star.ends_with("|*") => other_id.starts_with(&star[..star.len() - 1]),
                // a local id, or a foreign `<guid>|<agent>`
                literal => literal == other_id,
            };
            let structural =
                STRUCTURAL_TOKENS.contains(&key.as_str()) || key.starts_with(SUBTREE_PREFIX);
            if matched && verb_allowed(g, verb, privileged && structural) {
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

/// The heritable half of EGRESS: may a sender call `target_id` through a
/// heritable `can_call` entry one of its ANCESTORS holds? `ancestor_grants` is
/// their `can_call` maps, nearest first. A NAMED verb covers a privileged one;
/// `*` covers only an ordinary verb. First allow wins.
pub fn authorize_heritable<'a>(
    ancestor_grants: impl IntoIterator<Item = &'a Value>,
    verb: &str,
    target_id: &str,
) -> Decision {
    for grant in ancestor_grants {
        let Some(entry) = grant.get(target_id) else {
            continue;
        };
        if !is_heritable(entry) {
            continue;
        }
        let verbs = entry.get("verbs").and_then(Value::as_array);
        let names = |w: &str| verbs.is_some_and(|a| a.iter().any(|v| v.as_str() == Some(w)));
        if names(verb) || (names("*") && !PRIVILEGED_VERBS.contains(&verb)) {
            return Decision::allow();
        }
    }
    Decision {
        allowed: false,
        reason: String::new(),
        hint: FROM_ABOVE.to_string(),
    }
}

/// The first verb of a CONFERRED heritable entry's `verbs` that the granter's
/// HELD entry under the same key does not cover, or `None` when all are covered.
/// `*` needs `*`; a named verb needs the name, or `*` when it is not privileged.
/// A held value that is not a heritable entry holds nothing.
pub fn heritable_uncovered(held: Option<&Value>, want: &[Value]) -> Option<String> {
    let have: Vec<&str> = match held {
        Some(h) if is_heritable(h) => h["verbs"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    want.iter().filter_map(Value::as_str).find_map(|v| {
        let covered = have.contains(&v)
            || (v != "*" && have.contains(&"*") && !PRIVILEGED_VERBS.contains(&v));
        (!covered).then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests;
