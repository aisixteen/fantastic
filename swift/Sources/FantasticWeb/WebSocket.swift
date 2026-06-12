// WebSocket upgrade + frame routing.
//
// Mirrors Rust's `fantastic-web-ws`. Hand-rolled over NWConnection
// to avoid pulling in a separate WS package — we only need text
// frames, ping/pong, and close. RFC 6455 minimal subset.
//
// Frame protocol (matches python/bundled_agents/web/host/_proxy.py —
// the reference template):
//   client → server : {"type":"call",   "target":"<id>", "payload":{...}, "id":"<id>"}
//   client → server : {"type":"emit",   "target":"<id>", "payload":{...}}
//   client → server : {"type":"watch",  "src":"<id>"}
//   client → server : {"type":"unwatch","src":"<id>"}
//   server → client : {"type":"reply", "id":"<id>", "data":{...}}
//   server → client : {"type":"event", "payload":{...}}  (watcher fanout)
//
// Every connection auto-watches the URL-path agent (`agentSegment`)
// so a browser sees its inbox events without an explicit watch —
// AND honors explicit `watch`/`unwatch` frames that add/remove other
// sources onto the SAME client inbox (parity with Python's
// `_proxy.run`: `kernel.watch(host_agent_id, client_id)` up front +
// `_on_watch` for additional srcs). The kernel_bridge's
// `watch_remote` relies on the explicit-watch path.

import CryptoKit
import FantasticIoBridge
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

/// Run the shared WebSocket proxy for an upgrade request. The host's
/// route matcher calls this when a `web_ws`-contributed
/// `/{host_id}/ws` route matches — `hostId` is the captured path
/// segment (the agent whose inbox the connection auto-watches).
/// Promotes the connection to a bidirectional JSON-frame channel
/// routed through `kernel`. This is the shared machinery `web_ws`
/// reuses (analog of Python's `web/_proxy.run`) — the WS logic lives
/// in the host module, the `web_ws` bundle only declares the route.
public func runWebSocketProxy(
    hostId: String,
    legId: AgentId,
    connection: NWConnection,
    request: HTTPRequest,
    kernel: Kernel
) {
    Task {
        await handleUpgrade(
            agentSegment: hostId,
            legId: legId,
            connection: connection,
            request: request,
            kernel: kernel
        )
    }
}

