// io_bridge engine — the transport-agnostic bridge machine.
//
// Python `io_bridge/_engine.py`, function for function: a per-agent state
// holding a transport, a read loop that turns inbound `call`/`reply`/`error`/
// `event` frames into `kernel.send`/`kernel.emit`/pending resolution, and the
// verbs `boot`/`reconnect`/`forward`/`watch_remote`/`unwatch_remote`/`reflect`.
// The ONLY thing that differs per derivation is WHICH transport `boot` builds
// (the `buildTransport` seam) plus a `sentence` + family reflect fields.
//
// Every text a caller reads is owned HERE, not by a transport: a transport
// raises `ConnectionClosed(<message>)` and the verb wraps it
// (`bridge.forward: send failed: <message>`), because that is the one string
// python writes and a transport-flavoured wording diverges on every leg.
//
// Reply correlation: `corr_id = "<bridge_id>-<n>"` — namespaced per agent, so
// two legs in one kernel cannot collide, and the frame `id` echoes it.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Python `io_bridge._transport.ConnectionClosed` — the ONE failure a transport
/// raises. Its `message` is what a verb's `send failed: <e>` embeds verbatim.
public struct ConnectionClosed: Error, CustomStringConvertible, Sendable {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}

/// Python `_BaseTransport`. A frame in, a frame out — no texts, no correlation,
/// no policy: those belong to the engine.
///
/// The binary half is explicit where python's is implicit: python carries raw
/// `bytes` INSIDE the frame dict and the codec finds them, swift's `JSON` has no
/// bytes case, so the body travels beside the frame and `_binary_path` names
/// where it belongs on the wire.
public protocol BridgeTransport: Sendable {
    /// Python `transport.closed` — a leg the engine should stop using.
    func isClosed() async -> Bool
    func send(_ frame: JSON) async throws
    func sendBinary(header: JSON, body: Data) async throws
    /// The next DECODABLE frame, its raw trailing bytes (empty for text) and
    /// whether it ARRIVED as a binary frame — a `read_stream` REQUEST carries no
    /// input bytes yet its reply must, so the body alone cannot decide.
    func recv() async throws -> (frame: JSON, body: Data, isBinary: Bool)
    func close() async
}

/// How a pending forward settled: the remote's `data`, or the engine's own
/// `ConnectionError` text (python `bridge transport closed` / `bridge shut
/// down`), which the verb renders as `bridge.forward: <text>`.
enum PendingOutcome: Sendable {
    case data(JSON, Data)
    case closed(String)
}

/// Python `_BridgeState` — per-agent, process-memory only.
final class BridgeState: @unchecked Sendable {
    private let lock = NSLock()

    var transport: (any BridgeTransport)?
    /// Empty until `boot` succeeds — reflect falls through to the record.
    var transportKind: String = ""
    var readTask: TaskId?
    /// Transport-specific teardown a builder registers (the ssh tunnel).
    var cleanup: (@Sendable () async -> Void)?
    /// Bundle-specific reflect state (the ssh `tunnel_pid`).
    var extra: [String: JSON] = [:]
    /// Per-leg rules, symmetric and independent. Sealed inbound, silent
    /// outbound — boot ALWAYS overwrites them from the record.
    var ingress: IngressRule = IngressRules.DenyInbound()
    var egress: EgressRule = EgressRules.Silent()
    weak var kernel: Kernel?

    private var pending: [String: @Sendable (PendingOutcome?) -> Void] = [:]
    private var corrCounter: Int = 0
    /// In-flight INBOUND dispatches (one task per `call` frame), so the read
    /// loop keeps reading and teardown can cancel them.
    private var calls: [Int: Task<Void, Never>] = [:]
    /// Calls that ended before their task handle was tracked (a fast reply
    /// beats the `Task` initializer returning) — never re-inserted.
    private var finishedCalls: Set<Int> = []
    private var callCounter = 0
    /// Whether the read loop is running, and who is waiting for it to end.
    private var loopRunning = false
    private var loopWaiters: [@Sendable () -> Void] = []

