// #1 / #7 / #22-#25 — the substrate contract, as a caller sees it.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

@Suite("Substrate contract: paging, readable authority, root-owned verbs")
struct ContractU1Tests {
    private func makeKernel() -> Kernel {
        let kernel = Kernel(storage: .inMemory, bundles: BundleRegistry())
        let root = Agent(id: "core", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    private func mk(_ k: Kernel, _ id: String, parent: String = "kernel", meta: [(String, JSON)] = [])
        async -> JSON
    {
        var p: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"),
            "parent_id": .string(parent),
            "handler_module": .string("noop.tools"),
            "id": .string(id),
        ]
        for (k2, v) in meta { p[k2] = v }
        return await k.send([Item(AgentId("kernel"), .object(p))])[0].result
    }

    @Test func aKernelVerbAtANonRootAgentSaysWhereToSendIt() async {
        // The verb acts on the TREE, so the agent named is the OPERAND, not the
        // actor. Answered everywhere, "send create_agent to X" read three ways
        // and a model spent 16 turns unable to resolve which.
        let k = makeKernel()
        _ = await mk(k, "victim")
        let r = await k.send([Item(AgentId("victim"), .object(["type": .string("list_agents")]))])[0].result
        #expect(r["reason"].asString == "not_here")
        let hint = r["hint"].asString ?? ""
        #expect(hint.contains("kernel"))
        #expect(hint.contains("OPERAND"))
    }

    @Test func createAgentDemandsAParentAndResolvesItLikeATarget() async {
        let k = makeKernel()

        // Absent ⇒ missing_arg, and the refusal says what to name.
        let missing = await k.send([Item(AgentId("kernel"), .object([
                "type": .string("create_agent"),
                "handler_module": .string("noop.tools"), "id": .string("x"),
            ]))])[0].result
        #expect(missing["reason"].asString == "missing_arg")
        #expect(missing["hint"].asString?.contains("subtree") == true)

        // Unknown ⇒ unknown_agent, carrying the live id index.
        let unknown = await mk(k, "x", parent: "nope")
        #expect(unknown["reason"].asString == "unknown_agent")
        #expect(unknown["agents"].asArray != nil)

        // Named ⇒ the new agent lands THERE, not under whoever was addressed.
        _ = await mk(k, "mid")
        let leaf = await mk(k, "leaf", parent: "mid")
        #expect(
            leaf["parent_id"].asString == "mid",
            "the field that expresses placement must actually place it")
    }

    @Test func anAgentCanReadItsOwnAuthority() async {
        // The gate used to be INVISIBLE: an agent could only discover its
        // authority by trying, which cost whole turns of empirical probing.
        let k = makeKernel()
        _ = await mk(
            k, "worker",
            meta: [
                ("can_call", .object(["store": .array([.string("read")])])),
                ("callable_by", .object(["cli": .string("*")])),
            ])

        let own = await k.send([Item(AgentId("worker"), .object(["type": .string("reflect")]))])[0].result
        #expect(own["can_call"]["store"][0].asString == "read")
        #expect(own["callable_by"]["cli"].asString == "*")

        // ...and the same two keys ride every distilled TREE node, so the whole
        // authorization landscape is readable from ONE root reflect.
        let tree = await k.send([Item(AgentId("kernel"), .object(["type": .string("reflect")]))])[0].result
        let node = tree["tree"]["children"].asArray?.first { $0["id"].asString == "worker" }
        #expect(node?["can_call"]["store"][0].asString == "read")
        #expect(node?["callable_by"]["cli"].asString == "*")
    }

    @Test func listAgentsIsPagedAndTheCursorWalksTheWholeSet() async {
        let k = makeKernel()
        for i in 0..<4 { _ = await mk(k, "kid_\(i)") }

        // Default is UNCHANGED — everything, no cursor.
        let all = await k.send([Item(AgentId("kernel"), .object(["type": .string("list_agents")]))])[0].result
        #expect(all["agents"].asArray?.count == 5)  // core + 4
        #expect(all["next"] == .null)

        // Bounded, and resuming visits each id exactly once.
        var seen: [String] = []
        var cursor: String? = nil
        while true {
            var p: OrderedDictionary<String, JSON> = [
                "type": .string("list_agents"), "limit": .integer(2),
            ]
            if let c = cursor { p["after"] = .string(c) }
            let r = await k.send([Item(AgentId("kernel"), .object(p))])[0].result
            seen.append(contentsOf: r["agents"].asArray?.compactMap { $0["id"].asString } ?? [])
            guard let n = r["next"].asString else { break }
            cursor = n
        }
        #expect(seen.count == 5)
        #expect(Set(seen).count == 5, "a paged walk must not repeat or skip")
    }

    @Test func treeIdsIsPagedToo() async {
        let k = makeKernel()
        for i in 0..<4 { _ = await mk(k, "t_\(i)") }
        let p = await k.send([Item(AgentId("kernel"), .object([
                "type": .string("reflect"), "tree": .string("ids"), "limit": .integer(2),
            ]))])[0].result
        #expect(p["tree"].asArray?.count == 2)
        #expect(p["tree_next"].asString != nil, "a further page must be named")
    }
}
