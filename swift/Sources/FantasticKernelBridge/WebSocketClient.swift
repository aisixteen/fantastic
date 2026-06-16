// Cross-platform WebSocket CLIENT on swift-nio.
//
// `URLSessionWebSocketTask` is Apple-only — it does not exist (functionally) on
// swift-corelibs-foundation, so the bridge's dial-OUT legs (ws_bridge's
// `WebSocketTransport`, relay_connector's `RelayTransport`) could not run on Linux.
// This is the one missing primitive: a minimal WS client with a pull-based
// async surface (`send` / `receive` / `close`) mirroring the slice of
// `URLSessionWebSocketTask` those legs used — so they run identically on macOS
// and Linux. The bridge SERVER side already moved to NIO (FantasticWeb); this
// completes the client side. Supports `ws://` + `wss://` and offered
// subprotocols (the relay pairs on `Sec-WebSocket-Protocol`).
//
// Frames are single (non-fragmented) text/binary — the only shapes the bridge
// sends; `NIOWebSocketFrameAggregator` reassembles any the peer fragments.
// Client→server frames are masked per RFC 6455 (random per-frame key).

import Foundation
import NIOCore
import NIOHTTP1
import NIOPosix
import NIOSSL
import NIOWebSocket

/// A swift-nio WebSocket client. Pull-based: `receive()` awaits the next
/// message (or throws when the socket closes). Actor-isolated; channel writes
/// hop to the event loop internally, so callers may `send`/`receive`/`close`
/// from any task.
public actor NIOWebSocketClient {
    public enum Message: Sendable {
        case text(String)
        case binary([UInt8])
    }

    /// Thrown by `receive()` once the connection has closed (peer close frame,
    /// TCP drop, or local `close()`).
    public struct Closed: Error {
        public let reason: String
        public init(reason: String) { self.reason = reason }
    }

    /// Process-shared loop group (matches the rest of the kernel's NIO usage —
    /// never shut down).
    private static let group: EventLoopGroup = MultiThreadedEventLoopGroup.singleton
    private static let maxFrameSize = 1 << 24  // 16 MiB — matches the server + cloud MAX_FRAME.

    private let channel: Channel
    private let inbox: WSClientInbox

    private init(channel: Channel, inbox: WSClientInbox) {
        self.channel = channel
        self.inbox = inbox
    }

    /// Dial `url`, complete the HTTP→WS upgrade, and return a connected client.
    /// `subprotocols` are offered verbatim as `Sec-WebSocket-Protocol`;
    /// `authToken`, if set, is sent as an `X-Fantastic-Auth` header (the relay's
    /// connection credential). Throws if the TCP connect, TLS handshake, or WS
    /// upgrade fails.
    public static func connect(
        url: URL, subprotocols: [String] = [], authToken: String? = nil
    ) async throws -> NIOWebSocketClient {
        guard let scheme = url.scheme?.lowercased(),
            let host = url.host
        else {
            throw Closed(reason: "websocket: malformed url \(url)")
        }
        let isTLS = scheme == "wss"
        let port = url.port ?? (isTLS ? 443 : 80)
        var uri = url.path.isEmpty ? "/" : url.path
        if let q = url.query, !q.isEmpty { uri += "?\(q)" }

        let inbox = WSClientInbox()
        // Exactly-once completion guard around the upgrade promise: it's
        // resolved by whichever fires first — upgrade success, a pre-upgrade
        // channelInactive (dial refused / peer down), or a `connect()` throw
        // below. Completing a bare `EventLoopPromise` twice traps, and dropping
        // it uncompleted trips NIO's debug "leaking promise" assertion (which
        // crashed a debug build when a bridge dialed a not-yet-up peer).
        let signal = WSUpgradeSignal(group.next().makePromise(of: Void.self))

        let bootstrap = ClientBootstrap(group: group)
            .channelOption(.socketOption(.so_reuseaddr), value: 1)
            .channelInitializer { channel in
                let upgrader = NIOWebSocketClientUpgrader(
                    maxFrameSize: Self.maxFrameSize,
                    upgradePipelineHandler: { ch, _ in
                        ch.eventLoop.makeCompletedFuture {
                            let agg = NIOWebSocketFrameAggregator(
                                minNonFinalFragmentSize: 0,
                                maxAccumulatedFrameCount: Int.max,
                                maxAccumulatedFrameSize: Self.maxFrameSize)
                            try ch.pipeline.syncOperations.addHandler(agg)
                            try ch.pipeline.syncOperations.addHandler(
                                WSClientHandler(inbox: inbox))
                        }
                    })
                let config: NIOHTTPClientUpgradeSendableConfiguration = (
                    upgraders: [upgrader],
                    completionHandler: { _ in signal.succeed() }
                )
                let requester = WSUpgradeRequester(
                    host: host, uri: uri, subprotocols: subprotocols,
                    authToken: authToken, signal: signal)
                do {
                    if isTLS {
                        let sslContext = try NIOSSLContext(
                            configuration: TLSConfiguration.makeClientConfiguration())
                        let ssl = try NIOSSLClientHandler(
                            context: sslContext, serverHostname: host)
                        try channel.pipeline.syncOperations.addHandler(ssl)
                    }
                } catch {
                    return channel.eventLoop.makeFailedFuture(error)
                }
                return channel.pipeline.addHTTPClientHandlers(
                    withClientUpgrade: config
                ).flatMapThrowing {
                    try channel.pipeline.syncOperations.addHandler(requester)
                }
            }

        let channel: Channel
        do {
            channel = try await bootstrap.connect(host: host, port: port).get()
        } catch {
            // TCP/dial failure: the pipeline may never have reached a handler
            // that completes the signal, so settle it here (idempotent).
            signal.fail(error)
            throw error
        }
        // The upgrade may fail AFTER connect (bad response / non-101); the
        // requester's channelInactive fails the signal so this never hangs.
        do {
            try await signal.future.get()
        } catch {
            try? await channel.close().get()
            throw error
        }
        return NIOWebSocketClient(channel: channel, inbox: inbox)
    }

    /// Send one text/binary message as a single masked frame.
    public func send(_ message: Message) async throws {
        let frame: WebSocketFrame
        switch message {
        case .text(let s):
            var buf = channel.allocator.buffer(capacity: s.utf8.count)
            buf.writeString(s)
            frame = WebSocketFrame(
                fin: true, opcode: .text, maskKey: wsRandomMaskKey(), data: buf)
        case .binary(let bytes):
            var buf = channel.allocator.buffer(capacity: bytes.count)
            buf.writeBytes(bytes)
            frame = WebSocketFrame(
                fin: true, opcode: .binary, maskKey: wsRandomMaskKey(), data: buf)
        }
        try await channel.writeAndFlush(frame)
    }

    /// Await the next inbound message. Throws `Closed` once the socket is gone.
    public func receive() async throws -> Message {
        try await inbox.next()
    }

    /// Close the connection (best-effort close frame + TCP close). Idempotent.
    public func close() async {
        // A clean close frame, then drop the channel. `inbox` is failed by the
        // handler's channelInactive so any pending `receive()` unblocks.
        let empty = channel.allocator.buffer(capacity: 0)
        let close = WebSocketFrame(
            fin: true, opcode: .connectionClose, maskKey: wsRandomMaskKey(), data: empty)
        _ = try? await channel.writeAndFlush(close)
        try? await channel.close().get()
    }
}

