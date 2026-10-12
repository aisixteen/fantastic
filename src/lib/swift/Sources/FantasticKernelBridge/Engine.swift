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
// Reply correlation: `corr_id = "<bridge_id>-<boot nonce>-<n>"` — the bridge id
// keeps bridges apart, the nonce (minted at every boot) keeps BOOTS apart, and
// the frame `id` echoes it so logs correlate across hops. The frame id NAMES the
// execution on the receiver, so an id reused by a restarted sender would join a
// settled execution there and never run (audit IO-5, 2026-10-03).
//
// A leg's rules (`ingress_rule`/`egress_rule`/`auth`) are read off the LIVE
// record per frame — never cached at boot — so what `reflect` reports is what the
// gate enforces the moment `update_agent` writes it (audit ARCH-2, 2026-10-03).

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

/// How a pending forward settled: the remote's `data` (with the reply's chunk at
/// its slot of that `data` when it ARRIVED as a binary frame — a 0-byte chunk
/// included, C37; `nil` for a text reply), or the engine's own `ConnectionError`
/// text (python `bridge transport closed` / `bridge shut down`), which the verb
/// renders as `bridge.forward: <text>`.
enum PendingOutcome: Sendable {
    case data(JSON, Blob?)
    case closed(String)
}

/// Python `_BridgeState` — per-agent, process-memory only.
final class BridgeState: @unchecked Sendable {
    private let lock = NSLock()

    var transport: (any BridgeTransport)?
    /// Empty until `boot` succeeds — reflect falls through to the record.
    var transportKind: String = ""
    var readTask: TaskId?
    /// Transport-specific teardown a builder registers.
    var cleanup: (@Sendable () async -> Void)?
    /// Bundle-specific reflect state.
    var extra: [String: JSON] = [:]
    /// The remote agents this leg SUBSCRIBED to (`watch_remote`). An inbound
    /// `event` frame is re-emitted only when its `src` is here — nothing flows
    /// until subscribed, and nothing after an unwatch.
    private var watching: Set<String> = []
    weak var kernel: Kernel?

    private var pending: [String: @Sendable (PendingOutcome?) -> Void] = [:]
    /// `<bridge id>-<nonce>`, minted at every boot, and the count under it: the
    /// frame ids of this boot's forwards (`nextCorr`).
    private var corrPrefix = ""
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

    /// Python `_new_corr_prefix`: `<bridge id>-<12 hex>`, a fresh nonce per
    /// BOOT (ts's `mintPrefix` width). The counter under it restarts at every
    /// boot, so without the nonce a restarted sender's `<leg>-1` named an
    /// execution the receiver had already settled — the new call was joined to
    /// it and never ran (audit IO-5).
    static func newCorrPrefix(_ id: AgentId) -> String {
        var nonce = ""
        for _ in 0..<6 { nonce += String(format: "%02x", UInt8.random(in: 0...255)) }
        return "\(id.value)-\(nonce)"
    }

    /// A boot's correlator: a fresh prefix, the counter back at 0.
    func resetCorr(_ id: AgentId) {
        lock.withLock {
            corrPrefix = Self.newCorrPrefix(id)
            corrCounter = 0
        }
    }

