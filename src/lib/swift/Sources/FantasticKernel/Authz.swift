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
// (the persistence root; rarely named, NOT in the baseline), `$subtree:<X>` (X or
// any LIVE descendant of X, resolved at call time — callable_by ONLY, never a
// foreign id: a target accepting a whole subtree spawned dynamically).
//
// A `can_call` value may also be a HERITABLE entry,
// `{<target id>: {"verbs": [...], "for": "descendants"}}`: it reaches for the
// holder's DESCENDANTS, never for the holder itself. The egress check walks the
// sender's ancestors for one when the sender's own `can_call` refuses.
//
// The structural baseline stamped + MERGED on every agent (root, created, disk-loaded — in
// `Agent.init`), on BOTH fields, is `{$self:*, $parent:*, $children:*}` — reach/
// accept your own subtree and your creator. No `"*"` wildcard token.
//
// Authority flows from ABOVE only: grants are set by the creator/operator at
// create_agent, or a parent delegates a subset of its own `can_call`; an agent
// can NEVER raise its own authority (enforced at the gate, not here).
//
// An id in a grant is a local agent id (never containing `|`) or a FOREIGN agent
// `<guid>|<agent>` — a call relayed by a bridge leg pinned to that peer;
// `<guid>|*` names every agent of that one kernel. A foreign caller is judged on
// the TARGET's `callable_by` by its qualified id; the structural tokens never
// match it, so the baseline gives foreign agents nothing.
//
// PRIVILEGED verbs (the tree mutators, the bulk reads, the kernel's lifecycle and
// task controls, the loader's record verbs, every agent's `stop`/`mount`/
// `unmount`/`boot`) are never covered by a wildcard under a STRUCTURAL token (the
// baseline, and `$subtree:<X>`) or in a heritable entry — a NAMED verb covers one
// — EXCEPT when the other end is the holder itself (acting on your OWN subtree is
// baseline). A `*` under a NAMED id is the operator's own statement and covers
// everything. `watch` whose src is the ROOT loses a structural wildcard's cover
// the same way. This is an EGRESS rule
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

/// Verbs a STRUCTURAL wildcard never covers (`$parent: "*"` and the other tokens —
/// the baseline the kernel stamps on every agent). Under a token they must be
/// named explicitly, because the baseline is a statement about REACH and these are
/// statements about AUTHORITY or about BULK READS: "talk to your parent" can never
/// silently become "mutate the graph / stop the kernel / kill any task through
/// your parent". A `*` under a NAMED id covers them — that grant is the
/// operator's own.
///
/// The mutators are here because they change the tree. `state_subscribe` is here
/// because its operand is the WHOLE tree: the stream carries every send's
/// sender, target, verb and payload summary, for every agent. Without it, the
/// structural baseline (`$parent: "*"`) silently handed the entire stream to
/// every direct child of the root — which is how a leaf browser leg could tap it
/// while holding no grant that names it (dataroom audit, 2026-08-27).
public let privilegedVerbs: Set<String> = [
    "create_agent", "delete_agent", "update_agent", "state_subscribe",
    // A file_bridge's EXPOSURE: `allow` widens what the bridge serves, `disallow`
    // narrows it. Under the baseline alone any parent or child could `allow .`
    // and reopen the deny-all (audit 2026-09-28).
    "allow", "disallow",
    // The kernel's LIFECYCLE and TASK CONTROLS, answered by the root. Under the
    // baseline alone, every direct child of the root could stop the kernel or kill
    // ANY task (audit 2026-09-28). `ledger` is the bulk read of every execution;
    // `ack_deadletter` retires one.
    "kernel_start", "kernel_stop", "shutdown_agents", "ps", "cancel", "ledger",
    "ack_deadletter",
    // The LOADER's record verbs — they write, delete and read the persisted tree,
    // grants included.
    "persist_record", "forget_record", "load_tree",
    // An agent's own LIFECYCLE and a host's ROUTES. By verb name, whatever the
    // bundle: a web_ws/web_rest leg's `$parent:*` let any client it admitted stop
    // its web host for every client, or unmount a sealed sibling's routes (audit
    // 2026-10-03, BV-1).
    "stop", "mount", "unmount", "boot",
]

/// The structural tokens — the baseline's vocabulary. A privileged verb loses a
/// wildcard's cover only under one of these (and under a `$subtree:<X>` key).
public let structuralTokens: Set<String> = ["$self", "$parent", "$children", "$loader"]

/// `$subtree:<X>` — X or any live descendant of X. callable_by ONLY.
public let subtreePrefix = "$subtree:"

/// A heritable `can_call` entry's one audience.
public let heritableFor = "descendants"

/// A heritable `can_call` entry: exactly `{"verbs": [<non-empty str list>], "for": "descendants"}`.
/// Returns its verbs, or nil when `value` is not one.
public func heritableVerbs(_ value: JSON) -> [String]? {
    guard case .object(let m) = value, m.count == 2,
        m["for"]?.asString == heritableFor,
        case .array(let items)? = m["verbs"], !items.isEmpty
    else { return nil }
    var verbs: [String] = []
    for item in items {
        guard let v = item.asString else { return nil }
        verbs.append(v)
    }
    return verbs
}

/// The remedy shown on any denial: authority descends, it is never self-raised.
public let fromAbove: String =
    "authority is granted from ABOVE — set at create_agent by your creator, or by "
    + "the operator; a parent may delegate a subset of its own reach. You cannot "
    + "grant it to yourself. Agents are deny-all by default (both to call and to be called)."

