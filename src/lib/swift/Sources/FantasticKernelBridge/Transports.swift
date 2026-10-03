// ws_bridge transport seam — WS to a remote `web_ws`, and the `memory` test
// transport. It spawns nothing: an ssh tunnel is an `exec_bridge` job or devops,
// and the bridge dials its local end.
//
// Python `ws_bridge/_ws.py`: the bridge is an ASYMMETRIC PURE CLIENT — it dials
// `ws://{host}:{port}/{peer_id}/ws` and ships raw bridge frames
// (`call`/`reply`/`watch`/`event`). The engine + verbs live in `Engine.swift`;
// this file supplies only the transport builders and the WS mechanics.
//
// Every failure here raises `ConnectionClosed` or `BridgeConfigError` carrying
// ONLY python's message — the engine wraps it (`bridge: ws boot failed: …`).

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Python's `ValueError`/`RuntimeError`/`TimeoutError` out of `build_transport`:
/// the engine interpolates `str(e)`, so the description is the message alone.
public struct BridgeConfigError: Error, CustomStringConvertible, Sendable {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}

/// Python `_ws.TUNNEL_READY_TIMEOUT`.
let TUNNEL_READY_TIMEOUT = 5.0

// MARK: - WS transport

/// Python `WSTransport`: a websockets client wrapper. A frame carrying raw
/// bytes goes as a BINARY WS frame `[4B len|header|body]` (never base64); a
/// plain frame as a TEXT WS frame (UTF-8 JSON).
public actor WSTransport: BridgeTransport {
    /// The WS surface, through the same `RelayWire` seam the relay leg uses —
    /// python constructs `WSTransport(ws)` around whatever object speaks
    /// send/recv/close, which is how its resilience tests feed it frames.
    private let client: any RelayWire
    /// The socket is down: the peer closed it or it dropped (a receive or a send
    /// failed), or this side closed it.
    private var closedFlag = false

    public init(client: any RelayWire) {
        self.client = client
    }

    /// Dial, or raise with the dial's own text (the engine prefixes it).
    static func connect(url: URL) async throws -> WSTransport {
        do {
            return WSTransport(client: try await NIOWebSocketClient.connect(url: url))
        } catch {
            throw ConnectionClosed("\(error)")
        }
    }

    /// Python `WSTransport.closed` — the socket's state (`CLOSING`/`CLOSED`), so
    /// a leg whose remote dropped reflects `connected:false` and `boot` dials
    /// again.
    public func isClosed() async -> Bool { closedFlag }

    public func send(_ frame: JSON) async throws {
        do {
            try await client.send(.text(Codec.encodeText(frame)))
        } catch {
            closedFlag = true
            throw ConnectionClosed("\(error)")
        }
    }

    public func sendBinary(header: JSON, body: Data) async throws {
        do {
            try await client.send(
                .binary([UInt8](Codec.encodeBinaryFrame(header: header, body: body))))
        } catch {
            closedFlag = true
            throw ConnectionClosed("\(error)")
        }
    }

    /// The next DECODABLE frame. A malformed one is SKIPPED and NAMED on stderr,
    /// not fatal: the peer is remote, so a short binary header or bad JSON is an
    /// input, not a bug — raising here killed the read loop while the transport
    /// stayed open, leaving a leg that reflects `connected:true` and times out
    /// every forward. Python `ws_bridge/_ws.py:67-81`, whose two lines this
    /// prints verbatim (the `<e>` tail is the runtime's own decode message).
    public func recv() async throws -> (frame: JSON, body: Data, isBinary: Bool) {
        while true {
            let msg: NIOWebSocketClient.Message
            do {
                msg = try await client.receive()
            } catch {
                closedFlag = true
                throw ConnectionClosed("\(error)")
            }
            switch msg {
            case .text(let s):
                let frame: JSON
                do {
                    frame = try JSON.parseStrict(s)
                } catch {
                    RuntimeLog.warn("ws_bridge: skipping malformed frame: bad text frame: \(error)")
                    continue
                }
                guard case .object = frame else {
                    RuntimeLog.warn("ws_bridge: skipping non-object frame")
                    continue
                }
                return (frame, Data(), false)
            case .binary(let bytes):
                let d = Data(bytes)
                let header: JSON
                let body: Data
                do {
                    (header, body) = try Codec.decodeBinaryFrame(d)
                } catch {
                    RuntimeLog.warn("ws_bridge: skipping malformed frame: \(error)")
                    continue
                }
                guard case .object = header else {
                    RuntimeLog.warn("ws_bridge: skipping non-object frame")
                    continue
                }
                return (header, body, true)
            }
        }
    }

    public func close() async {
        closedFlag = true
        await client.close()
    }
}

// MARK: - memory transport

/// Python `MemoryTransport`: an in-process pipe, one half of a peered pair — what
/// one half sends, the other receives. It CARRIES frames and nothing else: each
/// half is driven by a bridge leg's engine, so the far kernel serves an inbound
/// `call` through its own leg — the ingress rule, then the leg's own send, gated
/// as the leg — never as the operator (audit G-4, 2026-10-03).
public final class MemoryTransport: BridgeTransport, @unchecked Sendable {
    private let pipe: MemoryPipe
    /// This half receives on `pipe.queues[side]` and sends to the other.
    private let side: Int

