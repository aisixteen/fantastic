// The MINIMAL call gate (owner decision 2026-10-03) — mirror of the python
// `tests/kernel/test_minimal_gate.py`. Unit tests of the one matching engine, then
// the kernel gate: two-sided allow/deny, `"*"` covers create_agent, the tree verbs
// acting anywhere once granted, and the write-time grant-key check.

import FantasticJSON
import OrderedCollections
import Testing

@testable import FantasticKernel

// The tree: boss → worker → kid → grandkid. Every id above `other`.
private func ancestorsOf(_ other: String) -> Set<String> {
    switch other {
    case "worker": return ["boss"]
    case "kid": return ["worker", "boss"]
    case "grandkid": return ["kid", "worker", "boss"]
    default: return []
    }
}

// holder "worker", parent "boss", child "kid", grandchild "grandkid".
private func auth(_ grant: JSON, _ verb: String, _ other: String) -> Decision {
    authorizeCall(
        grant: grant, verb: verb, otherId: other,
        holderId: "worker", parentId: "boss", ancestorIds: ancestorsOf(other))
}

@Test func absentGrantDeniesEverything() {
    let d = auth(.null, "send", "peer")
    #expect(!d.allowed)
    #expect(!d.hint.isEmpty)
}

@Test func emptyGrantDenies() {
    #expect(!auth(.object([:]), "send", "peer").allowed)
}

@Test func selfParentChildrenResolveRelativeToTheHolder() {
    #expect(auth(["$self": "*"], "ping", "worker").allowed)
    #expect(!auth(["$self": "*"], "ping", "boss").allowed)
    #expect(auth(["$parent": "*"], "ping", "boss").allowed)
    #expect(!auth(["$parent": "*"], "ping", "worker").allowed)
    #expect(auth(["$subtree": "*"], "ping", "kid").allowed)
    #expect(auth(["$subtree": "*"], "ping", "grandkid").allowed)
    #expect(!auth(["$subtree": "*"], "ping", "worker").allowed)
    #expect(!auth(["$subtree": "*"], "ping", "stranger").allowed)
}

@Test func removedTokensMatchNothing() {
    #expect(!auth(["$loader": "*"], "ping", "root").allowed)
    #expect(!auth(["$children": "*"], "ping", "kid").allowed)
    #expect(!auth(["$subtree:boss": "*"], "ping", "boss").allowed)
}

@Test func starCoversEveryVerbIncludingTheMutators() {
    for verb in ["ping", "create_agent", "update_agent", "delete_agent", "state_subscribe",
        "kernel_stop", "ps", "cancel", "ledger", "stop", "boot", "watch"]
    {
        #expect(auth(["kernel_state": "*"], verb, "kernel_state").allowed, "\(verb)")
        #expect(auth(["$parent": "*"], verb, "boss").allowed, "\(verb)")
    }
}

@Test func aVerbListGrantsExactlyThose() {
    let g: JSON = ["peer": ["ping", "read"]]
    #expect(auth(g, "ping", "peer").allowed)
    #expect(auth(g, "read", "peer").allowed)
    #expect(!auth(g, "write", "peer").allowed)
    #expect(auth(["peer": ["*"]], "anything", "peer").allowed)
}

@Test func foreignKeysMatchOnlyTheirKernel() {
    #expect(auth(["g2|scout": "*"], "ping", "g2|scout").allowed)
    #expect(!auth(["g2|scout": "*"], "ping", "g3|scout").allowed)
    #expect(auth(["g2|*": ["ping"]], "ping", "g2|any").allowed)
    #expect(!auth(["g2|*": ["ping"]], "ping", "g3|any").allowed)
}

@Test func theBaselineIsSelfOnly() {
    #expect(defaultCanCall() == ["$self": "*"])
    #expect(defaultCallableBy() == ["$self": "*"])
}

@Test func grantKeysAreChecked() {
    for field in ["can_call", "callable_by"] {
        for good in ["peer", "$self", "$parent", "$subtree"] {
            #expect(isGrantKey(field, good), "\(field) \(good)")
        }
        for bad in ["", "*", "$loader", "$subtree:x", "$foo", "|x", "g2|", "a|b|c"] {
            #expect(!isGrantKey(field, bad), "\(field) \(bad)")
        }
    }
    // A peer kernel's agent is an OUTBOUND address: `can_call` only — a caller is
    // always local, so in `callable_by` it could never match.
    for peer in ["g2|scout", "g2|*"] {
        #expect(isGrantKey("can_call", peer), "\(peer)")
        #expect(!isGrantKey("callable_by", peer), "\(peer)")
    }
}