    /// Python `_next_corr`: DASH, not colon — a ":" would land in the
    /// kernel-minted pid namespace and be refused on the serving side.
    /// Bridge-minted correlators are caller-chosen ids like any other — fresh
    /// per forward AND per boot, so each forward is its own intent.
    func nextCorr() -> String {
        lock.lock()
        defer { lock.unlock() }
        corrCounter += 1
        return "\(corrPrefix)-\(corrCounter)"
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
    func watch(_ src: String) { lock.lock(); watching.insert(src); lock.unlock() }
    func unwatch(_ src: String) { lock.lock(); watching.remove(src); lock.unlock() }
    func clearWatching() { lock.lock(); watching.removeAll(); lock.unlock() }
    func isWatching(_ src: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return watching.contains(src)
    }

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
    /// `build_transport` seam for everything else, then the common tail — mint
    /// this boot's correlator nonce, spawn the read loop, emit `bridge_up`. The
    /// leg's rules are NOT cached here: the gate and `forward` read them off the
    /// live record per frame; boot only refuses an egress rule the registry does
    /// not know, before dialing.
    func bootLeg(agentId: AgentId, kernel: Kernel) async -> JSON {
        let st = state(agentId)
        st.kernel = kernel
        if let t = st.transport, await !t.isClosed() {
            return .object([
                "already": .bool(true), "transport": .string(st.transportKind),
            ])
        }
        let meta = kernel.agent(agentId)?.meta ?? [:]
        do {
            _ = try resolveEgress(egressRule: meta["egress_rule"], auth: meta["auth"])
        } catch {
            return .object(["error": .string("bridge: bad auth rule: \(error)")])
        }
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
        st.resetCorr(agentId)
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
        st.clearWatching()
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
                    // THE CARRIER, not the length: a binary reply holds a chunk
                    // (an empty one included), a text reply holds none. The chunk
                    // keeps its slot in the reply (`["data", …p]` on the frame is
                    // `p` in `data`); a body the frame put outside `data` is not
                    // part of the reply.
                    resume(
                        .data(
                            frame["data"],
                            isBinary ? Codec.bodyUnder("data", frame, body: body) : nil))
                }
            case "error":
                if let corr = frame["id"].asString, let resume = st.take(corr) {
                    // `f"remote error: {frame.get('error')}"` — `str()`, so a
                    // missing field reads `None`.
                    resume(
                        .data(
                            .object([
                                "error": .string("remote error: \(pyStr(frame["error"]))")
                            ]), nil))
                }
            case "event":
                // The `watch_remote` RETURN half: re-emitted on the bridge's OWN
                // inbox for local watchers ONLY when its `src` is a remote agent
                // this leg subscribed to. An event nobody asked for is dropped.
                guard let src = frame["src"].asString, st.isWatching(src),
                    case .object(var obj) = frame["payload"]
                else { continue }
                obj["src"] = .string(src)
                let inner: JSON = .object(obj)
                // A BINARY event re-emits WHOLE. The envelope's `_binary_path` is
                // `["payload", …steps]`; the inbox carrier wants the steps INSIDE
                // the payload, so the body reaches a local watcher as the frame
                // the remote sent rather than a head with a hole in it.
                if isBinary, let blob = Codec.payloadBody(frame, body: body) {
                    kernel.emitItem(agentId, .binary(inner, blob))
                } else {
                    await kernel.emit(agentId, inner)
                }
            default:
                break  // other frame types are ignored so the loop stays robust
            }
        }
        // The transport is gone — every in-flight inbound dispatch has nowhere
        // to send its reply.
        st.cancelCalls()
        st.clearWatching()
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
        // The leg's INGRESS rule — read off the LIVE record, per frame, as every
        // web/file/exec leg does (audit ARCH-2). An unresolvable rule seals.
        let meta = kernel.agent(agentId)?.meta ?? [:]
        let ingress = resolveIngress(ingressRule: meta["ingress_rule"], auth: meta["auth"])
        let decision = ingress.authorize(
            AuthAction(
                kind: "call", target: addr, verb: inner["type"].asString ?? "",
                token: frame["auth_token"].asString))
        var reply: JSON
        var replyBody: Blob? = nil
        if case .deny(let denial) = decision {
            // FORWARDED, not re-worded — the rule's own reason and repair, so a
            // peer hitting a sealed leg learns how to open it.
            reply = denial.asRefusal
        } else {
            // AS THE LEG: the read loop is a task this leg's `boot` spawned, so
            // this task is the leg's too — the inbound call is sent as the leg and
            // gated as the leg (its `can_call`, the target's `callable_by`).
            // Whoever called on the other kernel is that kernel's business; here
            // the caller is the leg.
            let targetId = AgentId(target.asString ?? "")
            // ONE ITEM, text or binary: a binary frame's body sits at
            // `["payload", …steps]` (the frame's `_binary_path`) and rides the item's
            // binary channel AT THAT PATH of the payload, and the frame id names
            // the execution either way — a replayed binary call joins the recorded
            // outcome instead of running again. A body the frame put anywhere else
            // is not in the payload python's decoder hands the kernel.
            var built = Item.fromFrame(targetId, inner, frameId: frame["id"].asString)
            if isBinary {
                let (p, binary) = Codec.callPayload(frame, body: body)
                built.payload = pyTruthy(p) ? p : .object([:])
                built.binary = binary
            }
            // ANY verb's reply may hold raw bytes, whatever frame asked for it
            // (python's reply dict holds them; no list of verbs): a call with no
            // request body still carries its reply's body back.
            if built.binary == nil { built.rawReply = true }
            let out = await kernel.send([built])[0]
            (reply, replyBody) = (out.result, out.body)
        }
        let out: JSON = .object([
            "type": .string("reply"), "id": frame["id"], "data": reply,
        ])
        // THE CARRIER picks the frame kind, never the body's length: a 0-byte
        // chunk is still a chunk (C37). The chunk rides raw at `["data", …its
        // slot]` (`["data", "bytes"]` for a `read_stream` chunk, `["data", "x.y"]`
        // for a reply key `"x.y"`). The peer went away mid-dispatch; the read
        // loop already knows.
        guard let replyBody else {
            try? await transport.send(out)
            return
        }
        let at = replyBody.nested(in: "data")
        try? await transport.sendBinary(header: Codec.binaryHead(out, at), body: at.bytes)
    }

    // MARK: - shared verbs

    /// The leg's EGRESS rule as its record says NOW (python
    /// `resolve_egress(kernel.get(id))` per forward). Throws for a rule the
    /// registry does not know.
    private func liveEgress(_ agentId: AgentId, _ kernel: Kernel) throws -> EgressRule {
        let meta = kernel.agent(agentId)?.meta ?? [:]
        return try resolveEgress(egressRule: meta["egress_rule"], auth: meta["auth"])
    }

    /// Python `_forward`.
    func forwardVerb(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON {
        let target = payload["target"]
        let inner = payload["payload"]
        guard pyTruthy(target), case .object = inner else {
            return .object([
                "error": .string("bridge.forward: target (str) + payload (dict) required")
            ])
        }
        let timeout = try pyFloat(payload.asObject?["forward_timeout"] ?? .double(30.0))
        let st = state(agentId)
        guard let transport = st.transport, await !transport.isClosed() else {
            return Refusal.make(
                Refusal.notConnected, "bridge.forward: not connected", hint: "call boot first")
        }
        // The leg's EGRESS rule, off the live record per forward (never a
        // boot-time copy): an operator's `update_agent egress_rule=…` stamps the
        // next frame (audit ARCH-2).
        let egress: EgressRule
        do { egress = try liveEgress(agentId, kernel) } catch {
            return .object(["error": .string("bridge: bad auth rule: \(error)")])
        }
        let corr = st.nextCorr()
        var frame: JSON = .object([
            "type": .string("call"), "id": .string(corr), "target": target, "payload": inner,
        ])
        // Stamp this leg's EGRESS credential on the ENVELOPE (never the payload).
        if let token = egress.credential() { frame["auth_token"] = .string(token) }
        let slot = expect(st: st, corr: corr)
        do {
            try await transport.send(frame)
        } catch {
            _ = st.take(corr)
            return .object(["error": .string("bridge.forward: send failed: \(error)")])
        }
        switch await awaitPending(st: st, corr: corr, slot: slot, timeout: timeout) {
        case .data(let data, _): return data
        case .closed(let reason):
            return .object(["error": .string("bridge.forward: \(reason)")])
        case nil:
            return .object([
                "error": .string("bridge.forward: timeout after \(pyFloatRepr(timeout))s")
            ])
        }
    }

    /// `forward` carrying a raw request chunk / returning a raw reply chunk —
    /// the binary channel. Same correlation, same texts. The local caller sends
    /// `{type:forward, target, payload, forward_timeout?}` with the body INSIDE the inner
    /// payload (`Blob(parts: ["payload", …steps], …)` — python's forward payload
    /// holds the bytes there); the bridge ships a binary `call` frame with the body
    /// at `["payload", …steps]`, or a TEXT one when the call carries no body (a raw
    /// read). A body anywhere else is not part of the forwarded call, exactly as
    /// python's inner payload would not hold it. The reply's chunk comes back at
    /// its slot of the reply when the remote answered a binary frame — `nil` for a
    /// text reply (C37).
    func binaryForwardVerb(
        agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
    ) async throws -> (JSON, Blob?) {
        let target = header["target"]
        guard pyTruthy(target), case .object = header["payload"] else {
            return (
                .object([
                    "error": .string("bridge.forward: target (str) + payload (dict) required")
                ]), nil
            )
        }
        let timeout = try pyFloat(header.asObject?["forward_timeout"] ?? .double(30.0))
        let st = state(agentId)
        guard let transport = st.transport, await !transport.isClosed() else {
            return (
                Refusal.make(
                    Refusal.notConnected, "bridge.forward: not connected",
                    hint: "call boot first"), nil
            )
        }
        let carried: Blob? = body.flatMap {
            $0.parts.count > 1 && $0.parts[0] == "payload"
                ? Blob(parts: Array($0.parts.dropFirst()), $0.bytes) : nil
        }
        // The body's slot in the inner payload is `null` on the wire, in python's
        // position (`encode_frame` nulls the bytes where they sat); the kernel
        // admitted the path at launch.
        var inner = header["payload"]
        if let carried { try? pySetPath(&inner, parts: carried.parts, .null) }
        let egress: EgressRule
        do { egress = try liveEgress(agentId, kernel) } catch {
            return (.object(["error": .string("bridge: bad auth rule: \(error)")]), nil)
        }
        let corr = st.nextCorr()
        var frame: JSON = .object([
            "type": .string("call"), "id": .string(corr), "target": target, "payload": inner,
        ])
        if let token = egress.credential() { frame["auth_token"] = .string(token) }
        let slot = expect(st: st, corr: corr)
        do {
            if let carried {
                // The request body rides raw at `["payload", …its steps]` (a
                // python peer reinserts it there; the inner payload's slot stays
                // null on the wire).
                frame["_binary_path"] = Codec.payloadPath(carried.parts)
                try await transport.sendBinary(header: frame, body: carried.bytes)
            } else {
                try await transport.send(frame)
            }
        } catch {
            _ = st.take(corr)
            return (.object(["error": .string("bridge.forward: send failed: \(error)")]), nil)
        }
        switch await awaitPending(st: st, corr: corr, slot: slot, timeout: timeout) {
        case .data(let data, let body): return (data, body)
        case .closed(let reason):
            return (.object(["error": .string("bridge.forward: \(reason)")]), nil)
        case nil:
            return (
                .object([
                    "error": .string("bridge.forward: timeout after \(pyFloatRepr(timeout))s")
                ]), nil
            )
        }
    }

    /// Register `corr`'s waiter BEFORE its frame is sent — python creates the
    /// future and files it in `pending` first. A reply can beat the send's return
    /// (a loopback `read_stream` answers in microseconds), and a reply that finds
    /// no waiter is dropped: the caller then waited out the whole timeout and got
    /// no body (measured: `wsBinaryForwardStreamsRawBytesOverWire`, 30 s and 0
    /// bytes, 1 run in 80). The slot holds an outcome that lands before anyone
    /// awaits it.
    private func expect(st: BridgeState, corr: String) -> PendingSlot {
        let slot = PendingSlot()
        st.park(corr) { slot.finish($0) }
        return slot
    }

    /// `asyncio.wait_for(fut, timeout)`: the outcome, or nil on timeout. The
    /// pending entry is REMOVED by exactly one of {reply, failAll, timeout}, so
    /// the slot is finished once.
    private func awaitPending(
        st: BridgeState, corr: String, slot: PendingSlot, timeout: Double
    ) async -> PendingOutcome? {
        Task {
            try? await Task.sleep(nanoseconds: UInt64(max(0, timeout) * 1_000_000_000))
            if let resume = st.take(corr) { resume(nil) }
        }
        return await slot.value()
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
        if frameType == "unwatch" { st.unwatch(target) }
        do {
            try await transport.send(
                .object(["type": .string(frameType), "src": .string(target)]))
        } catch {
            return .object(["error": .string("bridge.\(verb): send failed: \(error)")])
        }
        if frameType == "watch" { st.watch(target) }
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
/// One call's outcome slot, registered before the send: `finish` may run before
/// or after `value()` starts waiting, and either order delivers exactly once.
private final class PendingSlot: @unchecked Sendable {
    private let lock = NSLock()
    private var outcome: PendingOutcome??
    private var continuation: CheckedContinuation<PendingOutcome?, Never>?

    func finish(_ o: PendingOutcome?) {
        lock.lock()
        if let c = continuation {
            continuation = nil
            lock.unlock()
            c.resume(returning: o)
            return
        }
        outcome = .some(o)
        lock.unlock()
    }

    func value() async -> PendingOutcome? {
        await withCheckedContinuation { (c: CheckedContinuation<PendingOutcome?, Never>) in
            lock.lock()
            if let o = outcome {
                lock.unlock()
                c.resume(returning: o)
                return
            }
            continuation = c
            lock.unlock()
        }
    }
}