    var pendingCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return pending.count
    }

    /// Python `_next_corr`: DASH, not colon — a ":" would land in the
    /// kernel-minted pid namespace and be refused on the serving side.
    func nextCorr(_ id: AgentId) -> String {
        lock.lock()
        defer { lock.unlock() }
        corrCounter += 1
        return "\(id.value)-\(corrCounter)"
    }

    func park(_ corr: String, _ resume: @escaping @Sendable (PendingOutcome?) -> Void) {
        lock.lock()
        pending[corr] = resume
        lock.unlock()
    }

    /// Remove and return a waiter — EXACTLY ONE of {reply, failAll, timeout}
    /// can take a given key, so the continuation resumes once.
    func take(_ corr: String) -> (@Sendable (PendingOutcome?) -> Void)? {
        lock.lock()
        defer { lock.unlock() }
        return pending.removeValue(forKey: corr)
    }

    /// Python's `for _cid, fut in st.pending: fut.set_exception(ConnectionError(...))`.
    func failAll(_ reason: String) {
        lock.lock()
        let all = pending
        pending.removeAll()
        lock.unlock()
        for (_, resume) in all { resume(.closed(reason)) }
    }

    func nextCallKey() -> Int {
        lock.lock()
        defer { lock.unlock() }
        callCounter += 1
        return callCounter
    }

    func trackCall(_ key: Int, _ task: Task<Void, Never>) {
        lock.lock()
        defer { lock.unlock() }
        if finishedCalls.remove(key) == nil { calls[key] = task }
    }

    func finishCall(_ key: Int) {
        lock.lock()
        defer { lock.unlock() }
        if calls.removeValue(forKey: key) == nil { finishedCalls.insert(key) }
    }

    /// Python awaits the cancelled read task, so its `finally` — cancel the
    /// in-flight inbound calls, emit `bridge_down`, fail every pending forward —
    /// has run before the rest of teardown. Swift cancels a `Task` without
    /// joining it, so teardown waits here instead.
    func loopStarted() {
        lock.lock()
        loopRunning = true
        lock.unlock()
    }

    func loopFinished() {
        lock.lock()
        loopRunning = false
        let waiting = loopWaiters
        loopWaiters = []
        lock.unlock()
        for w in waiting { w() }
    }

    func awaitLoop() async {
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            let waiter = LoopWaiter(c)
            lock.lock()
            if !loopRunning {
                lock.unlock()
                waiter.finish()
                return
            }
            loopWaiters.append { waiter.finish() }
            lock.unlock()
        }
    }

    func cancelCalls() {
        lock.lock()
        let all = calls
        calls.removeAll()
        finishedCalls.removeAll()
        lock.unlock()
        for (_, t) in all { t.cancel() }
    }
}

extension KernelBridgeBundle {

    // MARK: - state

    func state(_ id: AgentId) -> BridgeState {
        stateLock.lock()
        defer { stateLock.unlock() }
        if let s = states[id] { return s }
        let s = BridgeState()
        states[id] = s
        return s
    }

    func existingState(_ id: AgentId) -> BridgeState? {
        stateLock.lock()
        defer { stateLock.unlock() }
        return states[id]
    }

    // MARK: - boot / teardown

