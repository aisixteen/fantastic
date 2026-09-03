// `create_agent` is gated on the PLACEMENT TARGET, and `$loader` is real reach.
//
// Both measured 2026-09-02 against the live python kernel and ported here:
//
// - An agent holding the ordinary "may author tools" wiring
//   (`<root>:[create_agent]`) could plant a child under ANY agent, and the
//   child's stamped `$parent:*` baseline handed it unrestricted reach into that
//   agent. The gate asked about the ROOT (which merely ANSWERS the verb) instead
//   of the parent (which is ACTED UPON).
// - `checkConferral` skipped every `$`-prefixed key as "structural", but
//   `$loader` resolves to the ROOT for whoever holds it, so conferring it handed
//   over the kernel's control surface.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private struct Noop: AgentBundle {
    let name = "noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["ok": .bool(true)])
    }
}

@Suite("placement gate")
struct PlacementGateTests {

    private func freshKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("noop.tools", Noop())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    /// Create as the OPERATOR (ungated) — the fixture builder.
    @discardableResult
    private func seed(
        _ kernel: Kernel, _ id: String, parent: String,
        _ extra: OrderedDictionary<String, JSON> = [:]
    ) async -> JSON {
        var p: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"), "parent_id": .string(parent),
            "id": .string(id), "handler_module": .string("noop.tools"),
        ]
        for (k, v) in extra { p[k] = v }
        return await kernel.send([Item(AgentId("kernel"), .object(p))])[0].result
    }

    /// Create AS `sender` — the gated path under test.
    private func createAs(
        _ kernel: Kernel, _ sender: String, _ payload: OrderedDictionary<String, JSON>
    ) async -> JSON {
        var p: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"), "handler_module": .string("noop.tools"),
        ]
        for (k, v) in payload { p[k] = v }
        return await KernelTaskLocals.$currentSender.withValue(AgentId(sender)) {
            await kernel.send([Item(AgentId("kernel"), .object(p))])[0].result
        }
    }

    private let baseline: OrderedDictionary<String, JSON> = [
        "$self": .string("*"), "$parent": .string("*"), "$children": .string("*"),
    ]

    @Test func anAgentCreatesUnderItselfWithNoGrant() async {
        let kernel = freshKernel()
        await seed(kernel, "mid", parent: "core")
        await seed(kernel, "deep", parent: "mid")  // a GRANDCHILD
        let r = await createAs(
            kernel, "deep", ["parent_id": .string("deep"), "id": .string("kid")])
        #expect(r["id"].asString == "kid", "\(r.serialize())")
    }

    @Test func creatingUnderOwnDescendantIsBaseline() async {
        let kernel = freshKernel()
        await seed(kernel, "mid", parent: "core")
        await seed(kernel, "kid", parent: "mid")
        let r = await createAs(
            kernel, "mid", ["parent_id": .string("kid"), "id": .string("grandkid")])
        #expect(r["id"].asString == "grandkid", "\(r.serialize())")
    }

    @Test func aRootCreateGrantDoesNotReachASibling() async {
        // THE ESCALATION. `<root>:[create_agent]` is the ordinary authoring
        // grant; it must not be a key to every other agent's subtree.
        let kernel = freshKernel()
        await seed(kernel, "victim", parent: "core")
        var can = baseline
        can["core"] = .array([.string("*"), .string("create_agent"), .string("update_agent")])
        await seed(kernel, "author", parent: "core", ["can_call": .object(can)])
        let r = await createAs(
            kernel, "author", ["parent_id": .string("victim"), "id": .string("mole")])
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(r["side"].asString == "egress", "\(r.serialize())")
        #expect((r["error"].asString ?? "").contains("create under"), "\(r.serialize())")
        #expect(kernel.agent(AgentId("mole")) == nil)
    }

    @Test func creatingUnderAnUnrelatedAgentNeedsBothHalves() async {
        let kernel = freshKernel()
        await seed(kernel, "victim", parent: "core")
        var can = baseline
        can["victim"] = .array([.string("create_agent")])
        await seed(kernel, "author", parent: "core", ["can_call": .object(can)])
        // Egress granted, ingress not yet: the parent must accept a child.
        let r = await createAs(
            kernel, "author", ["parent_id": .string("victim"), "id": .string("a")])
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(r["side"].asString == "ingress", "\(r.serialize())")
        // Open the parent's ingress → the edge exists.
        var callable = baseline
        callable["author"] = .array([.string("create_agent")])
        _ = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("update_agent"), "id": .string("victim"),
                    "callable_by": .object(callable),
                ]))
        ])[0].result
        let ok = await createAs(
            kernel, "author", ["parent_id": .string("victim"), "id": .string("b")])
        #expect(ok["id"].asString == "b", "\(ok.serialize())")
    }

    @Test func loaderTokenInAConferredGrantIsBounded() async {
        let kernel = freshKernel()
        await seed(kernel, "mid", parent: "core")
        var can = baseline
        can["core"] = .array([.string("create_agent")])
        await seed(kernel, "deep", parent: "mid", ["can_call": .object(can)])
        // The granter itself cannot reach the root's task table…
        let denied = await KernelTaskLocals.$currentSender.withValue(AgentId("deep")) {
            await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[0]
                .result
        }
        #expect(denied["reason"].asString == "unauthorized", "\(denied.serialize())")
        // …so it cannot hand that reach to a child either.
        let r = await createAs(
            kernel, "deep",
            [
                "parent_id": .string("deep"), "id": .string("kid"),
                "can_call": .object(["$loader": .string("*")]),
            ])
        #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
        #expect(r["side"].asString == "delegation", "\(r.serialize())")
        #expect((r["error"].asString ?? "").contains("$loader"), "\(r.serialize())")
        #expect(kernel.agent(AgentId("kid")) == nil)
    }

    @Test func aGranterThatHoldsTheRootMayConferLoader() async {
        let kernel = freshKernel()
        var can = baseline
        can["core"] = .string("*")
        await seed(kernel, "holder", parent: "core", ["can_call": .object(can)])
        let r = await createAs(
            kernel, "holder",
            [
                "parent_id": .string("holder"), "id": .string("kid"),
                "can_call": .object(["$loader": .string("*")]),
            ])
        #expect(r["id"].asString == "kid", "\(r.serialize())")
    }

    @Test func anUnknownStructuralTokenIsRefused() async {
        let kernel = freshKernel()
        await seed(kernel, "a", parent: "core")
        let r = await createAs(
            kernel, "a",
            [
                "parent_id": .string("a"), "id": .string("kid"),
                "can_call": .object(["$everything": .string("*")]),
            ])
        #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
        #expect(r["side"].asString == "delegation", "\(r.serialize())")
        #expect((r["error"].asString ?? "").contains("$everything"), "\(r.serialize())")
    }
}
