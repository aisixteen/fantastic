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
        let path = url.path.isEmpty ? "/" : url.path
        let uri = url.query.map { $0.isEmpty ? path : "\(path)?\($0)" } ?? path

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
                    host: Self.hostHeader(host, port: port, secure: isTLS), uri: uri,
                    subprotocols: subprotocols, authToken: authToken, signal: signal)
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

    /// The upgrade's `Host` — python `websockets`' `build_host` (RFC 6455 §4.1 /
    /// RFC 3986 §3.2.2): the port rides along unless it is the scheme's
    /// default, and an IPv6 literal is bracketed.
    static func hostHeader(_ host: String, port: Int, secure: Bool) -> String {
        let h = host.contains(":") ? "[\(host)]" : host
        return port == (secure ? 443 : 80) ? h : "\(h):\(port)"
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
        //
        // WITH THE 1000 STATUS CODE, as python's `websockets.close()` sends by
        // default (`_ws.py::close` passes nothing). An EMPTY close payload is
        // "no status code" — the peer records 1005, which is not one of
        // `websockets`' OK_CLOSE_CODES, so the same orderly shutdown read as an
        // error on one runtime and a clean close on the other.
        var payload = channel.allocator.buffer(capacity: 2)
        payload.write(webSocketErrorCode: .normalClosure)
        let close = WebSocketFrame(
            fin: true, opcode: .connectionClose, maskKey: wsRandomMaskKey(), data: payload)
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
final class WSUpgradeRequester: ChannelInboundHandler, RemovableChannelHandler,
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
        // A bodiless GET: no framing header (python's `websockets` sends none).
        headers.add(name: "Host", value: host)
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
final class WSClientHandler: ChannelInboundHandler, @unchecked Sendable {
    typealias InboundIn = WebSocketFrame
    typealias OutboundOut = WebSocketFrame

    private let inbox: WSClientInbox
    /// One echo per connection — a close we answered is never answered twice.
    private var closeEchoed = false
    /// python `websockets.connect`'s keepalive (`ping_interval=20`,
    /// `ping_timeout=20`): a ping every 20 s, and a connection whose pong does
    /// not come back in time is failed 1011 `keepalive ping timeout` — which is
    /// what lets a bridge see a half-open peer (`bridge_down`) before a send.
    static let pingIntervalSeconds: Int64 = 20
    static let pongTimeoutSeconds: Int64 = 20
    private var keepalive: RepeatedTask?
    private var awaitingPong = false

    init(inbox: WSClientInbox) {
        self.inbox = inbox
    }

    func handlerAdded(context: ChannelHandlerContext) {
        let bound = NIOLoopBound(context, eventLoop: context.eventLoop)
        keepalive = context.eventLoop.scheduleRepeatedTask(
            initialDelay: .seconds(Self.pingIntervalSeconds),
            delay: .seconds(Self.pongTimeoutSeconds)
        ) { [weak self] _ in
            guard let self else { return }
            let ctx = bound.value
            if self.awaitingPong {
                self.keepalive?.cancel()
                var reason = ctx.channel.allocator.buffer(capacity: 24)
                reason.writeInteger(UInt16(1011))
                reason.writeString("keepalive ping timeout")
                let close = WebSocketFrame(
                    fin: true, opcode: .connectionClose, maskKey: wsRandomMaskKey(), data: reason)
                self.inbox.fail(
                    NIOWebSocketClient.Closed(reason: "websocket: keepalive ping timeout"))
                ctx.writeAndFlush(self.wrapOutboundOut(close)).whenComplete { _ in
                    bound.value.close(promise: nil)
                }
                return
            }
            self.awaitingPong = true
            let ping = WebSocketFrame(
                fin: true, opcode: .ping, maskKey: wsRandomMaskKey(),
                data: ctx.channel.allocator.buffer(capacity: 0))
            ctx.writeAndFlush(self.wrapOutboundOut(ping), promise: nil)
        }
    }

    func handlerRemoved(context: ChannelHandlerContext) {
        keepalive?.cancel()
        keepalive = nil
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
            // ECHO IT BACK, then drop the connection — RFC 6455 §5.5.1: the
            // endpoint MUST answer a Close with a Close, reusing the peer's
            // status code. Without the echo the peer's closing handshake never
            // completes: `websockets` decides between `ConnectionClosedOK` and
            // `ConnectionClosedError` on `close_rcvd is not None and
            // close_sent is not None and close_rcvd.code in OK_CLOSE_CODES`
            // (`protocol.py`), so a peer that closed with 1001 read swift's
            // silent TCP drop as an ERROR and logged one — python echoes (its
            // `websockets` client does it for free) and the same close is
            // clean. Measured on `bridges/peer_frames.json`: swift left an
            // extra `read_end` frame python does not have.
            inbox.fail(NIOWebSocketClient.Closed(reason: "websocket: peer closed"))
            if closeEchoed {
                context.close(promise: nil)
                return
            }
            closeEchoed = true
            let echo = WebSocketFrame(
                fin: true, opcode: .connectionClose, maskKey: wsRandomMaskKey(),
                data: frame.unmaskedData)
            let bound = NIOLoopBound(context, eventLoop: context.eventLoop)
            context.writeAndFlush(wrapOutboundOut(echo)).whenComplete { _ in
                bound.value.close(promise: nil)
            }
        case .pong:
            awaitingPong = false
        default:
            break  // continuation (aggregated away) — ignore.
        }
    }

    func channelInactive(context: ChannelHandlerContext) {
        keepalive?.cancel()
        keepalive = nil
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
