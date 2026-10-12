// Call security — the substrate authorization primitive.
//
// Mirror of the python canonical `kernel/_authz.py` (and rust `authz.rs`).
// The model is MINIMAL (owner decision 2026-10-03):
//
// - Every AGENT-originated send (`currentSender` is non-nil) is checked TWICE:
//   egress — the SENDER's `can_call` must grant `(target, verb)` — AND ingress —
//   the TARGET's `callable_by` must grant `(sender, verb)`. An edge exists only if
//   BOTH ends grant it. The gate NEVER branches on a verb name: nothing is exempt,
//   not even `reflect`, and kernel verbs answered by the root take the same
//   two-sided check against the root.
// - An OPERATOR send (`currentSender` is nil: a send made outside every dispatch —
//   the CLI, the cold bootstrap, a test driver, a transport handing an inbound
//   frame to its leg) is the origin root of trust and never reaches this file —
//   the gate skips it. Identity comes ONLY from dispatch: the sends a handler
//   makes (and the tasks it spawns) are that agent's.
//
// Both `can_call` and `callable_by` are grant maps of the same shape,
// `{key: "*" | ["<verb>", …]}`. `"*"` covers EVERY verb. A key is one of:
//   - a literal local agent id (never containing `|`);
//   - `$self` (the holder), `$parent` (its parent), `$subtree` (every agent
//     BELOW it, any depth) — resolved relative to the grant's HOLDER at call time;
//   - `<guid>|<agent>` / `<guid>|*` — one agent / every agent of a connected
//     kernel: `can_call` ONLY (an outbound peer address).
// A caller is always a LOCAL agent: a call a bridge leg serves is judged as the
// LEG, so a `<guid>|…` key in `callable_by` could never match and is refused at
// the write. Nothing else is a key: a write naming another one is refused
// `invalid_meta`.
//
// The baseline stamped + MERGED on every agent (root, created, disk-loaded — in
// `Agent.init`), on BOTH fields, is `{$self:*}` — nothing else. Explicit grants
// only.

import FantasticJSON
import OrderedCollections

/// The one authorization verdict.
public struct Decision {
    /// Whether the send is permitted.
    public let allowed: Bool
    /// On denial, a one-line statement of what was refused.
    public let reason: String
    /// On denial, a teaching pointer.
    public let hint: String
}

/// The remedy shown on any denial: authority descends, it is never self-raised.
public let fromAbove: String =
    "grants come from above — an ancestor (update_agent on an agent in its "
    + "subtree) or the operator writes can_call and callable_by; an agent never "
    + "raises its own"

/// The baseline stamped + merged on every agent, on `can_call`: `{$self: "*"}`.
public func defaultCanCall() -> JSON {
    .object(["$self": .string("*")])
}

/// The baseline for `callable_by` — mirror of `defaultCanCall`.
public func defaultCallableBy() -> JSON {
    defaultCanCall()
}

/// The legal-key lists, as the write-time shape refusal names them — per
/// field: a caller is always local, so only `can_call` may name a peer kernel's
/// agent. Python `LEGAL_GRANT_KEYS`.
public func legalGrantKeys(_ field: String) -> String {
    field == "can_call"
        ? "an agent id, $self, $parent, $subtree, <guid>|<agent> or <guid>|*"
        : "an agent id, $self, $parent, $subtree"
}

/// The structural tokens — the only `$` keys a grant map may carry.
public let grantTokens: Set<String> = ["$self", "$parent", "$subtree"]

/// Is `key` a legal key of grant `field`? Legal on both: a literal agent id
/// (non-empty, no `$` prefix, no `|`, not `*`), `$self`/`$parent`/`$subtree`.
/// `can_call` also takes `<guid>|<agent>` or `<guid>|*` (exactly one `|`, both
/// halves non-empty) — an outbound peer address; in `callable_by` such a key
/// could never match a caller and is refused. Python `_authz.bad_grant_key`.
public func isGrantKey(_ field: String, _ key: String) -> Bool {
    if key.isEmpty { return false }
    if key.hasPrefix("$") { return grantTokens.contains(key) }
    if let bar = key.firstIndex(of: "|") {
        if field != "can_call" { return false }
        let guid = key[..<bar]
        let agent = key[key.index(after: bar)...]
        return !guid.isEmpty && !agent.isEmpty && !agent.contains("|")
    }
    return key != "*"
}

/// A grant value is `"*"` (every verb) or a list of verb names (a `"*"` entry
/// in the list covers every verb too).
private func verbAllowed(_ grant: JSON, _ verb: String) -> Bool {
    if grant.asString == "*" { return true }
    if case .array(let items) = grant {
        return items.contains { $0.asString == verb || $0.asString == "*" }
    }
    return false
}

/// One matching engine, used BOTH ways. `grant` is the HOLDER's grant map
/// (`can_call` for egress, `callable_by` for ingress); `otherId` is the id at the
/// other end of the edge (the target for egress, the sender for ingress). Tokens
/// resolve relative to the holder. Absent/empty grant ⇒ deny. `ancestorIds` is
/// every id above the other end — `$subtree` matches when the holder is one.
public func authorizeCall(
    grant: JSON,
    verb: String,
    otherId: String,
    holderId: String,
    parentId: String?,
    ancestorIds: Set<String>
) -> Decision {
    if case .object(let map) = grant {
        for (key, g) in map {
            let matched: Bool
            switch key {
            case "$self": matched = otherId == holderId
            case "$parent": matched = parentId == otherId
            case "$subtree": matched = ancestorIds.contains(holderId)
            case let k where k.hasPrefix("$"): matched = false
            // every agent of ONE peer kernel: `<guid>|*` (a local id never
            // carries `|`, so this only ever matches an outbound peer target)
            case let k where k.hasSuffix("|*"): matched = otherId.hasPrefix(String(k.dropLast()))
            // a local id, or a peer target `<guid>|<agent>`
            default: matched = key == otherId
            }
            if matched && verbAllowed(g, verb) {
                return Decision(allowed: true, reason: "", hint: "")
            }
        }
    }
    return Decision(allowed: false, reason: "", hint: fromAbove)
}

