// `send` validates the ITEM before anything is resolved, gated or minted.
//
// Measured 2026-09-02 (python, ported): a payload without `type` reached the
// gate and the target instead of being a coded refusal; a non-string `type`
// raised out of `send`; rust/swift minted an id when the caller supplied `""`.

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

@Suite("send validation")
struct SendValidationTests {

    private func freshKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("noop.tools", Noop())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    @discardableResult
    private func create(_ kernel: Kernel, _ id: String) async -> JSON {
        await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("core"),
                    "id": .string(id), "handler_module": .string("noop.tools"),
                ]))
        ])[0].result
    }

    @Test func missingTypeIsMissingArgBeforeTheGate() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        await create(kernel, "b")  // no edge a→b: the gate WOULD refuse
        let out = await KernelTaskLocals.$currentSender.withValue(AgentId("a")) {
            await kernel.send([Item(AgentId("b"), .object([:]))])[0]
        }
        #expect(out.pid == nil, "\(out.result.serialize())")
        #expect(out.result["reason"].asString == "missing_arg", "\(out.result.serialize())")
        #expect(out.result["arg"].asString == "payload.type")
        #expect(out.result["side"] == .null, "validated BEFORE the gate — no gate side")
    }

    @Test func nonStringTypeIsBadArg() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        for bad: JSON in [.array([.string("x")]), .integer(7), .string(""), .null] {
            let out = await kernel.send([Item(AgentId("a"), .object(["type": bad]))])[0]
            #expect(out.result["reason"].asString == "bad_arg", "\(bad): \(out.result.serialize())")
            #expect(out.result["arg"].asString == "payload.type")
        }
    }

    @Test func emptyIdIsRefusedBadArg() async {
        let kernel = freshKernel()
        await create(kernel, "a")
        let item = Item(AgentId("a"), .object(["type": .string("poke")]), id: "")
        let out = await kernel.send([item])[0]
        #expect(out.pid == nil)
        #expect(out.result["reason"].asString == "bad_arg", "\(out.result.serialize())")
        #expect(out.result["arg"].asString == "id")
        // A colon id that names no execution is the minted namespace — refused too.
        let minted = Item(AgentId("a"), .object(["type": .string("poke")]), id: "a:deadbeef")
        let out2 = await kernel.send([minted])[0]
        #expect(out2.result["reason"].asString == "bad_arg", "\(out2.result.serialize())")
        #expect(out2.result["arg"].asString == "id")
    }

    @Test func createAgentWithEmptyIdIsRefused() async {
        let kernel = freshKernel()
        let before = kernel.allAgents().count
        let r = await create(kernel, "")
        #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
        #expect(r["arg"].asString == "id")
        #expect(kernel.allAgents().count == before, "no agent minted for \"\"")
    }
}
