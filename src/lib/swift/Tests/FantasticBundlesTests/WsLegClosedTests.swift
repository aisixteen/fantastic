// A ws leg reports its transport CLOSED once the remote drops (audit W-7,
// 2026-10-03).
//
// `WSTransport.isClosed()` answered `false` always (a copy of python's
// `getattr(self._ws, "closed", False)`, which websockets 16 never sets). After
// the remote closed, the read loop ended (`bridge_down`) while `reflect` kept
// saying `connected:true` and `boot` answered `{already:true}` — only
// `reconnect` recovered the leg. It now follows the socket.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import Foundation
import Testing

/// A socket that stays silent until the remote closes it.
private actor DroppingWire: RelayWire {
    private var waiters: [CheckedContinuation<NIOWebSocketClient.Message, Error>] = []
    private var down = false

    func send(_ message: NIOWebSocketClient.Message) async throws {
        if down { throw NIOWebSocketClient.Closed(reason: "websocket: connection closed") }
    }

    func receive() async throws -> NIOWebSocketClient.Message {
        if down { throw NIOWebSocketClient.Closed(reason: "websocket: peer closed") }
        return try await withCheckedThrowingContinuation { waiters.append($0) }
    }

    /// The remote closes the socket.
    func remoteCloses() {
        down = true
        let w = waiters
        waiters.removeAll()
        for c in w {
            c.resume(throwing: NIOWebSocketClient.Closed(reason: "websocket: peer closed"))
        }
    }

    func close() async { remoteCloses() }
}

@Suite("ws_bridge — a dropped socket is a closed leg")
struct WsLegClosedTests {

    @Test func closedFollowsTheSocket() async {
        let wire = DroppingWire()
        let t = WSTransport(client: wire, sealed: false)
        #expect(await t.isClosed() == false)
        let reader = Task { () -> String in
            do {
                _ = try await t.recv()
                return "returned a frame"
            } catch {
                return "\(error)"
            }
        }
        await wire.remoteCloses()
        _ = await reader.value
        #expect(await t.isClosed())
    }

    @Test(.timeLimit(.minutes(1)))
    func aDroppedLegReflectsDownAndBootDialsAgain() async throws {
        let registry = BundleRegistry()
        let bridge = KernelBridgeBundle(family: .ws)
        registry.register("ws_bridge.tools", bridge)
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        let made = await kernel.send([
            Item(
                AgentId("kernel"),
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "ws_bridge.tools", "id": "w_leg", "transport": "memory",
                ])
        ])[0].result
        #expect(made["id"] == "w_leg", "\(made.serialize())")
        func call(_ payload: JSON) async -> JSON {
            await kernel.send([Item(AgentId("w_leg"), payload)])[0].result
        }

        let first = DroppingWire()
        bridge.injectTransport(agentId: AgentId("w_leg"), transport: WSTransport(client: first, sealed: false))
        #expect(await call(["type": "boot"]) == ["booted": true, "transport": "memory"])
        #expect(await call(["type": "boot"]) == ["already": true, "transport": "memory"])
        #expect(await call(["type": "reflect"])["connected"] == true)

        await first.remoteCloses()
        let deadline = Date().addingTimeInterval(3)
        while await call(["type": "reflect"])["connected"] != false {
            if Date() >= deadline {
                Issue.record("reflect kept saying connected after the remote closed")
                return
            }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        let fwd = await call(["type": "forward", "target": "x", "payload": ["type": "reflect"]])
        #expect(fwd["reason"] == "not_connected", "\(fwd.serialize())")

        // boot DIALS AGAIN — no `{already:true}` for a dead socket.
        let second = DroppingWire()
        bridge.injectTransport(agentId: AgentId("w_leg"), transport: WSTransport(client: second, sealed: false))
        #expect(await call(["type": "boot"]) == ["booted": true, "transport": "memory"])
        #expect(await call(["type": "reflect"])["connected"] == true)

        _ = await kernel.send([
            Item(AgentId("kernel_state"), ["type": "delete_agent", "id": "w_leg"])
        ])[0].result
    }
}