private func handleUpgrade(
    agentSegment: String,
    legId: AgentId,
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

    // One synthetic client id per connection. The URL-path agent is
    // auto-watched onto it; explicit `watch` frames add more sources
    // onto the SAME inbox. A single drain task pumps that inbox out
    // as `{type:"event", payload}` frames. Mirrors Python's
    // `_proxy.run`: `client_id` + `watching` set + `drain_outbound`.
    let clientId = AgentId("ws_client_\(UUID().uuidString.prefix(8))")
    let state = WSClientState(clientId: clientId, legId: legId)
    let inbox = kernel.ensureInbox(clientId)
    if kernel.agent(agentId) != nil {
        kernel.watch(src: agentId, watcher: clientId)
        state.watching.insert(agentId)
    }
    Task {
        for await event in inbox {
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

    await readLoop(connection: connection, agentId: agentId, kernel: kernel, state: state)
}

/// Per-connection mutable state. The `watching` set is mutated only
/// from the read loop (frames are handled sequentially), so no extra
/// synchronization is needed; the drain task reads only the immutable
/// `clientId`.
final class WSClientState: @unchecked Sendable {
    let clientId: AgentId
    /// The web_ws LEG that contributed this `/ws` route — its `ingress_rule`
    /// gates every inbound call (sealed-by-default).
    let legId: AgentId
    var watching: Set<AgentId> = []
    init(clientId: AgentId, legId: AgentId) {
        self.clientId = clientId
        self.legId = legId
    }
}

private func readLoop(
    connection: NWConnection,
    agentId: AgentId,
    kernel: Kernel,
    state: WSClientState,
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
        await handleFrame(
            frame: frame, connection: connection, agentId: agentId,
            kernel: kernel, state: state)
        if frame.opcode == .close {
            connection.cancel()
            return
        }
    }
    if isComplete {
        connection.cancel()
        return
    }
    await readLoop(
        connection: connection, agentId: agentId, kernel: kernel,
        state: state, accumulated: buffer)
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
    kernel: Kernel,
    state: WSClientState
) async {
    switch frame.opcode {
    case .text:
        guard let text = String(data: frame.payload, encoding: .utf8),
            let parsed = try? JSON.parse(text)
        else { return }
        switch parsed["type"].asString {
        case "call":
            await handleCall(
                parsed: parsed, connection: connection, agentId: agentId, kernel: kernel,
                legId: state.legId)
        case "emit":
            // Mirror of Python's `_on_emit`: fire-and-forget into a
            // target's inbox, no reply.
            let target = parsed["target"].asString ?? agentId.value
            await kernel.emit(AgentId(target), parsed["payload"])
        case "watch":
            // Mirror of Python's `_on_watch`: add `src` onto this
            // client's inbox so its events stream back as `event`
            // frames. Idempotent via the `watching` set.
            if let src = parsed["src"].asString {
                let srcId = AgentId(src)
                if !state.watching.contains(srcId) {
                    kernel.watch(src: srcId, watcher: state.clientId)
                    state.watching.insert(srcId)
                }
            }
        case "unwatch":
            // Mirror of Python's `_on_unwatch`.
            if let src = parsed["src"].asString {
                let srcId = AgentId(src)
                if state.watching.contains(srcId) {
                    kernel.unwatch(src: srcId, watcher: state.clientId)
                    state.watching.remove(srcId)
                }
            }
        default:
            break  // unknown frame type — ignore (weak)
        }
    case .ping:
        sendFrame(connection: connection, opcode: .pong, payload: frame.payload)
    case .pong:
        break  // ignore
    case .close:
        sendFrame(connection: connection, opcode: .close, payload: Data())
    case .binary:
        // A binary WS message is the `[4B len|header|body]` codec frame — a
        // read_stream/write_stream chunk carrying RAW BYTES (the JS client pipes
        // the host's stream this way). Dispatch on the symmetric binary channel.
        await handleBinaryFrame(
            payload: frame.payload, connection: connection, agentId: agentId,
            kernel: kernel, legId: state.legId)
    case .continuation:
        break  // not handled
    }
}

private func handleBinaryFrame(
    payload: Data, connection: NWConnection, agentId: AgentId, kernel: Kernel, legId: AgentId
) async {
    guard let (header, blob) = Codec.decodeBinaryFrame(payload) else { return }
    let target = header["target"].asString ?? agentId.value
    let id = header["id"].asString ?? ""
    // GATE — same sealed-by-default web-leg choke point as a text call.
    if let denied = gateWebLeg(
        kernel: kernel, legId: legId, target: target,
        verb: header["type"].asString ?? "", token: header["auth_token"].asString)
    {
        let frame: JSON = .object([
            "type": .string("reply"), "id": .string(id), "data": denied,
        ])
        sendTextFrame(connection: connection, text: frame.serialize())
        return
    }
    let (reply, body) = await kernel.sendWithBinary(AgentId(target), header, blob)
    if body.isEmpty {
        // No reply bytes (e.g. write_stream status) → plain text reply frame.
        let frame: JSON = .object([
            "type": .string("reply"), "id": .string(id), "data": reply,
        ])
        sendTextFrame(connection: connection, text: frame.serialize())
    } else {
        // read_stream reply → a binary frame, body raw at `data.bytes`.
        var env: JSON = .object([
            "type": .string("reply"), "id": .string(id), "data": reply,
        ])
        env["_binary_path"] = .string("data.bytes")
        sendFrame(
            connection: connection, opcode: .binary,
            payload: Codec.encodeBinaryFrame(header: env, body: body))
    }
}

private func handleCall(
    parsed: JSON,
    connection: NWConnection,
    agentId: AgentId,
    kernel: Kernel,
    legId: AgentId
) async {
    let target = parsed["target"].asString ?? agentId.value
    let payload = parsed["payload"]
    let id = parsed["id"].asString ?? ""
    // GATE — web_ws is an io_bridge inbound (ws) derivation: SEALED by default.
    // Gate the inbound frame at the shared choke point with THIS leg's
    // ingress_rule (credential rides the frame envelope `auth_token`).
    if let denied = gateWebLeg(
        kernel: kernel, legId: legId, target: target,
        verb: payload["type"].asString ?? "", token: parsed["auth_token"].asString)
    {
        let frame: JSON = .object([
            "type": .string("reply"), "id": .string(id), "data": denied,
        ])
        sendTextFrame(connection: connection, text: frame.serialize())
        return
    }
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