    private init(_ pipe: MemoryPipe, _ side: Int) {
        self.pipe = pipe
        self.side = side
    }

    /// Python `MemoryTransport.pair()`: A's `send` reaches B's `recv` and vice
    /// versa.
    public static func pair() -> (MemoryTransport, MemoryTransport) {
        let pipe = MemoryPipe()
        return (MemoryTransport(pipe, 0), MemoryTransport(pipe, 1))
    }

    /// Either half closing closes the pair.
    public func isClosed() async -> Bool { pipe.withLock { pipe.isClosed } }

    public func send(_ frame: JSON) async throws { try deliver((frame, Data(), false)) }

    public func sendBinary(header: JSON, body: Data) async throws {
        try deliver((header, body, true))
    }

    private func deliver(_ frame: (JSON, Data, Bool)) throws {
        let peer = 1 - side
        let waiter: (@Sendable ((JSON, Data, Bool)?) -> Void)? = try pipe.withLock {
            if pipe.isClosed { throw ConnectionClosed("MemoryTransport closed") }
            if let w = pipe.waiters[peer] {
                pipe.waiters[peer] = nil
                return w
            }
            pipe.queues[peer].append(frame)
            return nil
        }
        waiter?(frame)
    }

    /// The next frame; one that arrived before a close is still delivered.
    public func recv() async throws -> (frame: JSON, body: Data, isBinary: Bool) {
        let next: (JSON, Data, Bool)? = await withCheckedContinuation {
            (c: CheckedContinuation<(JSON, Data, Bool)?, Never>) in
            let box = MemoryWaiter(c)
            let ready: (JSON, Data, Bool)?? = pipe.withLock {
                if !pipe.queues[side].isEmpty { return .some(pipe.queues[side].removeFirst()) }
                if pipe.isClosed { return .some(nil) }
                pipe.waiters[side] = { box.finish($0) }
                return nil
            }
            if let ready { box.finish(ready) }
        }
        guard let next else { throw ConnectionClosed("MemoryTransport closed") }
        return next
    }

    /// Closes BOTH halves: a reader parked on either wakes with the close.
    public func close() async {
        let parked = pipe.withLock {
            pipe.closed[side] = true
            let w = pipe.waiters
            pipe.waiters = [nil, nil]
            return w
        }
        for w in parked { w?(nil) }
    }
}

/// The two queues, the two parked readers and the two close flags a memory pair
/// shares — behind one lock.
private final class MemoryPipe: @unchecked Sendable {
    private let lock = NSLock()
    var queues: [[(JSON, Data, Bool)]] = [[], []]
    var waiters: [(@Sendable ((JSON, Data, Bool)?) -> Void)?] = [nil, nil]
    var closed = [false, false]
    var isClosed: Bool { closed[0] || closed[1] }

    /// `NSLock` is unavailable from an async context, so every critical section
    /// here is a synchronous closure.
    func withLock<T>(_ body: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try body()
    }
}

private final class MemoryWaiter: @unchecked Sendable {
    private let continuation: CheckedContinuation<(JSON, Data, Bool)?, Never>
    init(_ c: CheckedContinuation<(JSON, Data, Bool)?, Never>) { continuation = c }
    func finish(_ pair: (JSON, Data, Bool)?) { continuation.resume(returning: pair) }
}

// MARK: - the build_transport seam

extension KernelBridgeBundle {
    /// Python `build_transport`: the ONE thing that differs per derivation.
    func buildTransport(
        kind: String, agentId: AgentId, meta: OrderedDictionary<String, JSON>, kernel: Kernel,
        st: BridgeState
    ) async throws -> any BridgeTransport {
        switch family {
        case .ws: return try await buildWsTransport(kind: kind, meta: meta, st: st)
        case .relay:
            // python's relay `build_transport` ignores `kind` entirely — the
            // record's `transport` only names the reflect/`already` label.
            return try await buildRelayTransport(
                agentId: agentId, meta: meta, kernel: kernel, st: st)
        }
    }

    /// Python `ws_bridge._ws.build_transport`.
    private func buildWsTransport(
        kind: String, meta: OrderedDictionary<String, JSON>, st: BridgeState
    ) async throws -> any BridgeTransport {
        let peerIdValue = meta["peer_id"] ?? .null
        guard pyTruthy(peerIdValue) else {
            throw BridgeConfigError("peer_id required for the ws transport")
        }
        let peerId = pyStr(peerIdValue)
        if kind == "ws" {
            let portValue = meta["port"] ?? .null
            guard pyTruthy(portValue) else {
                throw BridgeConfigError("ws transport requires port")
            }
            let hostValue = meta["host"] ?? .null
            guard pyTruthy(hostValue) else {
                throw BridgeConfigError("ws transport requires host (nothing is guessed)")
            }
            let host = pyStr(hostValue)
            let port = try intArg(portValue)
            guard let url = URL(string: "ws://\(host):\(port)/\(peerId)/ws") else {
                throw BridgeConfigError("ws transport: malformed url")
            }
            return try await WSTransport.connect(url: url)
        }
        throw BridgeConfigError("unknown transport \(pyRepr(kind))")
    }

    /// `int(x)` inside `build_transport`: python raises the bare ValueError text.
    private func intArg(_ v: JSON) throws -> Int {
        do {
            return try pyInt(v)
        } catch let e as PyError {
            throw BridgeConfigError(e.message)
        }
    }
}