    /// Python `io_bridge.boot`: idempotency, the `memory` test kind, the
    /// `build_transport` seam for everything else, then the common tail —
    /// resolve the leg's rules, spawn the read loop, emit `bridge_up`.
    func bootLeg(agentId: AgentId, kernel: Kernel) async -> JSON {
        let st = state(agentId)
        st.kernel = kernel
        if let t = st.transport, await !t.isClosed() {
            return .object([
                "already": .bool(true), "transport": .string(st.transportKind),
            ])
        }
        let meta = kernel.agent(agentId)?.meta ?? [:]
        let kindValue = meta["transport"] ?? .null
        let kind = pyTruthy(kindValue) ? pyStr(kindValue) : "ws"
        let transport: any BridgeTransport
        if kind == "memory" {
            guard let injected = takeInjected(agentId) else {
                return .object([
                    "error": .string("bridge: memory transport requires test injection")
                ])
            }
            transport = injected
        } else {
            do {
                transport = try await buildTransport(
                    kind: kind, agentId: agentId, meta: meta, kernel: kernel, st: st)
            } catch {
                // A partially-registered cleanup (tunnel opened, ws failed) must
                // still run or the leg leaks.
                if let c = st.cleanup {
                    await c()
                    st.cleanup = nil
                }
                return .object(["error": .string("bridge: \(kind) boot failed: \(error)")])
            }
        }
        st.transport = transport
        st.transportKind = kind
        do {
            st.ingress = try resolveIngress(
                ingressRule: meta["ingress_rule"], auth: meta["auth"])
            st.egress = try resolveEgress(egressRule: meta["egress_rule"], auth: meta["auth"])
        } catch {
            // A bad rule must not leave the just-built transport open.
            await teardown(agentId)
            return .object(["error": .string("bridge: bad auth rule: \(error)")])
        }
        // REGISTERED: the read loop outlives this `boot` dispatch and re-emits
        // every inbound frame for the life of the leg.
        st.loopStarted()
        st.readTask = kernel.spawnTask(agent: agentId, verb: "read_loop") { [self] in
            await readLoop(agentId: agentId, kernel: kernel)
            st.loopFinished()
        }
        await kernel.emit(agentId, .object(["type": .string("bridge_up")]))
        return .object(["booted": .bool(true), "transport": .string(kind)])
    }

    /// Python `_teardown`: cancel the read loop, close the transport, run the
    /// transport cleanup, reject pending. Leaves a re-bootable state.
    func teardown(_ agentId: AgentId) async {
        guard let st = existingState(agentId) else { return }
        if let task = st.readTask, let kernel = st.kernel {
            _ = await kernel.cancelTask(task)
        }
        // Closing is what wakes a loop parked in `recv`; then JOIN it, so its
        // tail (`bridge_down`, `bridge transport closed`) has run — python gets
        // that ordering from awaiting the cancelled task.
        await st.transport?.close()
        await st.awaitLoop()
        st.cancelCalls()
        if let c = st.cleanup { await c() }
        st.failAll("bridge shut down")
        st.transport = nil
        st.readTask = nil
        st.cleanup = nil
        st.extra = [:]
        st.ingress = IngressRules.DenyInbound()
        st.egress = EgressRules.Silent()
    }

    // MARK: - read loop

    /// Python `_read_loop`: the long-lived consumer of the transport.
    private func readLoop(agentId: AgentId, kernel: Kernel) async {
        let st = state(agentId)
        guard let transport = st.transport else { return }
        while true {
            let frame: JSON
            let body: Data
            let isBinary: Bool
            do {
                (frame, body, isBinary) = try await transport.recv()
            } catch {
                break
            }
            switch frame["type"].asString {
            case "call":
                // TRACKED TASK, not an inline await: the local handler may itself
                // `forward` back over this bridge, and a loop parked inside
                // `kernel.send` cannot read the frame that would settle it.
                let key = st.nextCallKey()
                let t = Task { [self] in
                    await serveInboundCall(
                        st: st, transport: transport, kernel: kernel, agentId: agentId,
                        frame: frame, body: body, isBinary: isBinary)
                    st.finishCall(key)
                }
                st.trackCall(key, t)
            case "reply":
                if let corr = frame["id"].asString, let resume = st.take(corr) {
                    resume(.data(frame["data"], body))
                }
            case "error":
                if let corr = frame["id"].asString, let resume = st.take(corr) {
                    // `f"remote error: {frame.get('error')}"` — `str()`, so a
                    // missing field reads `None`.
                    resume(
                        .data(
                            .object([
                                "error": .string("remote error: \(pyStr(frame["error"]))")
                            ]), Data()))
                }
            case "event":
                // The `watch_remote` RETURN half: WE dialed out to a chosen peer
                // and asked it to stream, so this re-emit is trusted by
                // association, not a sealed-ingress inbound leak.
                let payload = frame["payload"]
                await kernel.emit(agentId, pyTruthy(payload) ? payload : .object([:]))
            default:
                break  // other frame types are ignored so the loop stays robust
            }
        }
        // The transport is gone — every in-flight inbound dispatch has nowhere
        // to send its reply.
        st.cancelCalls()
        await kernel.emit(agentId, .object(["type": .string("bridge_down")]))
        st.failAll("bridge transport closed")
    }

