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

    public init(client: any RelayWire) { self.client = client }

    /// Dial, or raise with the dial's own text (the engine prefixes it).
    static func connect(url: URL) async throws -> WSTransport {
        do {
            return WSTransport(client: try await NIOWebSocketClient.connect(url: url))
        } catch {
            throw ConnectionClosed("\(error)")
        }
    }

    /// Python `getattr(self._ws, "closed", False)` — websockets 16 exposes no
    /// `closed`, so a ws leg never reports one (§7, measured).
    public func isClosed() async -> Bool { false }

    public func send(_ frame: JSON) async throws {
        do {
            try await client.send(.text(Codec.encodeText(frame)))
        } catch {
            throw ConnectionClosed("\(error)")
        }
    }

    public func sendBinary(header: JSON, body: Data) async throws {
        do {
            try await client.send(
                .binary([UInt8](Codec.encodeBinaryFrame(header: header, body: body))))
        } catch {
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

    public func close() async { await client.close() }
}

// MARK: - memory transport

/// Python's `memory` transport: the paired test backbone, no socket. This end
/// SERVES a second in-process kernel — a `call` frame is dispatched there and
/// its `reply` queued back, a `watch` subscribes and queues `event` frames — so
/// a test drives the engine's real frame path (correlation, the gate, binary
/// bodies) rather than a shortcut around it.
public final class MemoryTransport: BridgeTransport, @unchecked Sendable {
    private weak var remote: Kernel?
    private let lock = NSLock()
    private var queue: [(JSON, Data, Bool)] = []
    private var waiter: (@Sendable ((JSON, Data, Bool)?) -> Void)?
    private var closedFlag = false
    private var watchers: [AgentId: (watcher: AgentId, drain: Task<Void, Never>)] = [:]

    public init(remote: Kernel) { self.remote = remote }

    /// `NSLock` is unavailable from an async context, so every critical section
    /// here is a synchronous closure.
    private func withLock<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }

    public func isClosed() async -> Bool { withLock { closedFlag } }

    public func send(_ frame: JSON) async throws {
        if await isClosed() { throw ConnectionClosed("memory transport closed") }
        switch frame["type"].asString {
        case "call":
            guard let remote else { throw ConnectionClosed("remote kernel gone") }
            var item = Item(AgentId(frame["target"].asString ?? ""), frame["payload"])
            // the frame's `secret` marks, verbatim — what the remote's inbound leg
            // does with them (`Secret`)
            if let marks = frame.asObject?["secret"] { item.secret = marks }
            // A stream verb rides the binary channel even from a text frame, as
            // the remote's web_ws leg routes it (`Codec.binaryEdgeVerbs`): a
            // `read_stream` reply is raw bytes whatever frame asked for it.
            if Codec.binaryEdgeVerbs.contains(frame["payload"]["type"].asString ?? "") {
                item.rawReply = true
            }
            let call = item
            // OPERATOR on the remote: the local sender means nothing in the
            // remote's grant space, and admission was already gated by this
            // leg's own ingress rule.
            let out1 = await KernelTaskLocals.$currentSender.withValue(nil) {
                await remote.send([call])[0]
            }
            pushReply(frame["id"], out1)
        case "watch":
            await startWatch(AgentId(frame["src"].asString ?? ""))
        case "unwatch":
            await stopWatch(AgentId(frame["src"].asString ?? ""))
        default:
            break
        }
    }

    public func sendBinary(header: JSON, body: Data) async throws {
        if await isClosed() { throw ConnectionClosed("memory transport closed") }
        guard let remote, header["type"].asString == "call" else { return }
        let target = AgentId(header["target"].asString ?? "")
        // THROUGH THE BATCH, like the text call above: the body rides the
        // item's binary channel AT ITS PATH in the payload, so the call is an
        // execution with its rows.
        let (payload, binary) = Codec.callPayload(header, body: body)
        var item = Item(target, payload, binary: binary)
        if binary == nil, Codec.binaryEdgeVerbs.contains(payload["type"].asString ?? "") {
            item.rawReply = true
        }
        if let marks = header.asObject?["secret"] { item.secret = marks }
        let call = item
        let out1 = await KernelTaskLocals.$currentSender.withValue(nil) {
            await remote.send([call])[0]
        }
        pushReply(header["id"], out1)
    }

    /// The remote's answer as the frame web_ws writes: THE CARRIER, not the
    /// length, picks the kind — a reply holding a chunk (a 0-byte one included,
    /// C37) is a binary frame at `["data", "bytes"]`, any other a text frame.
    private func pushReply(_ id: JSON, _ out1: Outcome) {
        var out: JSON = .object(["type": .string("reply"), "id": id, "data": out1.result])
        guard let body = out1.body else {
            push((out, Data(), false))
            return
        }
        out["_binary_path"] = Codec.binaryPath(["data", "bytes"])
        push((out, body, true))
    }

    public func recv() async throws -> (frame: JSON, body: Data, isBinary: Bool) {
        let next: (JSON, Data, Bool)? = await withCheckedContinuation {
            (c: CheckedContinuation<(JSON, Data, Bool)?, Never>) in
            let box = MemoryWaiter(c)
            lock.lock()
            if closedFlag {
                lock.unlock()
                box.finish(nil)
                return
            }
            if !queue.isEmpty {
                let head = queue.removeFirst()
                lock.unlock()
                box.finish(head)
                return
            }
            waiter = { box.finish($0) }
            lock.unlock()
        }
        guard let next else { throw ConnectionClosed("memory transport closed") }
        return next
    }

    public func close() async {
        let (w, live) = withLock {
            closedFlag = true
            let w = waiter
            waiter = nil
            let live = watchers
            watchers = [:]
            return (w, live)
        }
        for (_, entry) in live { entry.drain.cancel() }
        w?(nil)
    }

    private func push(_ pair: (JSON, Data, Bool)) {
        lock.lock()
        if let w = waiter {
            waiter = nil
            lock.unlock()
            w(pair)
            return
        }
        queue.append(pair)
        lock.unlock()
    }

    private func startWatch(_ target: AgentId) async {
        guard let remote else { return }
        if withLock({ watchers[target] != nil }) { return }
        let watcherId = AgentId("inmem_relay_\(UUID().uuidString.prefix(8))_\(target.value)")
        // THE READER REGISTERS FIRST: `watch` refuses a watcher with no inbox.
        let stream = remote.ensureInbox(watcherId)
        KernelTaskLocals.$currentSender.withValue(nil) {
            remote.watch(src: target, watcher: watcherId)
        }
        let drain = Task { [weak self] in
            for await item in stream {
                if Task.isCancelled { return }
                // THE ONE CODEC: a payload that carried bytes crosses as a binary
                // frame, `_binary_path` naming where they belong under `payload`.
                // A header with a hole where the body was is not the event python
                // mirrors to a watcher.
                var frame: JSON = .object([
                    "type": .string("event"), "src": .string(target.value),
                    "payload": item.payload,
                ])
                guard let body = item.body else {
                    self?.push((frame, Data(), false))
                    continue
                }
                if case .object(var o) = frame {
                    o["_binary_path"] = Codec.payloadPath(body.parts)
                    frame = .object(o)
                }
                self?.push((frame, body.bytes, true))
            }
        }
        withLock { watchers[target] = (watcherId, drain) }
    }

    private func stopWatch(_ target: AgentId) async {
        guard let entry = withLock({ watchers.removeValue(forKey: target) }) else { return }
        entry.drain.cancel()
        remote?.unwatch(src: target, watcher: entry.watcher)
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
