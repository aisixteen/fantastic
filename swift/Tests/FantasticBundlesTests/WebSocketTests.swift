// 8C: WebSocket upgrade + frame protocol tests.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@testable import FantasticWeb

@Suite("WebSocket", .serialized)
struct WebSocketTests {
    /// WS is opt-in (parity with Python): the host serves `/<id>/ws`
    /// only when a `web_ws` child contributes the route. `web` already
    /// auto-booted via `startKernelInMemory`, so create the child then
    /// hot-`mount` it onto the live server.
    private func mountWebWS(_ kernel: Kernel) async {
        let rec = await kernel.send(
            AgentId("web"),
            .object([
                "type": .string("create_agent"),
                "handler_module": .string("web_ws.tools"),
                "id": .string("web_ws"),
            ]))
        let wsId = rec["id"].asString ?? "web_ws"
        _ = await kernel.send(
            AgentId("web"),
            .object(["type": .string("mount"), "child_id": .string(wsId)]))
    }

    @Test func computeAcceptMatchesRFC() {
        // RFC 6455 example: key "dGhlIHNhbXBsZSBub25jZQ==" → accept
        // "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        let accept = computeWebSocketAccept(key: "dGhlIHNhbXBsZSBub25jZQ==")
        #expect(accept == "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
    }

    @Test func decodeUnmaskedTextFrame() {
        // FIN=1 + opcode=1 (text), len=5, payload="Hello"
        let bytes: [UInt8] = [
            0x81, 0x05, 0x48, 0x65, 0x6C, 0x6C, 0x6F,
        ]
        let data = Data(bytes)
        let result = decodeFrame(data)
        #expect(result != nil)
        if let (frame, consumed) = result {
            #expect(frame.opcode == .text)
            #expect(String(data: frame.payload, encoding: .utf8) == "Hello")
            #expect(consumed == 7)
        }
    }

    @Test func decodeMaskedTextFrame() {
        // RFC 6455 example masked frame: FIN=1 + opcode=1, MASK=1
        // len=5, mask=0x37,0xfa,0x21,0x3d, masked "Hello"
        let bytes: [UInt8] = [
            0x81, 0x85,
            0x37, 0xFA, 0x21, 0x3D,
            // "Hello" XOR'd with the mask:
            0x48 ^ 0x37, 0x65 ^ 0xFA, 0x6C ^ 0x21, 0x6C ^ 0x3D, 0x6F ^ 0x37,
        ]
        let data = Data(bytes)
        let result = decodeFrame(data)
        #expect(result != nil)
        if let (frame, _) = result {
            #expect(frame.opcode == .text)
            #expect(String(data: frame.payload, encoding: .utf8) == "Hello")
        }
    }

    @Test func decodeReturnsNilForIncompleteFrame() {
        // Header only, no payload bytes.
        let data = Data([0x81, 0x05])
        #expect(decodeFrame(data) == nil)
    }

    @Test func wsCallRoundTripsThroughServer() async throws {
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send(
            AgentId("web"), .object(["type": .string("boot")]))
        await mountWebWS(kernel)
        defer {
            Task {
                _ = await kernel.send(
                    AgentId("web"), .object(["type": .string("shutdown")]))
            }
        }
        let port = kernel.httpPort()
        let url = URL(string: "ws://127.0.0.1:\(port)/core/ws")!
        let task = URLSession.shared.webSocketTask(with: url)
        task.resume()

        // Send a call frame asking for list_agents on core.
        let frame: JSON = .object([
            "type": .string("call"),
            "id": .string("c1"),
            "target": .string("core"),
            "payload": .object(["type": .string("list_agents")]),
        ])
        try await task.send(.string(frame.serialize()))

        // Read until the reply arrives. The connection auto-watches
        // `core`, so the call's own inbox fanout can surface an
        // `event` frame before the `reply` — a real client (browser
        // transport.js, the Swift bridge) correlates replies by `id`
        // and handles events separately, so filter by frame type.
        var reply: JSON? = nil
        for _ in 0..<5 {
            let msg = try await task.receive()
            let text: String
            switch msg {
            case .string(let s): text = s
            case .data(let d): text = String(data: d, encoding: .utf8) ?? ""
            @unknown default: continue
            }
            guard let parsed = try? JSON.parse(text) else { continue }
            if parsed["type"].asString == "reply", parsed["id"].asString == "c1" {
                reply = parsed
                break
            }
        }
        let parsed = try #require(reply)
        let agents = parsed["data"]["agents"].asArray ?? []
        let ids = agents.compactMap { $0["id"].asString }
        #expect(ids.contains("core"))

        task.cancel(with: .normalClosure, reason: nil)
    }

    @Test func wsExplicitWatchStreamsEvents() async throws {
        // Parity with Python's `_proxy._on_watch`: connect to one
        // agent's WS endpoint, then send `{type:"watch", src:<other>}`
        // and verify emits on <other> arrive as `event` frames. This
        // is the path kernel_bridge.watch_remote drives.
        let kernel = try await startKernelInMemory(portHint: 0)
        _ = await kernel.send(
            AgentId("web"), .object(["type": .string("boot")]))
        await mountWebWS(kernel)
        defer {
            Task {
                _ = await kernel.send(
                    AgentId("web"), .object(["type": .string("shutdown")]))
            }
        }
        let port = kernel.httpPort()
        // Connect on /core/ws but explicitly watch a DIFFERENT source
        // so we exercise the explicit-watch path, not the auto-watch.
        let url = URL(string: "ws://127.0.0.1:\(port)/core/ws")!
        let task = URLSession.shared.webSocketTask(with: url)
        task.resume()

        // Subscribe to `web`'s inbox explicitly.
        let watchFrame: JSON = .object([
            "type": .string("watch"),
            "src": .string("web"),
        ])
        try await task.send(.string(watchFrame.serialize()))

        // Give the server a tick to register the watch, then emit on
        // `web`.
        try await Task.sleep(nanoseconds: 100_000_000)
        await kernel.emit(
            AgentId("web"),
            .object(["type": .string("token"), "text": .string("hi")]))

        // Read frames until we see the token event (skip any other
        // event frames the auto-watch on core might surface).
        var sawToken = false
        for _ in 0..<5 {
            let msg = try await task.receive()
            let text: String
            switch msg {
            case .string(let s): text = s
            case .data(let d): text = String(data: d, encoding: .utf8) ?? ""
            @unknown default: continue
            }
            guard let parsed = try? JSON.parse(text) else { continue }
            if parsed["type"].asString == "event",
                parsed["payload"]["type"].asString == "token"
            {
                #expect(parsed["payload"]["text"].asString == "hi")
                sawToken = true
                break
            }
        }
        #expect(sawToken, "explicit watch did not stream the token event")

        task.cancel(with: .normalClosure, reason: nil)
    }
}
