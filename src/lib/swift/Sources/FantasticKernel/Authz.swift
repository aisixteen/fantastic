// Call security — the substrate authorization primitive.
//
// Mirror of the python canonical `kernel/_authz.py` (and rust `authz.rs`).
// The model is **fully-gated, two-sided, from-above-only**:
//
// - Every AGENT-originated send (`currentSender` is non-nil) is checked TWICE:
//   egress — the SENDER's `can_call` must allow `(target, verb)` — AND ingress —
//   the TARGET's `callable_by` must allow `(sender, verb)`. An edge exists only if
//   BOTH ends grant it. The gate NEVER branches on a verb name: nothing is exempt,
//   not even `reflect`.
// - An OPERATOR send (`currentSender` is nil: the CLI, cold bootstrap, a loader
//   hydrating at boot, a test driver, or a verb completing its own side effect
//   under `$currentSender.withValue(nil)`) is the origin root of trust and never
//   reaches this file — the gate skips it.
//
// Both `can_call` and `callable_by` are grant maps of the same shape,
// `{ id | token : ["<verb>", …] | "*" }`. Tokens resolve at the gate relative to
// the grant's HOLDER: `$self`, `$parent`, `$children` (set membership), `$loader`
// (the persistence root; rarely named, NOT in the baseline). The structural
// baseline stamped + MERGED on every agent (root, created, disk-loaded — in
// `Agent.init`), on BOTH fields, is `{$self:*, $parent:*, $children:*}` — reach/
// accept your own subtree and your creator. No `"*"` wildcard token.
//
// Authority flows from ABOVE only: grants are set by the creator/operator at
// create_agent, or a parent delegates a subset of its own `can_call`; an agent
// can NEVER raise its own authority (enforced at the gate, not here).
//
// PRIVILEGED verbs (`create_agent`/`delete_agent`/`update_agent`) are never
// covered by a wildcard — named explicitly — EXCEPT when the other end is the
// holder itself (forking your OWN subtree is baseline). This is an EGRESS rule
// (`enforcePrivileged`): on the ingress side a `$children:*` callee legitimately
// accepts a privileged verb, since egress already guards escalation.

import FantasticJSON
import OrderedCollections

/// The one authorization verdict.
public struct Decision {
    /// Whether the send is permitted.
    public let allowed: Bool
    /// On denial, a one-line statement of what was refused.
    public let reason: String
    /// On denial, a teaching pointer: authority descends, never self-raised.
    public let hint: String
}

/// Structural graph mutation — privileged (a wildcard never satisfies these).
public let privilegedVerbs: Set<String> = ["create_agent", "delete_agent", "update_agent"]

/// The remedy shown on any denial: authority descends, it is never self-raised.
public let fromAbove: String =
    "authority is granted from ABOVE — set at create_agent by your creator, or by "
    + "the operator; a parent may delegate a subset of its own reach. You cannot "
    + "grant it to yourself. Agents are deny-all by default (both to call and to be called)."

/// The structural baseline stamped + merged on every agent, on `can_call`.
public func defaultCanCall() -> JSON {
    .object([
        "$self": .string("*"),
        "$parent": .string("*"),
        "$children": .string("*"),
    ])
}

/// The structural baseline for `callable_by` — mirror of `defaultCanCall`.
public func defaultCallableBy() -> JSON {
    defaultCanCall()
}

/// A grant value is `"*"` (any verb) or a list of verb names (which may itself
/// contain `"*"`). A PRIVILEGED verb loses its wildcard cover — only an explicit
/// list entry names it.
private func verbAllowed(_ grant: JSON, _ verb: String, privileged: Bool) -> Bool {
    if privileged {
        if case .array(let items) = grant {
            return items.contains { $0.asString == verb }
        }
        return false
    }
    if grant.asString == "*" { return true }
    if case .array(let items) = grant {
        return items.contains { $0.asString == verb || $0.asString == "*" }
    }
    return false
}

/// One matching engine, used BOTH ways. `grant` is the HOLDER's grant map
/// (`can_call` for egress, `callable_by` for ingress); `otherId` is the id at the
/// other end of the edge (the target for egress, the sender for ingress). Tokens
/// resolve relative to the holder. Absent/empty grant ⇒ deny.
///
/// `enforcePrivileged` gates the wildcard-exclusion for PRIVILEGED verbs — an
/// EGRESS concern only; the gate passes `false` on the ingress check.
public func authorizeCall(
    grant: JSON,
    verb: String,
    otherId: String,
    holderId: String,
    parentId: String?,
    childIds: Set<String>,
    loaderId: String?,
    enforcePrivileged: Bool = true
) -> Decision {
    // Privileged verbs lose wildcard cover UNLESS the other end is the holder
    // itself (acting on your own subtree is baseline) — egress only.
    let privileged =
        enforcePrivileged && privilegedVerbs.contains(verb) && otherId != holderId
    if case .object(let map) = grant {
        for (key, g) in map {
            let matched: Bool
            switch key {
            case "$self": matched = otherId == holderId
            case "$parent": matched = parentId == otherId
            case "$children": matched = childIds.contains(otherId)
            case "$loader": matched = loaderId == otherId
            case let k where k.hasPrefix("$"): matched = false
            default: matched = key == otherId
            }
            if matched && verbAllowed(g, verb, privileged: privileged) {
                return Decision(allowed: true, reason: "", hint: "")
            }
        }
    }
    return Decision(allowed: false, reason: "", hint: fromAbove)
}
