// The in-process `memory` bridge transport, pinned at its own surface.
//
// It is one of the hand-written waiter tables (a single parked `recv`, a FIFO
// of frames that arrived first, a close that releases the parked reader). Its
// promises: a frame that lands before `recv` waits is delivered, in arrival
// order; a parked `recv` wakes on the next frame; `close` wakes a parked `recv`
// with the closed error and every later `send`/`recv` fails the same way.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation
import Testing

private struct Echo: AgentBundle {
    let name = "echo"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        .object(["echoed": payload])
    }
}

private func remoteKernel() async -> Kernel {
    let r = BundleRegistry()
    r.register("echo.tools", Echo())
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
    k.register(root)
    k.setRoot(root)
    _ = await k.send([
        Item(
            "kernel_state",
            .object([
                "type": "create_agent", "parent_id": "kernel_state", "handler_module": "echo.tools",
                "id": "echo",
            ]))
    ])[0].result
    return k
}

private func call(_ id: String, _ n: Int) -> JSON {
    .object([
        "type": .string("call"), "id": .string(id), "target": .string("echo"),
        "payload": .object(["type": .string("ping"), "n": .integer(Int64(n))]),
    ])
}

@Suite("memory bridge transport")
struct MemoryTransportPinTests {

    @Test func framesThatArriveFirstAreDeliveredInOrder() async throws {
        let remote = await remoteKernel()
        let t = MemoryTransport(remote: remote)
        try await t.send(call("c1", 1))
        try await t.send(call("c2", 2))
        let a = try await t.recv()
        let b = try await t.recv()
        #expect(
            a.frame.serialize()
                == #"{"type":"reply","id":"c1","data":{"echoed":{"type":"ping","n":1}}}"#)
        #expect(
            b.frame.serialize()
                == #"{"type":"reply","id":"c2","data":{"echoed":{"type":"ping","n":2}}}"#)
        #expect(!a.isBinary && a.body.isEmpty)
        await t.close()
    }

    @Test func aParkedRecvWakesOnTheNextFrame() async throws {
        let remote = await remoteKernel()
        let t = MemoryTransport(remote: remote)
        let parked = Task { try await t.recv() }
        try await Task.sleep(nanoseconds: 30_000_000)
        try await t.send(call("c3", 3))
        let got = try await parked.value
        #expect(got.frame["id"] == "c3")
        await t.close()
    }

    @Test func closeWakesAParkedRecvAndFailsEverythingAfter() async throws {
        let remote = await remoteKernel()
        let t = MemoryTransport(remote: remote)
        let parked = Task { () -> String in
            do {
                _ = try await t.recv()
                return "returned a frame"
            } catch {
                return "\(error)"
            }
        }
        try await Task.sleep(nanoseconds: 30_000_000)
        await t.close()
        #expect(await parked.value == "memory transport closed")
        #expect(await t.isClosed())
        await #expect(throws: (any Error).self) { try await t.send(call("c4", 4)) }
        await #expect(throws: (any Error).self) {
            try await t.sendBinary(header: call("c5", 5), body: Data([1]))
        }
        await #expect(throws: (any Error).self) { _ = try await t.recv() }
    }

    /// A watched agent's events cross as `event` frames naming their `src`;
    /// after `unwatch` nothing more crosses.
    @Test func watchRelaysEventsUntilUnwatched() async throws {
        let remote = await remoteKernel()
        let t = MemoryTransport(remote: remote)
        try await t.send(.object(["type": .string("watch"), "src": .string("echo")]))
        await remote.emit(AgentId("echo"), .object(["type": .string("tick"), "n": .integer(1)]))
        let ev = try await t.recv()
        #expect(
            ev.frame.serialize()
                == #"{"type":"event","src":"echo","payload":{"type":"tick","n":1}}"#)
        try await t.send(.object(["type": .string("unwatch"), "src": .string("echo")]))
        await remote.emit(AgentId("echo"), .object(["type": .string("tick"), "n": .integer(2)]))
        try await t.send(call("c6", 6))
        let next = try await t.recv()
        #expect(next.frame["id"] == "c6", "an unwatched event crossed: \(next.frame.serialize())")
        await t.close()
    }
}
