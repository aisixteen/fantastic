// ws_bridge transport seam — WS (+ ssh-tunnelled WS) to a remote `web_ws`, and
// the `memory` test transport.
//
// Python `ws_bridge/_ws.py`: the bridge is an ASYMMETRIC PURE CLIENT — it dials
// `ws://{host}:{port}/{peer_id}/ws` and ships raw bridge frames
// (`call`/`reply`/`watch`/`event`). The engine + verbs live in `Engine.swift`;
// this file supplies only the transport builders and the WS/tunnel mechanics.
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
                    frame = try JSON.parse(s)
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
            let target = AgentId(frame["target"].asString ?? "")
            // OPERATOR on the remote: the local sender means nothing in the
            // remote's grant space, and admission was already gated by this
            // leg's own ingress rule.
            let reply = await KernelTaskLocals.$currentSender.withValue(nil) {
                await remote.send([Item(target, frame["payload"])])[0].result
            }
            push(
                (
                    .object([
                        "type": .string("reply"), "id": frame["id"], "data": reply,
                    ]), Data(), false
                ))
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
        let (reply, replyBody) = await KernelTaskLocals.$currentSender.withValue(nil) {
            await remote.sendWithBinary(target, header["payload"], body)
        }
        var out: JSON = .object(["type": .string("reply"), "id": header["id"], "data": reply])
        if !replyBody.isEmpty { out["_binary_path"] = .string("data.bytes") }
        push((out, replyBody, true))
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
            for await event in stream {
                if Task.isCancelled { return }
                self?.push((.object(["type": .string("event"), "payload": event]), Data(), false))
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

// MARK: - the ssh tunnel (ssh+ws)

/// Python `_open_tunnel`: `ssh -L local:localhost:remote -N <host>`, no `-f`, so
/// the handle survives for teardown. Returns the running process once the local
/// port accepts a connection.
func openTunnel(host: String, localPort: Int, remotePort: Int) async throws -> Process {
    let proc = Process()
    proc.executableURL = URL(fileURLWithPath: "/usr/bin/ssh")
    proc.arguments = [
        "-N", "-L", "\(localPort):localhost:\(remotePort)",
        "-o", "ExitOnForwardFailure=yes",
        "-o", "ServerAliveInterval=15",
        "-o", "ServerAliveCountMax=3",
        host,
    ]
    proc.standardInput = FileHandle.nullDevice
    proc.standardOutput = FileHandle.nullDevice
    proc.standardError = FileHandle.nullDevice
    do {
        try proc.run()
    } catch {
        throw BridgeConfigError("\(error)")
    }
    let deadline = Date().addingTimeInterval(TUNNEL_READY_TIMEOUT)
    while Date() < deadline {
        if !proc.isRunning {
            throw BridgeConfigError("ssh tunnel exited early (code \(proc.terminationStatus))")
        }
        if tunnelPortAccepts(localPort) { return proc }
        try? await Task.sleep(nanoseconds: 100_000_000)
    }
    proc.terminate()
    throw BridgeConfigError(
        "ssh tunnel to \(host):\(remotePort) not ready in \(pyFloatRepr(TUNNEL_READY_TIMEOUT))s")
}

/// One 200 ms TCP probe of `127.0.0.1:<port>` (python's `socket.connect`).
private func tunnelPortAccepts(_ port: Int) -> Bool {
    let fd = socket(AF_INET, SOCK_STREAM, 0)
    if fd < 0 { return false }
    defer { close(fd) }
    var timeout = timeval(tv_sec: 0, tv_usec: 200_000)
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_port = UInt16(port).bigEndian
    addr.sin_addr.s_addr = inet_addr("127.0.0.1")
    let ok = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0
        }
    }
    return ok
}

/// Python `_kill_tunnel`: SIGTERM the process group, wait 2 s, then SIGKILL.
func killTunnel(_ proc: Process) {
    guard proc.isRunning else { return }
    proc.terminate()
    let deadline = Date().addingTimeInterval(2.0)
    while proc.isRunning && Date() < deadline { usleep(20_000) }
    if proc.isRunning { kill(proc.processIdentifier, SIGKILL) }
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
            throw BridgeConfigError("peer_id required for ws/ssh+ws transports")
        }
        let peerId = pyStr(peerIdValue)
        if kind == "ws" {
            let localPort = meta["local_port"] ?? .null
            let portValue = pyTruthy(localPort) ? localPort : (meta["remote_port"] ?? .null)
            guard pyTruthy(portValue) else {
                throw BridgeConfigError("ws transport requires local_port or remote_port")
            }
            let hostValue = meta["host"] ?? .null
            let host = pyTruthy(hostValue) ? pyStr(hostValue) : "localhost"
            let port = try intArg(portValue)
            guard let url = URL(string: "ws://\(host):\(port)/\(peerId)/ws") else {
                throw BridgeConfigError("ws transport: malformed url")
            }
            return try await WSTransport.connect(url: url)
        }
        if kind == "ssh+ws" {
            let hostValue = meta["host"] ?? .null
            let localPort = try intArg(meta["local_port"] ?? .integer(0), orZero: true)
            let remoteValue = meta["remote_port"] ?? .null
            guard pyTruthy(hostValue), localPort != 0, pyTruthy(remoteValue) else {
                throw BridgeConfigError("ssh+ws requires host, local_port, remote_port")
            }
            let tunnel = try await openTunnel(
                host: pyStr(hostValue), localPort: localPort,
                remotePort: try intArg(remoteValue))
            st.extra["tunnel_pid"] = .integer(Int64(tunnel.processIdentifier))
            // OFF the cooperative pool: `killTunnel` blocks up to 2 s waiting on
            // an ssh that ignores SIGTERM, and a bridge teardown must not freeze
            // every other agent for that interval.
            st.cleanup = { await Task.detached { killTunnel(tunnel) }.value }
            do {
                guard let url = URL(string: "ws://localhost:\(localPort)/\(peerId)/ws") else {
                    throw BridgeConfigError("ssh+ws transport: malformed url")
                }
                return try await WSTransport.connect(url: url)
            } catch {
                killTunnel(tunnel)
                st.cleanup = nil
                st.extra["tunnel_pid"] = nil
                throw error
            }
        }
        throw BridgeConfigError("unknown transport \(pyRepr(kind))")
    }

    /// `int(x)` inside `build_transport`: python raises the bare ValueError text.
    private func intArg(_ v: JSON, orZero: Bool = false) throws -> Int {
        do {
            return orZero ? try pyIntOr(v, 0) : try pyInt(v)
        } catch let e as PyError {
            throw BridgeConfigError(e.message)
        }
    }
}