// THE SECRET-HOLDER MARK (owner 2026-10-02). `holds_secrets: true` on a record
// makes that agent unreachable from a model turn (ai_core refuses the call). The
// kernel owns two rules about the field itself, at the write:
//   - its SHAPE (`badHolderMark`): exactly `true`, or `null` — anything else is
//     `invalid_meta`, on every agent, at create_agent and update_agent alike;
//   - who LOWERS it (`checkHolderMark`, in the gate): on an agent whose record
//     says `true`, only the operator may write anything else.
// `null` IS THE CLEAR. update_agent REPLACES a named field, so `holds_secrets: null`
// leaves a record whose value is `null` — not a holder, since only exactly `true`
// marks one — the same way `delete_lock: null` lifts a lock. Python `_agent.py`
// `HOLDS_SECRETS` / `HOLDER_MARK_SIDE` / `HOLDER_MARK_HINT`.
let holdsSecretsField = "holds_secrets"
let holderMarkSide = "secret-holder"
let holderMarkHint =
    "only the operator clears it (update_agent holds_secrets=null); an agent may "
    + "mark a holder, never unmark one"

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
    if case .object = grant { return false }  // a heritable entry never grants its HOLDER
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
///
/// `inSubtree(otherId, x)` answers "is otherId x or a live descendant of x" — the
/// caller's live tree, asked at call time; absent ⇒ `$subtree:` keys match nothing.
public func authorizeCall(
    grant: JSON,
    verb: String,
    otherId: String,
    holderId: String,
    parentId: String?,
    childIds: Set<String>,
    loaderId: String?,
    enforcePrivileged: Bool = true,
    inSubtree: ((String, String) -> Bool)? = nil
) -> Decision {
    // Privileged verbs lose wildcard cover UNLESS the other end is the holder
    // itself (acting on your own subtree is baseline) — egress only.
    // `watch` whose src is the ROOT (`loaderId`) is held to the same rule: the
    // root's inbox carries every kernel verb's payload, `persist_blob` contents
    // included, and `$parent:*` handed it to every direct child (audit 2026-10-03,
    // KV-2). Every other watch is an ordinary verb.
    let privileged =
        enforcePrivileged
        && (privilegedVerbs.contains(verb) || (verb == "watch" && loaderId == otherId))
        && otherId != holderId
    if case .object(let map) = grant {
        for (key, g) in map {
            let matched: Bool
            switch key {
            case "$self": matched = otherId == holderId
            case "$parent": matched = parentId == otherId
            case "$children": matched = childIds.contains(otherId)
            case "$loader": matched = loaderId == otherId
            case let k where k.hasPrefix(subtreePrefix):
                let x = String(k.dropFirst(subtreePrefix.count))
                matched =
                    !x.isEmpty && !otherId.contains("|") && (inSubtree?(otherId, x) ?? false)
            case let k where k.hasPrefix("$"): matched = false
            // every agent of ONE foreign kernel: `<guid>|*` (a local id never
            // carries `|`, so this can only ever match a foreign caller)
            case let k where k.hasSuffix("|*"): matched = otherId.hasPrefix(String(k.dropLast()))
            // a local id, or a foreign `<guid>|<agent>`
            default: matched = key == otherId
            }
            let structural = structuralTokens.contains(key) || key.hasPrefix(subtreePrefix)
            if matched && verbAllowed(g, verb, privileged: privileged && structural) {
                return Decision(allowed: true, reason: "", hint: "")
            }
        }
    }
    return Decision(allowed: false, reason: "", hint: fromAbove)
}

/// The verbs whose OPERAND is a record (`payload.id`): the root answers them, the
/// agent they name is what is acted upon, so that agent is what the gate judges.
/// Python `_authz.RECORD_OPERAND_VERBS`.
public let recordOperandVerbs: Set<String> = ["update_agent", "delete_agent"]

/// EGRESS on a record operand (`update_agent` / `delete_agent` outside the
/// holder's own subtree): does the holder's `can_call` NAME the operand with the
/// verb — a literal key equal to its id whose list holds the verb by name?
///
/// Stricter than `authorizeCall`, on purpose: no structural token resolves to the
/// operand, no `<guid>|*`, and no wildcard covers it — `<id>: "*"` and
/// `<id>: ["*"]` are reach to the agent, never authority over its record.
/// Python `_authz.names_operand`.
public func namesOperand(_ grant: JSON, verb: String, operandId: String) -> Bool {
    guard case .object(let map) = grant, case .array(let verbs)? = map[operandId] else {
        return false
    }
    return verbs.contains(.string(verb))
}

/// The heritable half of EGRESS: may a sender call `targetId` through a heritable
/// `can_call` entry one of its ANCESTORS holds? `ancestorGrants` is their `can_call`
/// maps, nearest first. A NAMED verb covers a privileged one; `*` covers only an
/// ordinary verb — and never `watch` on the ROOT (`loaderId`), which needs an
/// explicit named grant (audit KV-2). First allow wins.
public func authorizeHeritable(
    ancestorGrants: [JSON], verb: String, targetId: String, loaderId: String?
) -> Decision {
    let privileged =
        privilegedVerbs.contains(verb) || (verb == "watch" && loaderId == targetId)
    for grant in ancestorGrants {
        guard case .object(let map) = grant, let entry = map[targetId],
            let verbs = heritableVerbs(entry)
        else { continue }
        if verbs.contains(verb) || (verbs.contains("*") && !privileged) {
            return Decision(allowed: true, reason: "", hint: "")
        }
    }
    return Decision(allowed: false, reason: "", hint: fromAbove)
}
