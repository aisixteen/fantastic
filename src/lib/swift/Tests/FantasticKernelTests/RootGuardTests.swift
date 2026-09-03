// Guards on the ROOT and on the record fields that name substrate wiring.
//
// Measured 2026-09-02 (python-canonical, ported):
//
// - `delete_agent id=<root>` cascaded the whole tree and answered
//   `{"deleted": true}`, leaving a process with no control surface.
// - `update_agent` silently DROPPED `handler_module` and the reserved keys and
//   still answered success, so a caller believed a change had landed that never
//   did.
// - A verb an agent cannot answer came back as a reason-less `{error: …}` which
//   `applyReflectFlags` then decorated with `kernel_verbs`/`tree`/`more` — a
//   refusal wearing a reflect's clothes.

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

@Suite("root guards")
struct RootGuardTests {

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
    private func call(_ kernel: Kernel, _ target: String, _ payload: JSON) async -> JSON {
        await kernel.send([Item(AgentId(target), payload)])[0].result
    }

    @discardableResult
    private func create(
        _ kernel: Kernel, _ id: String, _ hm: String = "noop.tools",
        _ extra: OrderedDictionary<String, JSON> = [:]
    ) async -> JSON {
        var p: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"), "parent_id": .string("core"),
            "id": .string(id), "handler_module": .string(hm),
        ]
        for (k, v) in extra { p[k] = v }
        return await call(kernel, "kernel", .object(p))
    }

    @Test func deleteAgentRefusesTheRoot() async {
        let kernel = freshKernel()
        await create(kernel, "child")
        let r = await call(
            kernel, "kernel",
            .object(["type": .string("delete_agent"), "id": .string("core")]))
        #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
        #expect(r["arg"].asString == "id", "\(r.serialize())")
        #expect(r["deleted"].asBool == false, "\(r.serialize())")
        #expect(kernel.agent(AgentId("core")) != nil)
        #expect(kernel.agent(AgentId("child")) != nil)
    }

    @Test func updateAgentRefusesHandlerModuleAndReservedKeys() async {
        let kernel = freshKernel()
        await create(kernel, "upd")
        for (key, value) in [
            ("handler_module", JSON.string("other.tools")),
            ("root_path", .string("/tmp/elsewhere")),
            ("ctx", .string("x")),
            ("parent", .string("y")),
        ] {
            let r = await call(
                kernel, "kernel",
                .object([
                    "type": .string("update_agent"), "id": .string("upd"),
                    key: value,
                ]))
            #expect(r["reason"].asString == "bad_arg", "\(key): \(r.serialize())")
            #expect(r["arg"].asString == key, "\(key): \(r.serialize())")
        }
        #expect(kernel.agent(AgentId("upd"))?.handlerModule == "noop.tools")
    }

    @Test func createAgentRefusesReservedKeys() async {
        let kernel = freshKernel()
        let r = await create(
            kernel, "reserved", "noop.tools", ["root_path": .string("/tmp/elsewhere")])
        #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
        #expect(r["arg"].asString == "root_path", "\(r.serialize())")
        #expect(kernel.agent(AgentId("reserved")) == nil)
    }

    @Test func aVerbABareAgentCannotAnswerIsCoded() async {
        let kernel = freshKernel()
        let r = await call(kernel, "kernel", .object(["type": .string("no_such_verb_at_all")]))
        #expect(r["reason"].asString == "unhandled", "\(r.serialize())")
        #expect(r["verb"].asString == "no_such_verb_at_all", "\(r.serialize())")
    }

    @Test func aRefusalIsNotDecoratedIntoAReflect() async {
        // A record may name a bundle THIS runtime does not have (the browser's
        // own agents persist here). The call is refused with a code — and is not
        // dressed up with kernel_verbs/tree/more.
        let kernel = freshKernel()
        let rec = await create(kernel, "foreign", "frontend_view.tools")
        #expect(rec["id"].asString == "foreign", "\(rec.serialize())")
        let r = await call(kernel, "foreign", .object(["type": .string("reflect")]))
        #expect(r["reason"].asString == "unhandled", "\(r.serialize())")
        #expect(r["verb"].asString == "reflect", "\(r.serialize())")
        for decoration in ["kernel_verbs", "tree", "more"] {
            #expect(r[decoration] == .null, "\(decoration) in \(r.serialize())")
        }
    }
}