    /// Python `_serve_inbound_call`: gate, dispatch, ship the reply.
    private func serveInboundCall(
        st: BridgeState, transport: any BridgeTransport, kernel: Kernel, agentId: AgentId,
        frame: JSON, body: Data, isBinary: Bool
    ) async {
        let target = frame["target"]
        let inner = pyTruthy(frame["payload"]) ? frame["payload"] : .object([:])
        // AUTH GATE — the single inbound choke point (python `gate_inbound`).
        // `addr` is the frame's target, else its `src`, else "".
        let addr =
            pyTruthy(target)
            ? pyStr(target) : (pyTruthy(frame["src"]) ? pyStr(frame["src"]) : "")
        let decision = st.ingress.authorize(
            AuthAction(
                kind: "call", target: addr, verb: inner["type"].asString ?? "",
                token: frame["auth_token"].asString))
        var reply: JSON
        var replyBody = Data()
        if case .deny(let denial) = decision {
            // FORWARDED, not re-worded — the rule's own reason and repair, so a
            // peer hitting a sealed leg learns how to open it.
            reply = denial.asRefusal
        } else {
            // UNDER THE LEG'S OWN IDENTITY: a peer frame reaching `kernel.send`
            // with a nil sender would be an OPERATOR call and the two-sided gate
            // would bound nothing.
            let targetId = AgentId(target.asString ?? "")
            let item = Item.fromFrame(targetId, inner, frameId: frame["id"].asString)
            if !isBinary {
                reply = await KernelTaskLocals.$currentSender.withValue(agentId) {
                    await kernel.send([item])[0].result
                }
            } else {
                (reply, replyBody) = await KernelTaskLocals.$currentSender.withValue(agentId) {
                    await kernel.sendWithBinary(targetId, inner, body)
                }
            }
        }
        var out: JSON = .object([
            "type": .string("reply"), "id": frame["id"], "data": reply,
        ])
        // The peer went away mid-dispatch; the read loop already knows.
        if replyBody.isEmpty {
            try? await transport.send(out)
        } else {
            out["_binary_path"] = .string("data.bytes")
            try? await transport.sendBinary(header: out, body: replyBody)
        }
    }

    // MARK: - shared verbs

