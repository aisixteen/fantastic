// WebSocket upgrade + frame routing.
//
// Mirrors Rust's `fantastic-web-ws`. Hand-rolled over NWConnection
// to avoid pulling in a separate WS package — we only need text
// frames, ping/pong, and close. RFC 6455 minimal subset.
//
// Frame protocol (matches python/bundled_agents/web/host/_proxy.py —
// the reference template):
//   client → server : {"type":"call",  "target":"<id>", "payload":{...}, "id":"<id>"}
//   server → client : {"type":"reply", "id":"<id>", "data":{...}}
//   server → client : {"type":"event", "payload":{...}}  (watcher fanout)

import CryptoKit
import FantasticJSON
import FantasticKernel
import Foundation
import Network

#if canImport(Darwin)
    import Darwin
#endif

/// Magic GUID from RFC 6455 used in the handshake.
private let WS_MAGIC = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

/// Compute Sec-WebSocket-Accept from the client's key.
public func computeWebSocketAccept(key: String) -> String {
    let concatenated = key + WS_MAGIC
    let digest = Insecure.SHA1.hash(data: concatenated.data(using: .utf8) ?? Data())
    return Data(digest).base64EncodedString()
}

/// Install the WebSocket upgrade handler on `server`. After this,
/// requests targeting `/<agent_id>/ws` with `Upgrade: websocket`
/// will be promoted to bidirectional JSON-frame channels routed
/// through `kernel`.
public func installWebSocketUpgrade(on server: WebServer, kernel: Kernel) {
    server.webSocketUpgrade = { agentSegment, connection, request in
        Task {
            await handleUpgrade(
                agentSegment: agentSegment,
                connection: connection,
                request: request,
                kernel: kernel
            )
        }
    }
}

private func handleUpgrade(
    agentSegment: String,
    connection: NWConnection,
    request: HTTPRequest,
    kernel: Kernel
) async {
    guard let key = request.headers["Sec-WebSocket-Key"] else {
        await sendError(connection: connection, message: "Missing Sec-WebSocket-Key")
        return
    }
    let accept = computeWebSocketAccept(key: key)
    let handshake = """
        HTTP/1.1 101 Switching Protocols\r
        Upgrade: websocket\r
        Connection: Upgrade\r
        Sec-WebSocket-Accept: \(accept)\r
        \r

        """
    let agentId = AgentId(agentSegment)
    let send: @Sendable (Data) -> Void = { data in
        connection.send(content: data, completion: .contentProcessed { _ in })
    }
    send(handshake.data(using: .utf8) ?? Data())

    // Auto-watch: WS clients subscribe to the agent's inbox so token
    // streams + state events flow without a separate subscribe verb.
    if kernel.agent(agentId) != nil {
        let watcherId = AgentId("ws_client_\(UUID().uuidString.prefix(8))")
        kernel.watch(src: agentId, watcher: watcherId)
        Task {
            for await event in kernel.ensureInbox(watcherId) {
                // Frame matches Python reference: {type:"event", payload}.
                // No `agent` field — emitter identity is whatever the
                // payload carries (typically via sender attribution).
                let frame: JSON = .object([
                    "type": .string("event"),
                    "payload": event,
                ])
                sendTextFrame(connection: connection, text: frame.serialize())
            }
        }
    }

    await readLoop(connection: connection, agentId: agentId, kernel: kernel)
}

private func readLoop(
    connection: NWConnection,
    agentId: AgentId,
    kernel: Kernel,
    accumulated: Data = Data()
) async {
    let (data, isComplete, _) = await receiveBytes(connection: connection)
    var buffer = accumulated
    if let data = data, !data.isEmpty {
        buffer.append(data)
    }
    // Parse as many complete frames as we have.
    while let (frame, consumed) = decodeFrame(buffer) {
        buffer.removeSubrange(0..<consumed)
        await handleFrame(frame: frame, connection: connection, agentId: agentId, kernel: kernel)
        if frame.opcode == .close {
            connection.cancel()
            return
        }
    }
    if isComplete {
        connection.cancel()
        return
    }
    await readLoop(connection: connection, agentId: agentId, kernel: kernel, accumulated: buffer)
}

private func receiveBytes(connection: NWConnection) async -> (Data?, Bool, Error?) {
    await withCheckedContinuation { cont in
        connection.receive(minimumIncompleteLength: 1, maximumLength: 16384) {
            data, _, isComplete, error in
            cont.resume(returning: (data, isComplete, error))
        }
    }
}

