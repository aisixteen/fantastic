// cloud_bridge transport — dial-OUT relay leg with end-to-end TLS 1.3 mTLS.
//
// Swift mirror of the canonical Python `cloud_bridge` (and the Rust
// `transport::cloud`). Both peers dial OUT (WSS) to a zero-trust relay that
// pairs them by `(tenant, rendezvous)` and forwards OPAQUE binary frames; the
// two peers then run a mutually-authenticated TLS 1.3 handshake — driven over
// the relay's byte pipe with NO socket — using self-signed Ed25519 device certs
// (see `CloudCert`), pinned by the peer's durable identity = its Ed25519 PUBLIC
// KEY (a custom verify callback, NOT the cert bytes — the cert is a disposable,
// possibly non-deterministic carrier), and tunnel the SAME kernel-bridge
// `call`/`reply`/`event` JSON frames as TLS application data, length-delimited
// (`u32` big-endian prefix). The relay sees only ciphertext.
//
// TLS-over-buffers uses an `EmbeddedChannel` + `NIOSSL{Client,Server}Handler`:
// we ferry ciphertext between the channel and the byte channel (the Swift analog
// of Python's `ssl.MemoryBIO` / Rust's `rustls` read_tls/write_tls). All channel
// ops are SYNCHRONOUS and we drive the loop with `embeddedEventLoop.run()` after
// connect/writeInbound so NIOSSL's scheduled handshake/encrypt writes (queued
// via `execute`) flush — exactly the demand-driven shape of Python's `_drive`.
// `EmbeddedEventLoop` is single-thread-affine, so the actor pins itself to one
// dedicated NIO thread (custom executor) and creates the channel there. The
// core is generic over a `CloudByteChannel`: `WSByteChannel` in production,
// `MemoryByteChannel` for the in-process loopback self-test.
//
// Like the canonical kernel, the leg is a SYMMETRIC peer: it both `forward`s
// (call → await reply) AND serves inbound `call` frames by dispatching them on
// the local kernel and replying. `reply`/`error` resolve pending forwards;
// `event` re-emits on the bridge's local inbox; `keepalive` is dropped. (Inbound
// `watch`/`unwatch` are ignored, matching the Python/Rust read loops.)

import Crypto
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import NIOEmbedded
import NIOPosix
import NIOSSL
import NIOTLS

// ── wire constants (byte-identical to Python/Rust) ─────────────────

/// WS subprotocol the relay authenticates + pairs on. The token rides as the
/// second offered subprotocol: `Sec-WebSocket-Protocol: fantastic.relay.v1, <token>`.
public let CLOUD_SUBPROTOCOL = "fantastic.relay.v1"
private let KEEPALIVE_TYPE = "keepalive"
private let MAX_FRAME = 16 * 1024 * 1024
private let HDR = 4
private let HEARTBEAT_SECONDS: UInt64 = 30
private let HANDSHAKE_MAX_ROUNDS = 100

// ── opaque-binary channel (the relay's WS Binary layer) ────────────

/// The opaque-binary-frame layer the TLS engine rides on. Each `sendBytes` /
/// `recvBytes` is one relay frame (the relay forwards them verbatim).
/// `recvBytes` throws `CloudChannelClosed` when the peer/relay hangs up.
public protocol CloudByteChannel: Sendable {
    func sendBytes(_ bytes: [UInt8]) async throws
    func recvBytes() async throws -> [UInt8]
    func close() async
}

/// Raised by a `CloudByteChannel` when the underlying transport closes.
public struct CloudChannelClosed: Error { public let reason: String }

/// Production channel: a URLSession WebSocket to the relay, Binary frames.
public actor WSByteChannel: CloudByteChannel {
    private let task: URLSessionWebSocketTask

    /// Dial `relayURL`, offering `fantastic.relay.v1` + the verbatim `token` as
    /// the two WS subprotocols (URLSession emits them as
    /// `Sec-WebSocket-Protocol: fantastic.relay.v1, <token>`).
    public init(relayURL: URL, token: String, session: URLSession = .shared) {
        self.task = session.webSocketTask(with: relayURL, protocols: [CLOUD_SUBPROTOCOL, token])
        self.task.resume()
    }

    public func sendBytes(_ bytes: [UInt8]) async throws {
        try await task.send(.data(Data(bytes)))
    }

    public func recvBytes() async throws -> [UInt8] {
        let msg = try await task.receive()
        switch msg {
        case .data(let d): return [UInt8](d)
        case .string(let s): return [UInt8](s.utf8)
        @unknown default: return []
        }
    }

    public func close() async {
        task.cancel(with: .normalClosure, reason: nil)
    }
}