    /// Python `_forward`.
    func forwardVerb(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON {
        let target = payload["target"]
        let inner = payload["payload"]
        guard pyTruthy(target), case .object = inner else {
            return .object([
                "error": .string("bridge.forward: target (str) + payload (dict) required")
            ])
        }
        let timeout = try pyFloat(payload.asObject?["timeout"] ?? .double(30.0))
        let st = state(agentId)
        guard let transport = st.transport, await !transport.isClosed() else {
            return Refusal.make(
                Refusal.notConnected, "bridge.forward: not connected", hint: "call boot first")
        }
        let corr = st.nextCorr(agentId)
        var frame: JSON = .object([
            "type": .string("call"), "id": .string(corr), "target": target, "payload": inner,
        ])
        // Stamp this leg's EGRESS credential on the ENVELOPE (never the payload).
        if let token = st.egress.credential() { frame["auth_token"] = .string(token) }
        do {
            try await transport.send(frame)
        } catch {
            return .object(["error": .string("bridge.forward: send failed: \(error)")])
        }
        switch await awaitPending(st: st, corr: corr, timeout: timeout) {
        case .data(let data, _): return data
        case .closed(let reason):
            return .object(["error": .string("bridge.forward: \(reason)")])
        case nil:
            return .object([
                "error": .string("bridge.forward: timeout after \(pyFloatRepr(timeout))s")
            ])
        }
    }

    /// `forward` carrying a raw request chunk / returning a raw reply chunk.
    /// Same correlation, same texts; the body rides beside the frame.
    func binaryForwardVerb(
        agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
    ) async throws -> (JSON, Data) {
        let target = header["target"]
        let inner = header["payload"]
        guard pyTruthy(target), case .object = inner else {
            return (
                .object([
                    "error": .string("bridge.forward: target (str) + payload (dict) required")
                ]), Data()
            )
        }
        let timeout = try pyFloat(header.asObject?["timeout"] ?? .double(30.0))
        let st = state(agentId)
        guard let transport = st.transport, await !transport.isClosed() else {
            return (
                Refusal.make(
                    Refusal.notConnected, "bridge.forward: not connected",
                    hint: "call boot first"), Data()
            )
        }
        let corr = st.nextCorr(agentId)
        var frame: JSON = .object([
            "type": .string("call"), "id": .string(corr), "target": target, "payload": inner,
            "_binary_path": .string("payload.bytes"),
        ])
        if let token = st.egress.credential() { frame["auth_token"] = .string(token) }
        do {
            try await transport.sendBinary(header: frame, body: blob)
        } catch {
            return (.object(["error": .string("bridge.forward: send failed: \(error)")]), Data())
        }
        switch await awaitPending(st: st, corr: corr, timeout: timeout) {
        case .data(let data, let body): return (data, body)
        case .closed(let reason):
            return (.object(["error": .string("bridge.forward: \(reason)")]), Data())
        case nil:
            return (
                .object([
                    "error": .string("bridge.forward: timeout after \(pyFloatRepr(timeout))s")
                ]), Data()
            )
        }
    }

    /// `asyncio.wait_for(fut, timeout)`: the outcome, or nil on timeout. The
    /// pending entry is REMOVED by exactly one of {reply, failAll, timeout}, so
    /// the continuation resumes once.
    private func awaitPending(st: BridgeState, corr: String, timeout: Double) async
        -> PendingOutcome?
    {
        await withCheckedContinuation { (c: CheckedContinuation<PendingOutcome?, Never>) in
            let waiter = Waiter(c)
            st.park(corr) { waiter.finish($0) }
            Task {
                try? await Task.sleep(nanoseconds: UInt64(max(0, timeout) * 1_000_000_000))
                if let resume = st.take(corr) { resume(nil) }
            }
        }
    }

    /// Python `_watch_remote` / `_unwatch_remote` — one shape, two frames.
    func watchVerb(
        agentId: AgentId, payload: JSON, verb: String, frameType: String, replyKey: String
    ) async -> JSON {
        guard let target = payload["target"].asString, !target.isEmpty else {
            return .object(["error": .string("bridge.\(verb): target (str) required")])
        }
        let st = state(agentId)
        guard let transport = st.transport, await !transport.isClosed() else {
            return Refusal.make(
                Refusal.notConnected, "bridge.\(verb): not connected", hint: "call boot first")
        }
        do {
            try await transport.send(
                .object(["type": .string(frameType), "src": .string(target)]))
        } catch {
            return .object(["error": .string("bridge.\(verb): send failed: \(error)")])
        }
        var out: JSON = .object(["ok": .bool(true)])
        out[replyKey] = .string(target)
        return out
    }
}

/// Carries the read loop's join continuation.
private final class LoopWaiter: @unchecked Sendable {
    private let continuation: CheckedContinuation<Void, Never>
    init(_ c: CheckedContinuation<Void, Never>) { continuation = c }
    func finish() { continuation.resume() }
}

/// Carries one `CheckedContinuation` across the `pending` table. The table's
/// removal is what makes a resume exclusive; this only gives the closure
/// something Sendable to hold.
private final class Waiter: @unchecked Sendable {
    private let continuation: CheckedContinuation<PendingOutcome?, Never>
    init(_ c: CheckedContinuation<PendingOutcome?, Never>) { continuation = c }
    func finish(_ outcome: PendingOutcome?) { continuation.resume(returning: outcome) }
}
