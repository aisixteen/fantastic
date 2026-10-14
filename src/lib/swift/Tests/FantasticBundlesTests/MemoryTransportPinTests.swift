// The in-process `memory` bridge transport, pinned at its own surface.
//
// Python `MemoryTransport.pair()`: two halves, what one sends the other receives.
// It is one of the hand-written waiter tables (a parked `recv` per half, a FIFO
// of frames that arrived first, a close that releases the parked readers). Its
// promises: a frame that lands before `recv` waits is delivered, in arrival
// order; a parked `recv` wakes on the next frame; a frame sent before a close is
// still delivered; `close` on either half wakes a parked `recv` on both with the
// closed error, and every later `send`/`recv` fails the same way.
//
// It CARRIES frames and serves nothing: the far kernel answers a `call` through
// its own leg — `aServingLegGatesWhatCrossesTheMemoryPair`.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation
import Testing

private func call(_ id: String, _ n: Int) -> JSON {
    .object([
        "type": .string("call"), "id": .string(id), "target": .string("echo"),
        "payload": .object(["type": .string("ping"), "n": .integer(Int64(n))]),
    ])
}

private struct Echo: AgentBundle {
    let name = "echo"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["echoed": payload])
    }
}

/// A kernel (guid `guid`) carrying a ws_bridge bundle — handed back so a test
/// injects transports — and the `echo` bundle.
private func bridgeKernel(guid: String) -> (Kernel, KernelBridgeBundle) {
    let ws = KernelBridgeBundle(family: .ws)
    let r = BundleRegistry()
    r.register("ws_bridge.tools", ws)
    r.register("echo.tools", Echo())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
    k.register(root)
    k.setRoot(root)
    k.update(root.id, ["guid": .string(guid)])
    return (k, ws)
}

private func op(_ k: Kernel, _ target: String, _ payload: JSON) async -> JSON {
    await k.send([Item(AgentId(target), payload)])[0].result
}

@Suite("memory bridge transport")
struct MemoryTransportPinTests {

    @Test func framesThatArriveFirstAreDeliveredInOrder() async throws {
        let (a, b) = MemoryTransport.pair()
        try await a.send(call("c1", 1))
        try await a.send(call("c2", 2))
        let first = try await b.recv()
        let second = try await b.recv()
        #expect(first.frame == call("c1", 1))
        #expect(second.frame == call("c2", 2))
        #expect(!first.isBinary && first.body.isEmpty)
        await a.close()
    }

    @Test func aParkedRecvWakesOnTheNextFrame() async throws {
        let (a, b) = MemoryTransport.pair()
        let parked = Task { try await b.recv() }
        try await Task.sleep(nanoseconds: 30_000_000)
        try await a.send(call("c3", 3))
        let got = try await parked.value
        #expect(got.frame["id"] == "c3")
        await a.close()
    }

    @Test func aBinaryFrameCrossesAsBinary() async throws {
        let (a, b) = MemoryTransport.pair()
        let head: JSON = [
            "type": "reply", "id": "e1", "data": ["seq": 3, "chunk": .null],
            "_binary_path": ["data", "chunk"],
        ]
        try await a.sendBinary(header: head, body: Data([0x00, 0xFF, 0x0A]))
        let r = try await b.recv()
        #expect(r.isBinary)
        #expect(r.frame == head)
        #expect(r.body == Data([0x00, 0xFF, 0x0A]))
        await a.close()
    }

