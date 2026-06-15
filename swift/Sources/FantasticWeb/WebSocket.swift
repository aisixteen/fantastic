// WebSocket proxy handler (swift-nio).
//
// Replaces the former hand-rolled RFC-6455 framing over NWConnection.
// NIO's WebSocket upgrader (installed in HTTPServer.swift's pipeline)
// owns the handshake (Sec-WebSocket-Accept) + frame codec; this handler
// only carries the PROXY semantics — relaying JSON/binary frames to the
// kernel and streaming watched events back. Cross-platform (Linux too).
//
// Frame protocol (matches python/bundled_agents/web/host/_proxy.py):
//   client → server : {"type":"call",   "target":"<id>", "payload":{...}, "id":"<id>"}
//   client → server : {"type":"emit",   "target":"<id>", "payload":{...}}
//   client → server : {"type":"watch",  "src":"<id>"}
//   client → server : {"type":"unwatch","src":"<id>"}
//   server → client : {"type":"reply",  "id":"<id>", "data":{...}}
//   server → client : {"type":"event",  "payload":{...}}   (watcher fanout)
//
// Each connection auto-watches the URL-path agent (`hostId`) onto a
// synthetic `ws_client_<hex>` inbox, and honors explicit watch/unwatch
// onto the SAME inbox (parity with Python's `_proxy.run`). A single
// drain task pumps that inbox out as `event` frames.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import NIOFoundationCompat
import NIOWebSocket

/// Shared WebSocket proxy — installed by the host's pipeline after a
/// `/{host_id}/ws` upgrade. `hostId` is the auto-watched agent; `legId`
/// is the `web_ws` leg whose `ingress_rule` gates every inbound call.
final class WebSocketProxyHandler: ChannelInboundHandler, @unchecked Sendable {
    typealias InboundIn = WebSocketFrame
    typealias OutboundOut = WebSocketFrame

    private let hostId: String
    private let legId: AgentId
    private let kernel: Kernel
    /// One synthetic client id per connection (its inbox is the fanout point).
    private let clientId: AgentId
    /// Sources watched onto this client's inbox. Mutated ONLY on the event
    /// loop (handlerAdded / channelRead / teardown all run there).
    private var watching: Set<AgentId> = []
    private var drainTask: Task<Void, Never>?

    init(hostId: String, legId: AgentId, kernel: Kernel) {
        self.hostId = hostId
        self.legId = legId
        self.kernel = kernel
        self.clientId = AgentId("ws_client_\(UUID().uuidString.prefix(8))")
    }

    func handlerAdded(context: ChannelHandlerContext) {
        let channel = context.channel
        let hostAgent = AgentId(hostId)
        let inbox = kernel.ensureInbox(clientId)
        // Auto-watch the URL-path agent (if it exists) onto this client.
        if kernel.agent(hostAgent) != nil {
            kernel.watch(src: hostAgent, watcher: clientId)
            watching.insert(hostAgent)
        }
        // Drain the inbox → `{type:"event", payload}` text frames. Runs off
        // the loop; touches only the immutable clientId + the Sendable channel.
        drainTask = Task {
            for await event in inbox {
                let frame: JSON = .object(["type": .string("event"), "payload": event])
                Self.writeText(channel: channel, text: frame.serialize())
            }
        }
    }

    func channelInactive(context: ChannelHandlerContext) {
        teardown()
        context.fireChannelInactive()
    }

    func handlerRemoved(context: ChannelHandlerContext) {
        teardown()
    }

    private func teardown() {
        drainTask?.cancel()
        drainTask = nil
        for src in watching {
            kernel.unwatch(src: src, watcher: clientId)
        }
        watching.removeAll()
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let frame = unwrapInboundIn(data)
        let channel = context.channel
        switch frame.opcode {
        case .text:
            handleText(String(buffer: frame.unmaskedData), channel: channel)
        case .binary:
            var payload = frame.unmaskedData
            let bytes = payload.readData(length: payload.readableBytes) ?? Data()
            handleBinary(bytes, channel: channel)
        case .ping:
            let pong = WebSocketFrame(fin: true, opcode: .pong, data: frame.unmaskedData)
            context.writeAndFlush(wrapOutboundOut(pong), promise: nil)
        case .connectionClose:
            let close = WebSocketFrame(
                fin: true, opcode: .connectionClose,
                data: context.channel.allocator.buffer(capacity: 0))
            context.writeAndFlush(wrapOutboundOut(close)).whenComplete { _ in
                context.close(promise: nil)
            }
        case .pong, .continuation:
            break  // pong ignored; fragmentation not used by the JS client
        default:
            break
        }
    }

