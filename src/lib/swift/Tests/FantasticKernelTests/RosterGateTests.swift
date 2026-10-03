// C7 — the DISTILLED `tree` is the gated roster (python `_roster`).
//
// `tree=all` with `detail != full` answers `{id, sentence?, description?, mode?,
// children}` — what each agent is FOR, not how it is wired. The sentence is
// ASKED FROM THE AGENT ITSELF through a real internal dispatch, so it cannot
// drift from what the agent says; and the walk is GATED one node at a time,
// because dispatching reflect directly let any caller allowed to reflect ONE
// agent read every descendant's sentence and description through the tree
// (measured 2026-09-02).

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

/// Answers `reflect` with a sentence, a PURPOSE key (`mode`) and a key the
/// roster must NOT copy.
private struct RosterProbe: AgentBundle {
    let name = "roster_probe"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard payload["type"].asString == "reflect" else { return .object(["ok": .bool(true)]) }
        return .object([
            "id": .string(agentId.value),
            "sentence": .string("\(agentId.value) is for testing."),
            "mode": .string("sealed"),
            "verbs": .object(["poke": .string("pokes")]),
        ])
    }
}

/// Its reflect BLOWS UP — python's exception propagates out of `_roster` and
/// fails the whole reflect.
private struct RaisingProbe: AgentBundle {
    let name = "raising_probe"
    struct Boom: Error, CustomStringConvertible { let description = "boom" }
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        throw Boom()
    }
}