// ── inbox: lock-guarded cross-thread message handoff (EL → actor) ──────────

/// FIFO bridge from the event-loop handler (which delivers frames) to the
/// actor's `receive()`. Lock-guarded so frame ORDER is preserved — a
/// `Task{}`-per-frame hop would race and reorder a stream.
final class WSClientInbox: @unchecked Sendable {
    private let lock = NSLock()
    private var queue: [NIOWebSocketClient.Message] = []
    private var waiters: [CheckedContinuation<NIOWebSocketClient.Message, Error>] = []
    private var failure: Error?

    func deliver(_ m: NIOWebSocketClient.Message) {
        lock.lock()
        if !waiters.isEmpty {
            let w = waiters.removeFirst()
            lock.unlock()
            w.resume(returning: m)
            return
        }
        queue.append(m)
        lock.unlock()
    }

    func fail(_ error: Error) {
        lock.lock()
        if failure == nil { failure = error }
        let pending = waiters
        waiters.removeAll()
        lock.unlock()
        for w in pending { w.resume(throwing: error) }
    }

    func next() async throws -> NIOWebSocketClient.Message {
        try await withCheckedThrowingContinuation { cont in
            lock.lock()
            if !queue.isEmpty {
                let m = queue.removeFirst()
                lock.unlock()
                cont.resume(returning: m)
                return
            }
            if let f = failure {
                lock.unlock()
                cont.resume(throwing: f)
                return
            }
            waiters.append(cont)
            lock.unlock()
        }
    }
}

// ── pipeline handlers ──────────────────────────────────────────────────────