/// In-process channel for the loopback self-test — a cross-wired pair.
public actor MemoryByteChannel: CloudByteChannel {
    private var inbox: [[UInt8]] = []
    private var waiters: [CheckedContinuation<[UInt8], Error>] = []
    private var peer: MemoryByteChannel?
    private var closed = false

    public init() {}

    /// Build a cross-wired pair: bytes sent on one arrive on the other.
    public static func pair() async -> (MemoryByteChannel, MemoryByteChannel) {
        let a = MemoryByteChannel()
        let b = MemoryByteChannel()
        await a.setPeer(b)
        await b.setPeer(a)
        return (a, b)
    }

    func setPeer(_ p: MemoryByteChannel) { peer = p }

    fileprivate func deliver(_ bytes: [UInt8]) {
        if !waiters.isEmpty {
            waiters.removeFirst().resume(returning: bytes)
        } else {
            inbox.append(bytes)
        }
    }

    public func sendBytes(_ bytes: [UInt8]) async throws {
        guard let peer, !closed else { throw CloudChannelClosed(reason: "memory channel closed") }
        await peer.deliver(bytes)
    }

    public func recvBytes() async throws -> [UInt8] {
        if !inbox.isEmpty { return inbox.removeFirst() }
        if closed { throw CloudChannelClosed(reason: "memory channel closed") }
        return try await withCheckedThrowingContinuation { cont in
            waiters.append(cont)
        }
    }

    public func close() async {
        closed = true
        let pending = waiters
        waiters.removeAll()
        for w in pending { w.resume(throwing: CloudChannelClosed(reason: "memory channel closed")) }
    }
}

// ── handshake-completion handler ───────────────────────────────────

/// Flips `done` when the TLS handshake completes. The flag is NSLock-guarded so
/// the transport actor can read it safely. This handler does NOT implement
/// `channelRead`, so decrypted app-data flows past it into the channel's inbound
/// buffer (drained via `readInbound`).
final class HandshakeWaiter: ChannelInboundHandler, @unchecked Sendable {
    typealias InboundIn = ByteBuffer
    private let lock = NSLock()
    private var _done = false
    var done: Bool {
        lock.lock()
        defer { lock.unlock() }
        return _done
    }
    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        if case TLSUserEvent.handshakeCompleted = event {
            lock.lock()
            _done = true
            lock.unlock()
        }
        context.fireUserInboundEventTriggered(event)
    }
}

// ── the transport ──────────────────────────────────────────────────