    @Test func closeWakesBothHalvesAndFailsEverythingAfter() async throws {
        let (a, b) = MemoryTransport.pair()
        func parkedRecv(_ t: MemoryTransport) -> Task<String, Never> {
            Task {
                do {
                    _ = try await t.recv()
                    return "returned a frame"
                } catch {
                    return "\(error)"
                }
            }
        }
        let onA = parkedRecv(a)
        let onB = parkedRecv(b)
        try await Task.sleep(nanoseconds: 30_000_000)
        await a.close()
        #expect(await onA.value == "MemoryTransport closed")
        #expect(await onB.value == "MemoryTransport closed")
        for t in [a, b] {
            #expect(await t.isClosed())
            await #expect(throws: (any Error).self) { try await t.send(call("c4", 4)) }
            await #expect(throws: (any Error).self) {
                try await t.sendBinary(header: call("c5", 5), body: Data([1]))
            }
            await #expect(throws: (any Error).self) { _ = try await t.recv() }
        }
    }

    @Test func aFrameSentBeforeTheCloseIsStillDelivered() async throws {
        let (a, b) = MemoryTransport.pair()
        try await a.send(call("c6", 6))
        await a.close()
        #expect(try await b.recv().frame["id"] == "c6")
        await #expect(throws: (any Error).self) { _ = try await b.recv() }
    }

    /// THE FAR LEG SERVES: the memory pair carries a forward to a
    /// SERVING leg on the far kernel, which gates it as python's
    /// `_serve_inbound_call` does — its ingress rule, then the leg's OWN send: a
    /// call a bridge leg serves is judged as THE LEG (its `can_call`, the target's
    /// `callable_by`), never as the far caller and never as the operator.
    @Test(.timeLimit(.minutes(1)))
    func aServingLegGatesWhatCrossesTheMemoryPair() async throws {
        let (ka, wsA) = bridgeKernel(guid: "guid-a")
        let (kb, wsB) = bridgeKernel(guid: "guid-b")
        let aLeg = await op(
            ka, "kernel",
            [
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "ws_bridge.tools", "id": "a_leg", "transport": "memory",
            ])["id"]
        #expect(aLeg == "a_leg")
        let bLeg = await op(
            kb, "kernel",
            [
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "ws_bridge.tools", "id": "b_leg", "transport": "memory",
                "can_call": ["$subtree": "*"],
            ])["id"]
        #expect(bLeg == "b_leg")
        let echo = await op(
            kb, "kernel",
            [
                "type": "create_agent", "parent_id": "b_leg", "handler_module": "echo.tools",
                "id": "echo",
            ])["id"]
        #expect(echo == "echo")
        let (ta, tb) = MemoryTransport.pair()
        wsA.injectTransport(agentId: AgentId("a_leg"), transport: ta)
        wsB.injectTransport(agentId: AgentId("b_leg"), transport: tb)
        for (k, id) in [(ka, "a_leg"), (kb, "b_leg")] {
            let r = await op(k, id, ["type": "boot"])
            #expect(r["booted"] == true, "\(r.serialize())")
        }
        func forward() async -> JSON {
            await op(
                ka, "a_leg", ["type": "forward", "target": "echo", "payload": ["type": "ping"]])
        }

        // 1. the serving leg is SEALED by default: nothing runs on kb
        let sealed = await forward()
        #expect(sealed["error"] == "edge sealed by policy", "\(sealed.serialize())")
        #expect(sealed["reason"] == "unauthorized")

        // 2. opened — the target judges THE LEG
        _ = await op(
            kb, "kernel",
            ["type": "update_agent", "id": "b_leg", "ingress_rule": "allow_all_ingress"])
        let refused = await forward()
        #expect(
            refused["error"] == "echo does not accept 'ping' from b_leg",
            "\(refused.serialize())")
        #expect(refused["reason"] == "unauthorized")

        // 3. `echo` accepting the leg: the call runs on kb
        _ = await op(
            kb, "kernel", ["type": "update_agent", "id": "echo", "callable_by": ["b_leg": "*"]])
        #expect(await forward() == ["echoed": ["type": "ping"]])

        // 4. the leg's own can_call bounds what its peer reaches
        _ = await op(kb, "kernel", ["type": "update_agent", "id": "b_leg", "can_call": ["$self": "*"]])
        let egress = await forward()
        #expect(egress["side"] == "egress", "\(egress.serialize())")
    }
}