/// Writes the HTTP GET that initiates the upgrade on `channelActive`. The
/// `NIOWebSocketClientUpgrader` injects the `Upgrade`/`Connection`/
/// `Sec-WebSocket-Key`/`-Version` headers; this only adds `Host`, a zero
/// `Content-Length`, and any offered subprotocols.
private final class WSUpgradeRequester: ChannelInboundHandler, RemovableChannelHandler,
    @unchecked Sendable
{
    typealias InboundIn = HTTPClientResponsePart
    typealias OutboundOut = HTTPClientRequestPart

    private let host: String
    private let uri: String
    private let subprotocols: [String]
    private let authToken: String?
    private let signal: WSUpgradeSignal

    init(
        host: String, uri: String, subprotocols: [String], authToken: String?,
        signal: WSUpgradeSignal
    ) {
        self.host = host
        self.uri = uri
        self.subprotocols = subprotocols
        self.authToken = authToken
        self.signal = signal
    }

    func channelActive(context: ChannelHandlerContext) {
        var headers = HTTPHeaders()
        headers.add(name: "Host", value: host)
        headers.add(name: "Content-Length", value: "0")
        if !subprotocols.isEmpty {
            headers.add(
                name: "Sec-WebSocket-Protocol", value: subprotocols.joined(separator: ", "))
        }
        if let authToken = authToken {
            headers.add(name: "X-Fantastic-Auth", value: authToken)
        }
        let head = HTTPRequestHead(
            version: .http1_1, method: .GET, uri: uri, headers: headers)
        context.write(wrapOutboundOut(.head(head)), promise: nil)
        context.writeAndFlush(wrapOutboundOut(.end(nil)), promise: nil)
        context.fireChannelActive()
    }

    func channelInactive(context: ChannelHandlerContext) {
        // If we go inactive before the upgrade completed, the upgrade failed —
        // unblock the awaiting `connect()`. (No-op once already succeeded.)
        signal.fail(NIOWebSocketClient.Closed(reason: "websocket: upgrade did not complete"))
        context.fireChannelInactive()
    }
}

/// Exactly-once completion wrapper around the upgrade promise. `succeed`/`fail`
/// after the first call are no-ops — so the success path, a pre-upgrade
/// `channelInactive`, and a `connect()` throw can all race to settle it without
/// double-completing (a trap) or leaking it uncompleted (a debug-build crash).
final class WSUpgradeSignal: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false
    private let promise: EventLoopPromise<Void>

    init(_ promise: EventLoopPromise<Void>) {
        self.promise = promise
    }

    var future: EventLoopFuture<Void> { promise.futureResult }

    func succeed() {
        lock.lock()
        let first = !done
        done = true
        lock.unlock()
        if first { promise.succeed(()) }
    }

    func fail(_ error: Error) {
        lock.lock()
        let first = !done
        done = true
        lock.unlock()
        if first { promise.fail(error) }
    }
}

/// Post-upgrade frame handler: aggregated text/binary → the inbox; ping → pong;
/// close / inactive / error → fail the inbox so `receive()` throws.
private final class WSClientHandler: ChannelInboundHandler, @unchecked Sendable {
    typealias InboundIn = WebSocketFrame
    typealias OutboundOut = WebSocketFrame

    private let inbox: WSClientInbox

    init(inbox: WSClientInbox) {
        self.inbox = inbox
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let frame = unwrapInboundIn(data)
        switch frame.opcode {
        case .text:
            var d = frame.unmaskedData
            let s = d.readString(length: d.readableBytes) ?? ""
            inbox.deliver(.text(s))
        case .binary:
            var d = frame.unmaskedData
            let bytes = d.readBytes(length: d.readableBytes) ?? []
            inbox.deliver(.binary(bytes))
        case .ping:
            let pong = WebSocketFrame(
                fin: true, opcode: .pong, maskKey: wsRandomMaskKey(), data: frame.unmaskedData)
            context.writeAndFlush(wrapOutboundOut(pong), promise: nil)
        case .connectionClose:
            inbox.fail(NIOWebSocketClient.Closed(reason: "websocket: peer closed"))
            context.close(promise: nil)
        default:
            break  // pong / continuation (aggregated away) — ignore.
        }
    }

    func channelInactive(context: ChannelHandlerContext) {
        inbox.fail(NIOWebSocketClient.Closed(reason: "websocket: connection closed"))
        context.fireChannelInactive()
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        inbox.fail(error)
        context.close(promise: nil)
    }
}

/// A fresh random 4-byte masking key — client→server frames MUST be masked
/// (RFC 6455 §5.3). `SystemRandomNumberGenerator` backs `UInt8.random`.
func wsRandomMaskKey() -> WebSocketMaskingKey {
    WebSocketMaskingKey((0..<4).map { _ in UInt8.random(in: UInt8.min...UInt8.max) })!
}