@Suite("C7: the distilled tree is the gated roster")
struct RosterGateTests {
    private func makeKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("roster_probe.tools", RosterProbe())
        registry.register("raising_probe.tools", RaisingProbe())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    @discardableResult
    private func mk(
        _ k: Kernel, _ id: String, handler: String = "roster_probe.tools",
        meta: [(String, JSON)] = []
    ) async -> JSON {
        var p: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"), "parent_id": .string("kernel"),
            "handler_module": .string(handler), "id": .string(id),
        ]
        for (key, v) in meta { p[key] = v }
        let r = await k.send([Item(AgentId("kernel"), .object(p))])[0].result
        #expect(r["id"].asString == id, "fixture create failed: \(r.serialize())")
        return r
    }

    /// `$self` / `$parent` / `$children` restated, plus the named edges — an
    /// explicit grant REPLACES the baseline, it does not merge with it.
    private func grant(_ extra: [String: JSON]) -> JSON {
        var o: OrderedDictionary<String, JSON> = [
            "$self": .string("*"), "$parent": .string("*"), "$children": .string("*"),
        ]
        for (k, v) in extra { o[k] = v }
        return .object(o)
    }

    // ── shape ───────────────────────────────────────────────────

    @Test func distilledTreeCarriesPurposeNotWiring() async {
        let k = makeKernel()
        await mk(k, "worker", meta: [("description", .string("the prod deploy key"))])

        let r = await k.send([Item(AgentId("kernel"), .object(["type": .string("reflect")]))])[0]
            .result
        // The ROOT node: a bare agent answers its own identity.
        #expect(r["tree"]["id"].asString == "kernel_state")
        #expect(r["tree"]["sentence"].asString?.hasPrefix("Fantastic kernel") == true)
        #expect(r["tree"].asObject?["handler_module"] == nil, "the roster is not the wiring tree")
        #expect(r["tree"].asObject?["parent_id"] == nil)
        #expect(r["tree"].asObject?["display_name"] == nil)

        let node = r["tree"]["children"].asArray?.first { $0["id"].asString == "worker" }
        #expect(
            node?.asObject.map { Array($0.keys) } == [
                "id", "sentence", "description", "mode", "children",
            ])
        // The sentence came from the AGENT's own reflect, not from its record.
        #expect(node?["sentence"].asString == "worker is for testing.")
        // Operator-set purpose outranks the bundle sentence.
        #expect(node?["description"].asString == "the prod deploy key")
        // `mode` is a PURPOSE key; `verbs` is not, so it does not ride the roster.
        #expect(node?["mode"].asString == "sealed")
        #expect(node?.asObject?["verbs"] == nil)
    }

    @Test func detailFullKeepsTheWiringNode() async {
        let k = makeKernel()
        await mk(k, "worker")
        let r = await k.send([
            Item(
                AgentId("kernel"),
                .object(["type": .string("reflect"), "detail": .string("full")]))
        ])[0].result
        let node = r["tree"]["children"].asArray?.first { $0["id"].asString == "worker" }
        #expect(node?["handler_module"].asString == "roster_probe.tools")
        #expect(node?["display_name"].asString == "worker")
        #expect(node?.asObject?["sentence"] == nil, "full is wiring, not purpose")
    }

    // ── the gate ────────────────────────────────────────────────

    @Test func theRosterOmitsWhatTheCallerMayNotReflect() async {
        let k = makeKernel()
        await mk(k, "caller", meta: [("can_call", grant(["open": .array([.string("reflect")])]))])
        await mk(
            k, "open",
            meta: [("callable_by", grant(["caller": .array([.string("reflect")])]))])
        await mk(k, "secret", meta: [("description", .string("the thing nobody may read"))])

        let r = await k.sendAs(
            sender: AgentId("caller"), target: AgentId("kernel"),
            payload: .object(["type": .string("reflect")]))
        let kids = r["tree"]["children"].asArray ?? []
        #expect(kids.map { $0["id"].asString } == ["caller", "open", "secret"])

        let open = kids.first { $0["id"].asString == "open" }
        #expect(open?["sentence"].asString == "open is for testing.")

        // DENIED: the SHAPE is already public through `tree=ids`, so the id and
        // the children stay — and nothing the agent says about itself does.
        let secret = kids.first { $0["id"].asString == "secret" }
        #expect(secret?.asObject.map { Array($0.keys) } == ["id", "children"])
        #expect(secret?["children"].asArray?.isEmpty == true)

        // An OPERATOR (no sender) reads everything, as everywhere else.
        let asOperator = await k.send([
            Item(AgentId("kernel"), .object(["type": .string("reflect")]))
        ])[0].result
        let openly = asOperator["tree"]["children"].asArray?.first { $0["id"].asString == "secret" }
        #expect(openly?["description"].asString == "the thing nobody may read")
    }

    /// py `test_reflect_roster_gate.py::test_the_full_tree_applies_the_same_per_node_gate`
    /// — measured before the fix (audit 2026-10-03, KV-3): a depth-2 agent refused
    /// `reflect` on its sibling read that sibling's description and grants by
    /// reflecting its own parent with `detail:"full"`. A denied node keeps `id` +
    /// `children`.
    @Test func theFullTreeAppliesTheSamePerNodeGate() async {
        let k = makeKernel()
        await mk(k, "rgf_parent", meta: [("description", .string("the parent"))])
        await mk(
            k, "rgf_sibling",
            meta: [
                ("parent_id", .string("rgf_parent")),
                ("description", .string("the prod deploy key")),
                ("can_call", grant(["rgf_peer": .string("*")])),
            ])
        await mk(
            k, "rgf_kid",
            meta: [("parent_id", .string("rgf_sibling")), ("description", .string("deeper"))])
        await mk(k, "rgf_reader", meta: [("parent_id", .string("rgf_parent"))])
        let direct = await k.sendAs(
            sender: AgentId("rgf_reader"), target: AgentId("rgf_sibling"),
            payload: .object(["type": .string("reflect")]))
        #expect(direct["reason"].asString == "unauthorized")
        let full: JSON = .object(["type": .string("reflect"), "detail": .string("full")])
        let r = await k.sendAs(
            sender: AgentId("rgf_reader"), target: AgentId("rgf_parent"), payload: full)
        #expect(r["tree"]["description"].asString == "the parent")
        let kids = r["tree"]["children"].asArray ?? []
        let sib = kids.first { $0["id"].asString == "rgf_sibling" }
        #expect(sib?.asObject.map { Array($0.keys) } == ["id", "children"])
        let deeper = sib?["children"].asArray?.first
        #expect(deeper?["id"].asString == "rgf_kid")
        #expect(deeper?.asObject.map { Array($0.keys) } == ["id", "children"])
        let reader = kids.first { $0["id"].asString == "rgf_reader" }
        #expect(reader?["handler_module"].asString == "roster_probe.tools")
        // The operator sees everything, as everywhere else.
        let op = await k.send([Item(AgentId("rgf_parent"), full)])[0].result
        let opSib = op["tree"]["children"].asArray?.first { $0["id"].asString == "rgf_sibling" }
        #expect(opSib?["description"].asString == "the prod deploy key")
        #expect(opSib?["can_call"]["rgf_peer"].asString == "*")
    }

    // ── it is a REAL dispatch ───────────────────────────────────

    @Test func distilledTreeReflectEchoesToNodeWatchers() async {
        let k = makeKernel()
        await mk(k, "worker")
        await mk(k, "spy")
        _ = k.ensureInbox(AgentId("spy"))
        #expect(k.watch(src: AgentId("worker"), watcher: AgentId("spy")) == nil)

        let cap = EventCapture()
        _ = k.subscribe(cap.handler())
        _ = await k.send([Item(AgentId("kernel"), .object(["type": .string("reflect")]))])[0].result

        let reflects = cap.snapshot().filter {
            $0["kind"].asString == "send" && $0["payload"]["type"].asString == "reflect"
        }
        // The node's OWN traffic event, plus the mirror on its watcher — the
        // roster reflect runs through the kernel's fanning dispatch, never a
        // direct bundle call.
        #expect(reflects.contains { $0["agent_id"].asString == "worker" })
        #expect(reflects.contains { $0["agent_id"].asString == "spy" })
    }

    @Test func theRosterDispatchWritesAnInternalCallRowThatNeverSettles() async {
        let k = makeKernel()
        await mk(k, "worker")
        _ = await k.send([Item(AgentId("kernel"), .object(["type": .string("reflect")]))])[0].result

        let calls = k.rows.all().filter { $0["agent"]?.asString == "worker" }.filter { $0["kind"]?.asString == "call" }
        let internals = calls.filter { $0["internal"]?.asBool == true }
        #expect(internals.count == 1, "one internal reflect per roster node")
        let row = internals[0]
        #expect(row["verb"]?.asString == "reflect")
        // It opens NO ledger execution, so nothing stamps or settles it.
        #expect(row["outcome"] == nil, "the kernel's own read never settles")
        #expect(row["name"] == nil)
    }

    @Test func aDeniedNodeIsNotDispatchedAtAll() async {
        let k = makeKernel()
        await mk(k, "caller", meta: [("can_call", grant([:]))])
        await mk(k, "secret")

        _ = await k.sendAs(
            sender: AgentId("caller"), target: AgentId("kernel"),
            payload: .object(["type": .string("reflect")]))
        // Only its create-time `boot` row — no reflect call, internal or not.
        let rows = k.rows.all().filter { $0["agent"]?.asString == "secret" }
        #expect(
            !rows.contains { $0["verb"]?.asString == "reflect" },
            "a node the caller may not reflect is never called")
        #expect(rows.allSatisfy { $0["internal"] == nil })
    }

    // ── a node that RAISES fails the whole reflect ──────────────

    @Test func aRaisingNodeFailsTheWholeReflect() async {
        let k = makeKernel()
        await mk(k, "bomb", handler: "raising_probe.tools")
        let o = await k.send([Item(AgentId("kernel"), .object(["type": .string("reflect")]))])[0]
        #expect(o.status == .failed)
        #expect(o.result["raised"].asBool == true)
        #expect(o.result.asObject?["tree"] == nil, "a raised reflect is not a reflect")
    }
}