    // MARK: - frame dispatch

    private func handleText(_ text: String, channel: Channel) {
        guard let parsed = try? JSON.parse(text) else { return }
        switch parsed["type"].asString {
        case "call":
            let target = parsed["target"].asString ?? hostId
            let payload = parsed["payload"]
            let id = parsed["id"].asString ?? ""
            let token = parsed["auth_token"].asString
            let legId = self.legId
            let kernel = self.kernel
            Task {
                if let denied = gateWebLeg(
                    kernel: kernel, legId: legId, target: target,
                    verb: payload["type"].asString ?? "", token: token)
                {
                    Self.writeReply(channel: channel, id: id, data: denied)
                    return
                }
                // Field MUST be `data` — transport.js reads `msg.data`.
                let reply = await kernel.send(AgentId(target), payload)
                Self.writeReply(channel: channel, id: id, data: reply)
            }
        case "emit":
            // Fire-and-forget into a target's inbox; no reply, not gated.
            let target = parsed["target"].asString ?? hostId
            let payload = parsed["payload"]
            let kernel = self.kernel
            Task { await kernel.emit(AgentId(target), payload) }
        case "watch":
            if let src = parsed["src"].asString {
                let s = AgentId(src)
                if !watching.contains(s) {
                    kernel.watch(src: s, watcher: clientId)
                    watching.insert(s)
                }
            }
        case "unwatch":
            if let src = parsed["src"].asString {
                let s = AgentId(src)
                if watching.contains(s) {
                    kernel.unwatch(src: s, watcher: clientId)
                    watching.remove(s)
                }
            }
        default:
            break  // unknown frame type — ignore (weak)
        }
    }

    private func handleBinary(_ payload: Data, channel: Channel) {
        // The binary WS message is the `[4B len|header|body]` codec frame —
        // the STANDARD call envelope `{type:"call", id, target, payload:{…}}`
        // with raw body trailing. Dispatch the INNER payload (py/rust parity).
        guard let (header, blob) = Codec.decodeBinaryFrame(payload) else { return }
        let target = header["target"].asString ?? hostId
        let id = header["id"].asString ?? ""
        let inner = header["payload"]
        let token = header["auth_token"].asString
        let legId = self.legId
        let kernel = self.kernel
        Task {
            if let denied = gateWebLeg(
                kernel: kernel, legId: legId, target: target,
                verb: inner["type"].asString ?? "", token: token)
            {
                Self.writeReply(channel: channel, id: id, data: denied)
                return
            }
            let (reply, body) = await kernel.sendWithBinary(AgentId(target), inner, blob)
            if body.isEmpty {
                Self.writeReply(channel: channel, id: id, data: reply)
            } else {
                // read_stream reply → binary frame, body raw at `data.bytes`.
                var env: JSON = .object([
                    "type": .string("reply"), "id": .string(id), "data": reply,
                ])
                env["_binary_path"] = .string("data.bytes")
                Self.writeBinary(
                    channel: channel, bytes: Codec.encodeBinaryFrame(header: env, body: body))
            }
        }
    }

    // MARK: - writes (channel is Sendable; writeAndFlush hops to the loop)

    private static func writeReply(channel: Channel, id: String, data: JSON) {
        let frame: JSON = .object(["type": .string("reply"), "id": .string(id), "data": data])
        writeText(channel: channel, text: frame.serialize())
    }

    static func writeText(channel: Channel, text: String) {
        var buf = channel.allocator.buffer(capacity: text.utf8.count)
        buf.writeString(text)
        let frame = WebSocketFrame(fin: true, opcode: .text, data: buf)
        channel.writeAndFlush(frame, promise: nil)
    }

    static func writeBinary(channel: Channel, bytes: Data) {
        var buf = channel.allocator.buffer(capacity: bytes.count)
        buf.writeBytes(bytes)
        let frame = WebSocketFrame(fin: true, opcode: .binary, data: buf)
        channel.writeAndFlush(frame, promise: nil)
    }
}