// MARK: - the kernel gate

private struct Noop: AgentBundle {
    let name = "noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }
}

/// root `kernel_state` accepting `a` (`*`); `a` may call the root and `b`
/// (`*`); `b` accepts `a` for `ping` only; `kid` is `a`'s child; `other` is a
/// child of the root.
private func gated() async -> Kernel {
    let r = BundleRegistry()
    r.register("noop.tools", Noop())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(
        id: "kernel_state", handlerModule: nil, parentId: nil,
        meta: ["guid": "g1", "callable_by": ["a": "*"]])
    k.register(root)
    k.setRoot(root)
    func seed(_ id: String, _ parent: String, _ extra: OrderedDictionary<String, JSON> = [:]) async
    {
        var p: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": .string(parent), "id": .string(id),
            "handler_module": "noop.tools",
        ]
        for (key, v) in extra { p[key] = v }
        _ = await k.send([Item("kernel", .object(p))])[0].result
    }
    await seed("a", "kernel_state", ["can_call": ["kernel_state": "*", "b": "*"]])
    await seed("b", "kernel_state", ["callable_by": ["a": ["ping"]]])
    await seed("kid", "a")
    await seed("other", "kernel_state")
    return k
}

private func call(_ k: Kernel, as sender: String, _ target: String, _ p: JSON) async -> JSON {
    await KernelTaskLocals.$sender.withValue(AgentId(sender)) {
        await k.send([Item(AgentId(target), p)])[0].result
    }
}

@Suite("minimal gate — the kernel")
struct MinimalGateTests {

    @Test func twoSidedAllowAndDeny() async {
        let k = await gated()
        // both ends grant it
        #expect(await call(k, as: "a", "b", ["type": "ping"])["ok"] == true)
        // egress grants it, ingress does not
        let ingress = await call(k, as: "a", "b", ["type": "write"])
        #expect(ingress["side"] == "ingress")
        #expect(ingress["error"] == "b does not accept 'write' from a")
        // ingress would, egress does not
        let egress = await call(k, as: "kid", "b", ["type": "ping"])
        #expect(egress["side"] == "egress")
        #expect(egress["error"] == "kid may not call 'ping' on b")
    }

    /// No baseline beyond `$self`: a child reaches neither its parent
    /// nor the root without a grant, and the root takes the same two-sided check.
    @Test func noParentBaselineAndTheRootIsGatedBothWays() async {
        let k = await gated()
        #expect(await call(k, as: "kid", "a", ["type": "ping"])["side"] == "egress")
        let r = await call(
            k, as: "other", "kernel", ["type": "list_agents"])
        #expect(r["side"] == "egress")
        // egress on the root but no ingress — refused at the root
        k.update(AgentId("other"), ["can_call": ["$self": "*", "kernel_state": "*"]])
        let r2 = await call(k, as: "other", "kernel", ["type": "list_agents"])
        #expect(r2["side"] == "ingress")
        #expect(r2["error"] == "kernel_state does not accept 'list_agents' from other")
    }

    @Test func starCoversCreateAgent() async {
        let k = await gated()
        let r = await call(
            k, as: "a", "kernel",
            ["type": "create_agent", "parent_id": "a", "id": "made", "handler_module": "noop.tools"])
        #expect(r["error"] == .null, "\(r)")
        #expect(k.agent(AgentId("made"))?.parentId == AgentId("a"))
    }

