// Well-known aliases (C35) — a declared `alias` meta makes an agent reachable
// by a stable name through `send`, like the built-in `kernel`→root alias.
// Declared config, not automation: the operator sets `alias=…`, `Kernel.
// register` wires it into `well_known`, `send`/`create_agent parent_id`/the
// gate's placement operand resolve through it.
//
// Mirrors python `PT/kernel/test_alias.py`.

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

@Suite("C35: well-known aliases")
struct AliasTests {
    private func freshKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("noop.tools", Noop())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    @discardableResult
    private func create(_ kernel: Kernel, id: String, alias: String? = nil) async -> JSON {
        var payload: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "handler_module": .string("noop.tools"), "id": .string(id),
        ]
        if let alias { payload["alias"] = .string(alias) }
        return await kernel.send([Item(AgentId("kernel"), .object(payload))])[0].result
    }

    @Test func aliasResolvesToAgent() async {
        let kernel = freshKernel()
        _ = await create(kernel, id: "store_x", alias: "store")
        let r = await kernel.send([Item(AgentId("store"), .object(["type": .string("reflect")]))])[0]
            .result
        #expect(r["tree"]["id"].asString == "store_x", "\(r)")
    }

    @Test func literalIdUnaffectedByAliases() async {
        let kernel = freshKernel()
        _ = await create(kernel, id: "real", alias: "real_alias")
        let r = await kernel.send([Item(AgentId("real"), .object(["type": .string("reflect")]))])[0]
            .result
        #expect(r["tree"]["id"].asString == "real", "\(r)")
    }

    @Test func aliasClearedOnDelete() async {
        let kernel = freshKernel()
        _ = await create(kernel, id: "a1", alias: "myalias")
        let mid = await kernel.send([Item(AgentId("myalias"), .object(["type": .string("reflect")]))])[0]
            .result
        #expect(mid["tree"]["id"].asString == "a1", "\(mid)")
        let deleted = await kernel.send([
            Item(AgentId("kernel"), .object(["type": .string("delete_agent"), "id": .string("a1")]))
        ])[0].result
        #expect(deleted["deleted"].asBool == true, "\(deleted)")
        // A dangling alias errors cleanly (resolves to itself — no such agent).
        let after = await kernel.send([
            Item(AgentId("myalias"), .object(["type": .string("reflect")]))
        ])[0].result
        #expect(after["error"].asString != nil, "\(after)")
    }

    /// `create_agent parent_id` resolves the SAME way `send` does — including
    /// through a declared alias.
    @Test func createAgentParentIdResolvesThroughAlias() async {
        let kernel = freshKernel()
        _ = await create(kernel, id: "home", alias: "house")
        let child = await kernel.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("create_agent"), "parent_id": .string("house"),
                    "handler_module": .string("noop.tools"), "id": .string("kid"),
                ]))
        ])[0].result
        #expect(child["parent_id"].asString == "home", "\(child)")
    }
}