private func handleFrame(
    frame: WebSocketFrame,
    connection: NWConnection,
    agentId: AgentId,
    kernel: Kernel
) async {
    switch frame.opcode {
    case .text:
        guard let text = String(data: frame.payload, encoding: .utf8),
            let parsed = try? JSON.parse(text)
        else { return }
        if parsed["type"].asString == "call" {
            await handleCall(parsed: parsed, connection: connection, agentId: agentId, kernel: kernel)
        }
    case .ping:
        sendFrame(connection: connection, opcode: .pong, payload: frame.payload)
    case .pong:
        break  // ignore
    case .close:
        sendFrame(connection: connection, opcode: .close, payload: Data())
    case .binary, .continuation:
        break  // not handled in 8C
    }
}

private func handleCall(
    parsed: JSON,
    connection: NWConnection,
    agentId: AgentId,
    kernel: Kernel
) async {
    let target = parsed["target"].asString ?? agentId.value
    let payload = parsed["payload"]
    let id = parsed["id"].asString ?? ""
    let reply = await kernel.send(AgentId(target), payload)
    // Field name MUST be `data` — transport.js's reply handler reads
    // `msg.data` (see fantastic-web/Resources/transport.js:193). Rust
    // kernel used `data`; this Swift port had drifted to `result` which
    // caused every browser-side `t.call(...)` to resolve to `undefined`.
    let frame: JSON = .object([
        "type": .string("reply"),
        "id": .string(id),
        "data": reply,
    ])
    sendTextFrame(connection: connection, text: frame.serialize())
}

// MARK: - Frame encode / decode

enum WSOpcode: UInt8 {
    case continuation = 0x0
    case text = 0x1
    case binary = 0x2
    case close = 0x8
    case ping = 0x9
    case pong = 0xA
}

struct WebSocketFrame {
    let opcode: WSOpcode
    let payload: Data
}

/// Decode one frame from `data` if a complete one is available.
/// Returns the frame + number of bytes consumed. Returns nil if
/// the buffer doesn't yet contain a full frame.
func decodeFrame(_ data: Data) -> (WebSocketFrame, Int)? {
    guard data.count >= 2 else { return nil }
    let byte0 = data[data.startIndex]
    let byte1 = data[data.startIndex + 1]
    // FIN bit is byte0 & 0x80; we treat fragmented frames as
    // unsupported (text frames in the brain-kernel use case are
    // small enough to never fragment).
    let opcodeRaw = byte0 & 0x0F
    guard let opcode = WSOpcode(rawValue: opcodeRaw) else { return nil }
    let masked = (byte1 & 0x80) != 0
    var payloadLen = Int(byte1 & 0x7F)
    var cursor = data.startIndex + 2
    if payloadLen == 126 {
        guard data.count >= cursor + 2 else { return nil }
        payloadLen = (Int(data[cursor]) << 8) | Int(data[cursor + 1])
        cursor += 2
    } else if payloadLen == 127 {
        guard data.count >= cursor + 8 else { return nil }
        var len: UInt64 = 0
        for i in 0..<8 {
            len = (len << 8) | UInt64(data[cursor + i])
        }
        payloadLen = Int(len)
        cursor += 8
    }
    var maskKey: [UInt8] = []
    if masked {
        guard data.count >= cursor + 4 else { return nil }
        maskKey = Array(data[cursor..<(cursor + 4)])
        cursor += 4
    }
    guard data.count >= cursor + payloadLen else { return nil }
    var payload = Data(data[cursor..<(cursor + payloadLen)])
    if masked && !maskKey.isEmpty {
        for i in 0..<payload.count {
            payload[i] ^= maskKey[i % 4]
        }
    }
    let consumed = (cursor + payloadLen) - data.startIndex
    return (WebSocketFrame(opcode: opcode, payload: payload), consumed)
}

private func sendTextFrame(connection: NWConnection, text: String) {
    sendFrame(
        connection: connection,
        opcode: .text,
        payload: text.data(using: .utf8) ?? Data())
}

private func sendFrame(connection: NWConnection, opcode: WSOpcode, payload: Data) {
    var frame = Data()
    frame.append(0x80 | opcode.rawValue)  // FIN + opcode
    let len = payload.count
    if len < 126 {
        frame.append(UInt8(len))
    } else if len <= 0xFFFF {
        frame.append(126)
        frame.append(UInt8((len >> 8) & 0xFF))
        frame.append(UInt8(len & 0xFF))
    } else {
        frame.append(127)
        let len64 = UInt64(len)
        for i in (0..<8).reversed() {
            frame.append(UInt8((len64 >> (i * 8)) & 0xFF))
        }
    }
    frame.append(payload)
    connection.send(content: frame, completion: .contentProcessed { _ in })
}

private func sendError(connection: NWConnection, message: String) async {
    let response = """
        HTTP/1.1 400 Bad Request\r
        Content-Type: text/plain\r
        Content-Length: \(message.utf8.count)\r
        Connection: close\r
        \r
        \(message)
        """
    connection.send(
        content: response.data(using: .utf8),
        completion: .contentProcessed { _ in connection.cancel() })
}