    /// GRANTS ARE THE ONLY RULE (owner, 2026-10-06): holding the tree verbs on the
    /// root, an agent creates, updates and deletes ANY agent it names — outside its
    /// own subtree and itself included.
    @Test func aTreeVerbGrantedOnTheRootActsAnywhere() async {
        let k = await gated()
        let create = await call(
            k, as: "a", "kernel",
            ["type": "create_agent", "parent_id": "b", "id": "x", "handler_module": "noop.tools"])
        #expect(create["error"] == .null, "\(create)")
        #expect(k.agent(AgentId("x"))?.parentId == AgentId("b"))
        let update = await call(
            k, as: "a", "kernel", ["type": "update_agent", "id": "other", "description": "x"])
        #expect(update["updated"] == true, "\(update)")
        let me = await call(k, as: "a", "kernel", ["type": "update_agent", "id": "a", "description": "y"])
        #expect(me["updated"] == true, "\(me)")
        let gone = await call(k, as: "a", "kernel", ["type": "delete_agent", "id": "other"])
        #expect(gone["error"] == .null, "\(gone)")
        #expect(k.agent(AgentId("other")) == nil)
    }

    @Test func anAncestorMutatesAtAnyDepth() async {
        let k = await gated()
        // `deep` under `kid` under `a`: in `a`'s subtree, two levels down
        let made = await call(
            k, as: "a", "kernel",
            ["type": "create_agent", "parent_id": "kid", "id": "deep", "handler_module": "noop.tools"])
        #expect(made["error"] == .null, "\(made)")
        #expect(k.agent(AgentId("deep"))?.parentId == AgentId("kid"))
        let up = await call(
            k, as: "a", "kernel", ["type": "update_agent", "id": "deep", "description": "x"])
        #expect(up["updated"] == true, "\(up)")
        let gone = await call(k, as: "a", "kernel", ["type": "delete_agent", "id": "deep"])
        #expect(gone["error"] == .null, "\(gone)")
        #expect(k.agent(AgentId("deep")) == nil)
    }

    @Test func anUnknownGrantKeyIsRefusedAtTheWrite() async {
        let k = await gated()
        for bad in ["$loader", "$subtree:a", "*"] {
            let r = await k.send([
                Item(
                    "kernel",
                    [
                        "type": "update_agent", "id": "b",
                        "callable_by": .object([bad: "*"]),
                    ])
            ])[0].result
            #expect(r["reason"] == "invalid_meta", "\(bad): \(r)")
            #expect(
                r["error"].asString
                    == "callable_by[\(pyRepr(bad))] is not a grant key — legal keys: an agent id, "
                    + "$self, $parent, $subtree", "\(r)")
        }
        // a peer kernel's agent is never a caller here: refused in callable_by,
        // accepted in can_call (an outbound address)
        let peerIn = await k.send([
            Item("kernel", ["type": "update_agent", "id": "b", "callable_by": ["g2|scout": "*"]])
        ])[0].result
        #expect(peerIn["reason"] == "invalid_meta", "\(peerIn)")
        let peerOut = await k.send([
            Item("kernel", ["type": "update_agent", "id": "b", "can_call": ["g2|scout": "*"]])
        ])[0].result
        #expect(peerOut["updated"] == true, "\(peerOut)")
        let oldShape = await k.send([
            Item(
                "kernel",
                ["type": "update_agent", "id": "a", "can_call": ["b": ["verbs": ["ping"], "for": "descendants"]]])
        ])[0].result
        #expect(oldShape["reason"] == "invalid_meta")
        #expect(oldShape["error"].asString?.contains(#"must be "*" or a list of verb strings"#) == true)
    }

    @Test func anOperatorIsUngated() async {
        let k = await gated()
        let r = await k.send([
            Item("kernel", ["type": "create_agent", "parent_id": "b", "id": "op", "handler_module": "noop.tools"])
        ])[0].result
        #expect(r["error"] == .null, "\(r)")
    }

    @Test func aWatchIsGatedLikeASend() async {
        let k = await gated()
        _ = k.ensureInbox(AgentId("kid"))
        let denied = KernelTaskLocals.$sender.withValue(AgentId("kid")) {
            k.watch(src: AgentId("b"), watcher: AgentId("kid"))
        }
        #expect(denied?["reason"] == "unauthorized")
        _ = k.ensureInbox(AgentId("a"))
        // a may watch the root: a's can_call names it with "*", the root accepts a
        let ok = KernelTaskLocals.$sender.withValue(AgentId("a")) {
            k.watch(src: AgentId("kernel_state"), watcher: AgentId("a"))
        }
        #expect(ok == nil)
        #expect(
            KernelTaskLocals.$sender.withValue(AgentId("a")) {
                k.authorizeStateSubscribe(AgentId("a"))
            } == nil)
    }
}
