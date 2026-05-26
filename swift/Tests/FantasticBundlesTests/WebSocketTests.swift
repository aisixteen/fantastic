// 8C: WebSocket upgrade + frame protocol tests.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@testable import FantasticWeb

@Suite("WebSocket", .serialized)
struct WebSocketTests {
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

        // Receive reply.
        let msg = try await task.receive()
        let text: String
        switch msg {
        case .string(let s): text = s
        case .data(let d): text = String(data: d, encoding: .utf8) ?? ""
        @unknown default: text = ""
        }
        let parsed = try JSON.parse(text)
        #expect(parsed["type"].asString == "reply")
        #expect(parsed["id"].asString == "c1")
        let agents = parsed["data"]["agents"].asArray ?? []
        let ids = agents.compactMap { $0["id"].asString }
        #expect(ids.contains("core"))

        task.cancel(with: .normalClosure, reason: nil)
    }
}
