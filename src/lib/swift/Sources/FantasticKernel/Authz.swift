// Call security — the substrate authorization primitive.
//
// Mirror of the python canonical `kernel/_authz.py` (and rust `authz.rs`).
// Deny-all is the kernel law for an AGENT-originated send (`currentSender` is
// non-nil): permitted only by an explicit grant on the SENDER's record,
// `can_call: { target | token : ["<verb>", …] | "*" }`. An OPERATOR send
// (`currentSender` is nil: the CLI, cold bootstrap, a loader hydrating at boot, a
// test driver calling `kernel.send` directly) is the root of trust and never
// reaches this file — the gate skips it. `reflect` is never gated.
//
// Tokens resolve at the gate: `$self`, `$parent`, `$children` (set membership),
// `$loader` (the root). The Unix baseline the kernel stamps + MERGES on every
// `create_agent` is `{$self:*, $parent:*, $children:*}` — command your own subtree
// and answer your creator. NOTE the divergence from python, which also carries
// `$loader:[persist_blob,load_blob]`: python persists through an agent→loader
// `persist_blob` call, but the swift substrate persists INLINE (kernel-native
// writes to a discovered `file_bridge` store, run as operator), so there is no
// agent→loader persist call to grant. A bundle that persists through its own
// `file_bridge_id` gets an explicit store grant instead. Deliberate per-runtime
// difference, like the root-id asymmetry.
//
// PRIVILEGED verbs (`create_agent`/`delete_agent`/`update_agent`) are never
// covered by a wildcard — they must be named explicitly — EXCEPT when the target
// is the sender itself (forking/editing your OWN subtree is baseline, not
// escalation). This plugs the depth-1 hole: a root child's `$parent:*` cannot
// mutate the root.

import FantasticJSON
import OrderedCollections

/// The one authorization verdict.
public struct Decision {
    /// Whether the send is permitted.
    public let allowed: Bool
    /// On denial, a one-line statement of what was refused.
    public let reason: String
    /// On denial, a teaching pointer: how to open the edge (operator only).
    public let hint: String
}

/// Structural graph mutation — privileged (a wildcard never satisfies these).
public let privilegedVerbs: Set<String> = ["create_agent", "delete_agent", "update_agent"]

/// The Unix baseline stamped + merged on every `create_agent`.
public func defaultCanCall() -> JSON {
    .object([
        "$self": .string("*"),
        "$parent": .string("*"),
        "$children": .string("*"),
    ])
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

/// Deny-all + egress grants. Pure — the gate resolves the tree facts (parent,
/// children, loader) and hands them in. `canCall` is the sender's
/// `meta["can_call"]` (a JSON object, or `.null`/absent ⇒ empty ⇒ deny-all).
public func authorizeCall(
    canCall: JSON,
    verb: String,
    targetId: String,
    senderId: String,
    parentId: String?,
    childIds: Set<String>,
    loaderId: String?
) -> Decision {
    let privileged = privilegedVerbs.contains(verb) && targetId != senderId
    if case .object(let map) = canCall {
        for (key, grant) in map {
            let matched: Bool
            switch key {
            case "$self": matched = senderId == targetId
            case "$parent": matched = parentId == targetId
            case "$children": matched = childIds.contains(targetId)
            case "$loader": matched = loaderId == targetId
            default: matched = key == targetId
            }
            if matched && verbAllowed(grant, verb, privileged: privileged) {
                return Decision(allowed: true, reason: "", hint: "")
            }
        }
    }
    return Decision(
        allowed: false,
        reason: "\(senderId) may not call '\(verb)' on \(targetId)",
        hint:
            "grant it on \(senderId)'s can_call — set it at create_agent, ask the "
            + "operator, or open your OWN reach with update_agent on yourself "
            + "(id=\(senderId), can_call.\(targetId)=['\(verb)'], or '*' for all verbs). "
            + "Agents are deny-all by default."
    )
}