/// A `cloud_bridge` leg: a `CloudByteChannel` to the relay + an mTLS session
/// over it, tunnelling length-delimited kernel-bridge JSON frames.
public actor CloudBridgeTransport {
    /// One dedicated NIO thread pins every cloud_bridge `EmbeddedChannel` to a
    /// single pthread. The actor runs ALL its work on this thread (custom
    /// executor below) and creates its channel here, so `EmbeddedEventLoop`'s
    /// thread-affinity invariant always holds (no "NIO API misuse" warnings,
    /// safe under the future strict-mode crash). NIO ops are fast + release the
    /// thread at every `await` on the byte channel, so legs share it freely.
    private static let pinnedLoop: EventLoop =
        MultiThreadedEventLoopGroup(numberOfThreads: 1).next()

    public nonisolated var unownedExecutor: UnownedSerialExecutor {
        CloudBridgeTransport.pinnedLoop.executor.asUnownedSerialExecutor()
    }

    private let channel: any CloudByteChannel
    /// Created in `handshake` (the first isolated method, which runs on the
    /// pinned thread) so its `EmbeddedEventLoop.myThread` == the pinned thread.
    private var embedded: EmbeddedChannel!
    private let waiter: HandshakeWaiter
    private var rbuf: ByteBuffer

    /// Outstanding forwards keyed by id; resumed when the peer echoes the reply.
    private var pending: [String: CheckedContinuation<JSON, Never>] = [:]
    private var nextId: UInt64 = 1
    private let forwardTimeoutSeconds: Double = 30.0

    /// Local sink for inbound `event` re-emit + inbound `call` dispatch.
    private var eventSink: AgentId?
    private weak var kernel: Kernel?

    private var receiveTask: Task<Void, Never>?
    private var heartbeatTask: Task<Void, Never>?
    private var open = true

    private init(channel: any CloudByteChannel) {
        self.channel = channel
        self.waiter = HandshakeWaiter()
        self.rbuf = ByteBuffer()
    }

    /// Build the mTLS session over `channel` and run the handshake (pinning the
    /// peer to `approvedPeerPEMs`). `server` selects the TLS role. The local
    /// kernel sink (`localAgentId` + `localKernel`) is wired BEFORE the receive
    /// loop starts, so the very first inbound `call`/`event` frame dispatches on
    /// the kernel rather than racing a nil sink.
    public static func connect(
        channel: any CloudByteChannel,
        server: Bool,
        certDER: [UInt8],
        keyPKCS8: [UInt8],
        approvedPeerPEMs: [String],
        localAgentId: AgentId? = nil,
        localKernel: Kernel? = nil
    ) async throws -> CloudBridgeTransport {
        let t = CloudBridgeTransport(channel: channel)
        try await t.handshake(
            server: server, certDER: certDER, keyPKCS8: keyPKCS8,
            approvedPeerPEMs: approvedPeerPEMs)
        await t.finishBoot(localAgentId: localAgentId, localKernel: localKernel)
        return t
    }

    /// Wire the local kernel sink (if any) then start the receive + heartbeat
    /// loops — one atomic actor hop so the sink is live before any frame lands.
    private func finishBoot(localAgentId: AgentId?, localKernel: Kernel?) {
        if let localAgentId, let localKernel {
            self.eventSink = localAgentId
            self.kernel = localKernel
        }
        startLoops()
    }

    // ── TLS context + handshake ────────────────────────────────────

    private static func makeContext(
        server: Bool, certDER: [UInt8], keyPKCS8: [UInt8], approvedPeerPEMs: [String]
    ) throws -> NIOSSLContext {
        let cert = try NIOSSLCertificate(bytes: certDER, format: .der)
        let key = try NIOSSLPrivateKey(bytes: keyPKCS8, format: .der)
        guard !approvedPeerPEMs.isEmpty else {
            throw CloudChannelClosed(reason: "cloud_bridge: no approved peer certs")
        }
        var cfg =
            server
            ? TLSConfiguration.makeServerConfiguration(
                certificateChain: [.certificate(cert)], privateKey: .privateKey(key))
            : TLSConfiguration.makeClientConfiguration()
        cfg.certificateChain = [.certificate(cert)]
        cfg.privateKey = .privateKey(key)
        cfg.minimumTLSVersion = .tlsv13
        cfg.maximumTLSVersion = .tlsv13
        // BoringSSL's default signature-algorithm prefs omit Ed25519, so an
        // Ed25519-cert handshake dies with NO_COMMON_SIGNATURE_ALGORITHMS.
        // Advertise + sign with ed25519 explicitly (our only key type).
        cfg.signingSignatureAlgorithms = [.ed25519]
        cfg.verifySignatureAlgorithms = [.ed25519]
        // `.noHostnameVerification` requests + expects the peer cert (mutual auth
        // on the server) but skips hostname checks; the actual trust decision is
        // made by the custom pubkey-pinning callback on the handler, NOT here.
        cfg.certificateVerification = .noHostnameVerification
        return try NIOSSLContext(configuration: cfg)
    }

    /// The Ed25519 public key (32 bytes) carried by a cert — its durable
    /// identity. The SPKI is `30 2A 30 05 06 03 2B 65 70 03 21 00 <32B key>`, so
    /// the key is the trailing 32 bytes of the SPKI DER.
    private static func ed25519Pubkey(_ cert: NIOSSLCertificate) -> Data? {
        guard let spki = try? cert.extractPublicKey().toSPKIBytes(), spki.count >= 32 else {
            return nil
        }
        return Data(spki.suffix(32))
    }

    /// Approved peer pubkeys, extracted from the pinned `approved_peer_certs`.
    private static func approvedPubkeys(_ pems: [String]) -> Set<Data> {
        var out = Set<Data>()
        for pem in pems {
            guard let certs = try? NIOSSLCertificate.fromPEMBytes(Array(pem.utf8)) else { continue }
            for c in certs where ed25519Pubkey(c) != nil {
                out.insert(ed25519Pubkey(c)!)
            }
        }
        return out
    }

    private func handshake(
        server: Bool, certDER: [UInt8], keyPKCS8: [UInt8], approvedPeerPEMs: [String]
    ) async throws {
        // Created here (on the pinned executor thread) so its EmbeddedEventLoop
        // is bound to the thread every later op runs on.
        embedded = EmbeddedChannel()
        // A FAILED handshake (e.g. an un-pinned peer cert) discards this leg
        // WITHOUT a close() call, so tear the channel down HERE — on the pinned
        // thread, before the dropped actor's EmbeddedEventLoop deinits on the
        // caller's executor (NIO API misuse → a future hard crash). On SUCCESS we
        // KEEP it (the tunnel rides it); it's torn down later in close().
        var ok = false
        defer { if !ok { tearDownEmbedded() } }
        let ctx = try Self.makeContext(
            server: server, certDER: certDER, keyPKCS8: keyPKCS8,
            approvedPeerPEMs: approvedPeerPEMs)
        // Pin by the peer's durable identity = its Ed25519 PUBLIC KEY, not the
        // exact cert bytes. The cert is a disposable carrier (it may be
        // non-deterministic / rotate — e.g. Swift's CryptoKit randomizes Ed25519
        // signatures); only the key is the identity. A custom verification
        // callback extracts the peer leaf's SPKI pubkey and checks it against the
        // approved set (mirrors rust's pinned verifiers).
        let approvedPubkeys = Self.approvedPubkeys(approvedPeerPEMs)
        let verify: NIOSSLCustomVerificationCallback = { certs, promise in
            guard let leaf = certs.first, let pk = Self.ed25519Pubkey(leaf) else {
                promise.succeed(.failed)
                return
            }
            promise.succeed(approvedPubkeys.contains(pk) ? .certificateVerified : .failed)
        }
        let tlsHandler: ChannelHandler
        if server {
            tlsHandler = NIOSSLServerHandler(context: ctx, customVerificationCallback: verify)
        } else {
            tlsHandler = try NIOSSLClientHandler(
                context: ctx, serverHostname: nil, customVerificationCallback: verify)
        }
        // Synchronous EmbeddedChannel ops; drive the loop with run() so NIOSSL's
        // scheduled handshake/encrypt writes (queued via execute) flush.
        try embedded.pipeline.syncOperations.addHandler(tlsHandler)
        try embedded.pipeline.syncOperations.addHandler(waiter)
        // Activate → fires channelActive; the client schedules its ClientHello.
        let addr = try SocketAddress(ipAddress: "127.0.0.1", port: 1)
        embedded.connect(to: addr, promise: nil)
        embedded.embeddedEventLoop.run()

        var rounds = 0
        while rounds < HANDSHAKE_MAX_ROUNDS {
            rounds += 1
            // Flush all outbound ciphertext produced so far (ClientHello,
            // ServerHello.., our Finished — including the record that completes
            // the peer's handshake, produced by the LAST writeInbound).
            try await flushOutbound()
            if waiter.done { break }
            // Feed one inbound batch. A TLS error (bad/unpinned peer cert) is
            // caught in the pipeline and re-thrown by writeInbound → connect
            // aborts with a clean error.
            let data = try await channel.recvBytes()
            try await feedInbound(data)
        }
        guard waiter.done else {
            throw CloudChannelClosed(reason: "cloud_bridge: handshake did not complete")
        }
        // Push any trailing record (e.g. the client's Finished produced by the
        // writeInbound that flipped `done` in the final iteration).
        try await flushOutbound()
        ok = true  // handshake done — keep `embedded` alive for the tunnel.
    }

    /// Feed one inbound ciphertext batch through TLS, drive the loop so NIOSSL's
    /// scheduled writes/decrypts run, and accumulate any decrypted plaintext.
    private func feedInbound(_ data: [UInt8]) async throws {
        var buf = embedded.allocator.buffer(capacity: data.count)
        buf.writeBytes(data)
        try embedded.writeInbound(buf)
        embedded.embeddedEventLoop.run()
        // Surface a handshake/TLS failure. The pubkey-pin verify callback is
        // async (NIOSSL pauses, calls it, resumes on a later loop tick), so a
        // REJECTED peer cert fails the handshake during `run()` — AFTER
        // `writeInbound` already returned without throwing. Without this the loop
        // would fall through to `recvBytes()` and block forever (the peer has
        // nothing more to send). `throwIfErrorCaught` re-raises the caught
        // channel error → `connect` aborts cleanly (no-op on the success path).
        try embedded.throwIfErrorCaught()
        while let plain = try embedded.readInbound(as: ByteBuffer.self) {
            rbuf.writeImmutableBuffer(plain)
        }
    }

    /// Drain every outbound ciphertext buffer the channel has queued and ship
    /// each as one relay frame.
    private func flushOutbound() async throws {
        while let out = try embedded.readOutbound(as: ByteBuffer.self) {
            try await channel.sendBytes(Array(out.readableBytesView))
        }
    }

    // ── loops ──────────────────────────────────────────────────────

    private func startLoops() {
        receiveTask = Task { [weak self] in await self?.receiveLoop() }
        heartbeatTask = Task { [weak self] in await self?.heartbeatLoop() }
    }

    private func receiveLoop() async {
        // Drain anything captured during the handshake tail, then pump.
        await dispatchFrames()
        while open {
            let data: [UInt8]
            do {
                data = try await channel.recvBytes()
            } catch {
                break  // relay/peer closed
            }
            do {
                try await feedInbound(data)
            } catch {
                break  // TLS error
            }
            await dispatchFrames()
        }
        failAllPending("cloud_bridge connection closed")
    }

    private func heartbeatLoop() async {
        while open {
            try? await Task.sleep(nanoseconds: HEARTBEAT_SECONDS * 1_000_000_000)
            if !open { break }
            // App-level keepalive — the relay does NOT forward WS pings.
            _ = await sendFrame(.object(["type": .string(KEEPALIVE_TYPE)]))
        }
    }

    private func dispatchFrames() async {
        while let frame = popFrame() {
            await dispatch(frame)
        }
        rbuf.discardReadBytes()
    }

    /// Pop one complete length-delimited frame from `rbuf`, skipping keepalives.
    /// Returns nil when no complete frame is buffered yet.
    private func popFrame() -> JSON? {
        while rbuf.readableBytes >= HDR {
            let n = Int(rbuf.getInteger(at: rbuf.readerIndex, endianness: .big, as: UInt32.self) ?? 0)
            if n > MAX_FRAME {
                Task { await self.close() }
                return nil
            }
            if rbuf.readableBytes < HDR + n { return nil }
            rbuf.moveReaderIndex(forwardBy: HDR)
            let body = rbuf.readBytes(length: n) ?? []
            guard let frame = try? JSON.parse(String(decoding: body, as: UTF8.self)) else { continue }
            if frame["type"].asString == KEEPALIVE_TYPE { continue }
            return frame
        }
        return nil
    }

    private func dispatch(_ frame: JSON) async {
        switch frame["type"].asString {
        case "reply":
            if let id = frame["id"].asString, let cont = pending.removeValue(forKey: id) {
                cont.resume(returning: frame["data"])
            }
        case "error":
            if let id = frame["id"].asString, let cont = pending.removeValue(forKey: id) {
                cont.resume(
                    returning: .object([
                        "error": .string("remote error: \(frame["error"].asString ?? "unknown")")
                    ]))
            }
        case "event":
            if let sink = eventSink, let kernel = kernel {
                await kernel.emit(sink, frame["payload"])
            }
        case "call":
            // Symmetric peer: dispatch the inbound call on the local kernel and
            // ship the reply back (mirrors the Rust/Python read loop).
            let id = frame["id"]
            let target = frame["target"].asString ?? ""
            let reply: JSON
            if target.isEmpty {
                reply = .object(["error": .string("cloud_bridge: empty call target")])
            } else if let kernel = kernel {
                reply = await kernel.send(AgentId(target), frame["payload"])
            } else {
                reply = .object(["error": .string("cloud_bridge: no local kernel")])
            }
            _ = await sendFrame(
                .object(["type": .string("reply"), "id": id, "data": reply]))
        default:
            // reply/error/call/event handled; keepalive dropped in popFrame;
            // inbound watch/unwatch + unknown types ignored (Python/Rust parity).
            break
        }
    }

    // ── send path ──────────────────────────────────────────────────

    /// Encrypt + length-frame `frame` and ship it as relay ciphertext frames.
    private func sendFrame(_ frame: JSON) async -> Bool {
        guard open else { return false }
        let json = Array(frame.serialize().utf8)
        guard json.count <= MAX_FRAME else { return false }
        var plain = embedded.allocator.buffer(capacity: HDR + json.count)
        plain.writeInteger(UInt32(json.count), endianness: .big)
        plain.writeBytes(json)
        do {
            try embedded.writeOutbound(plain)  // NIOSSL encrypts → outbound
            embedded.embeddedEventLoop.run()
            try await flushOutbound()
        } catch {
            return false
        }
        return true
    }

    public func forward(target: AgentId, payload: JSON) async -> JSON {
        guard open else {
            return .object([
                "error": .string("cloud_bridge: not connected"),
                "reason": .string("not_connected"),
            ])
        }
        let id = mintId()
        let frame: JSON = .object([
            "type": .string("call"),
            "id": .string(id),
            "target": .string(target.value),
            "payload": payload,
        ])
        let sent = await sendFrame(frame)
        if !sent {
            return .object([
                "error": .string("cloud_bridge.forward: send failed"),
                "reason": .string("transport_error"),
            ])
        }
        return await withCheckedContinuation { cont in
            pending[id] = cont
            Task { await self.timeoutPending(id: id) }
        }
    }

    /// Best-effort `watch_remote`: ship a `{type:"watch", src:target}` frame.
    /// (Whether the peer streams events back is the peer's behavior; the
    /// Python/Rust read loops do not act on inbound watch frames today.)
    public func watchRemote(target: AgentId) async -> JSON {
        let ok = await sendFrame(
            .object(["type": .string("watch"), "src": .string(target.value)]))
        return ok
            ? .object(["ok": .bool(true), "watching": .string(target.value)])
            : .object(["error": .string("cloud_bridge.watch_remote: send failed")])
    }

    public func unwatchRemote(target: AgentId) async -> JSON {
        let ok = await sendFrame(
            .object(["type": .string("unwatch"), "src": .string(target.value)]))
        return ok
            ? .object(["ok": .bool(true), "unwatched": .string(target.value)])
            : .object(["error": .string("cloud_bridge.unwatch_remote: send failed")])
    }

    public func close() async {
        guard open else { return }
        open = false
        receiveTask?.cancel()
        heartbeatTask?.cancel()
        await channel.close()
        failAllPending("cloud_bridge closed")
        tearDownEmbedded()
    }

    // ── helpers ────────────────────────────────────────────────────

    /// Release the EmbeddedChannel on its OWN (pinned) thread. MUST be called
    /// from an actor-isolated method (so it runs on `pinnedLoop`): nil-ing the
    /// last strong ref HERE runs the EmbeddedEventLoop's deinit on the thread that
    /// created it, satisfying NIO's thread-affinity check — an off-thread deinit
    /// is "NIO API misuse" and a hard crash in future swift-nio. Uses a HARD
    /// `.all` close (never finish()/graceful shutdown — that blocks forever
    /// awaiting the now-gone peer's close_notify), then one `run()` to flush the
    /// deferred close completion so the loop has no unexecuted scheduled tasks.
    private func tearDownEmbedded() {
        guard let ch = embedded else { return }
        ch.close(mode: .all, promise: nil)
        ch.embeddedEventLoop.run()
        embedded = nil
    }

    private func timeoutPending(id: String) async {
        try? await Task.sleep(nanoseconds: UInt64(forwardTimeoutSeconds * 1_000_000_000))
        if let cont = pending.removeValue(forKey: id) {
            cont.resume(
                returning: .object([
                    "error": .string(
                        "cloud_bridge.forward: timeout after \(Int(forwardTimeoutSeconds))s"),
                    "reason": .string("timeout"),
                ]))
        }
    }

    private func failAllPending(_ reason: String) {
        let waiters = pending
        pending.removeAll()
        for (_, cont) in waiters {
            cont.resume(
                returning: .object([
                    "error": .string(reason),
                    "reason": .string("transport_dropped"),
                ]))
        }
    }

    private func mintId() -> String {
        nextId &+= 1
        return "cb_\(nextId)"
    }
}
