// `currentOrigin` — the CALLER, not the callee.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

private final class Seen: @unchecked Sendable {
    private let lock = NSLock()
    private var _sender: String?
    private var _origin: String?
    private var _ran = false
    func record(sender: String?, origin: String?) {
        lock.lock(); defer { lock.unlock() }
        _sender = sender; _origin = origin; _ran = true
    }
    var value: (sender: String?, origin: String?, ran: Bool) {
        lock.lock(); defer { lock.unlock() }
        return (_sender, _origin, _ran)
    }
}

private struct SpyBundle: AgentBundle {
    let name = "origin_spy"
    let seen: Seen
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        seen.record(
            sender: KernelTaskLocals.currentSender?.value,
            origin: KernelTaskLocals.currentOrigin?.value)
        return .object(["ok": .bool(true)])
    }
}

@Suite("currentOrigin: a handler reads its CALLER")
struct CurrentOriginTests {
    @Test func aHandlerReadsItsCallerNotItself() async {
        // `currentSender` inside a handler is that handler's OWN agent — the
        // callee. A bundle whose payload NAMES a resource needs the CALLER,
        // which is what `currentOrigin` carries.
        let seen = Seen()
        let registry = BundleRegistry()
        registry.register("origin_spy.tools", SpyBundle(seen: seen))
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)

        // Two-sided deny-all: a sibling send needs BOTH grants.
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("kernel_state"),
                "handler_module": .string("origin_spy.tools"),
                "id": .string("spy"),
                "callable_by": .object(["caller": .string("*")]),
            ]))])[0].result
        _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"),
                "parent_id": .string("kernel_state"),
                "handler_module": .string("origin_spy.tools"),
                "id": .string("caller"),
                "can_call": .object(["spy": .string("*")]),
            ]))])[0].result

        // OPERATOR send: there is no caller, so there is no origin.
        _ = await kernel.send([Item(AgentId("spy"), .object(["type": .string("poke")]))])[0].result
        var v = seen.value
        #expect(v.ran)
        #expect(v.sender == "spy", "sender is the CALLEE")
        #expect(v.origin == nil, "an operator send has no origin")

        // AGENT send: origin names the CALLER; sender still names the callee.
        _ = await kernel.sendAs(
            sender: AgentId("caller"), target: AgentId("spy"),
            payload: .object(["type": .string("poke")]))
        v = seen.value
        #expect(v.sender == "spy")
        #expect(
            v.origin == "caller",
            "origin must be the CALLER — the whole reason the third local exists")
    }
}
