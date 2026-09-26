// WebSocket proxy handler (swift-nio).
//
// Replaces the former hand-rolled RFC-6455 framing over NWConnection.
// NIO's WebSocket upgrader (installed in HTTPServer.swift's pipeline)
// owns the handshake (Sec-WebSocket-Accept) + frame codec; this handler
// only carries the PROXY semantics — relaying JSON/binary frames to the
// kernel and streaming watched events back. Cross-platform (Linux too).
//
// Frame protocol (matches python `PB/io/web_ws/src/web_ws/_proxy.py::run`):
//   client → server : {"type":"call",   "target":"<id>", "payload":{...}, "id":"<id>"}
//   client → server : {"type":"emit",   "target":"<id>", "payload":{...}}
//   client → server : {"type":"watch",  "src":"<id>"}
//   client → server : {"type":"unwatch","src":"<id>"}
//   client → server : {"type":"state_subscribe"|"state_unsubscribe"}
//   server → client : {"type":"reply",  "id":"<id>", "data":{...}}
//   server → client : {"type":"error",  "id":"<id>", ...denial}
//   server → client : {"type":"event",  "payload":{...}}   (watcher fanout)
//   server → client : {"type":"state_snapshot"|"state_event", ...}
//
// Each connection auto-watches the URL-path agent (`hostId`) onto a synthetic
// `_ws_<6 hex>` inbox, and honors explicit watch/unwatch onto the SAME inbox
// (parity with python's `_proxy.run`). A single drain task pumps that inbox out
// as `event` frames.
//
// **THE LEG GATE (C41).** Python resolves the leg's ingress rule PER FRAME and
// PER KIND (call/emit/watch/state_subscribe), so opening the edge
// (`update_agent ingress_rule=allow_all_ingress`) takes effect without a
// reconnect, and an ABSENT rule — or an absent leg RECORD — is SEALED. A denied
// `call` is answered (`{error, reason:'unauthorized', hint?, see?}` inside the
// reply's `data`) because discovery-through-denial is the point; every other
// kind is dropped silently, the connect-time auto-watch included — a sealed leg
// that auto-watched leaked the host's whole event stream to anyone who completed
// the handshake (measured: `sealed sock event:output 5`).
//
// **A FRAME IS AN INPUT, NOT A CONTRACT (A11.7).** An undecodable text or binary
// frame is answered with ONE error frame, BEFORE any gate, and the socket is
// KEPT with every in-flight call on it.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import NIOFoundationCompat
import NIOWebSocket
import OrderedCollections

/// Shared WebSocket proxy — installed by the host's pipeline after a
/// `/{host_id}/ws` upgrade. `hostId` is the auto-watched agent; `legId`
/// is the `web_ws` leg whose `ingress_rule` gates every inbound call.
final class WebSocketProxyHandler: ChannelInboundHandler, @unchecked Sendable {
    typealias InboundIn = WebSocketFrame
    typealias OutboundOut = WebSocketFrame

    private let hostId: String
    private let legId: AgentId
    private let kernel: Kernel
    /// The web boot pid every task of this socket is registered under — python
    /// serves the ASGI app inside `CURRENT_TASK.scope(<boot pid>)`
    /// (`web/tools.py`), so `ws_drain` and each `ws_call` are CHILDREN of the
    /// boot task, exactly as a REST request's dispatch is (A11.31). A NIO
    /// upgrade runs on the event loop with no task-local scope at all, so
    /// without this both loops registered as roots and `ps` reported
    /// `parent: null` where python names the pid.
    private let bootParent: TaskId?
    /// One synthetic client id per connection (its inbox is the fanout point).
    private let clientId: AgentId
    /// THIS client's inbox — the queue the kernel's fanout fills. Read by the
    /// drain task AND, for one step, by every `ws_call` task (see `outLock`).
    /// Set in `handlerAdded`, before any frame can arrive.
    private var inbox: Inbox?
    /// ONE ORDER ON THE WIRE (python's). Python fans a payload into this
    /// client's queue BEFORE the target's handler runs and its single loop puts
    /// that payload on the socket before the handler's reply — so a watcher
    /// ALWAYS sees the `event` before the `reply`. Swift's drain is a parallel
    /// task, so the reply overtook the event it must follow. This lock makes
    /// flush-then-reply ONE step: a `ws_call` takes it, forwards everything
    /// already queued for this client, writes its reply, and releases — while
    /// the drain, which holds the same lock across its own forwarding, cannot be
    /// mid-frame. Every write of this connection is issued from inside it, in
    /// the order NIO's event loop then sees. No sleep, no yield.
    ///
    /// **EVERY FRAME, NOT ONLY THE QUEUED ONES.** `writeAndFlush` off the event
    /// loop only ENQUEUES onto the loop, so a frame written straight from
    /// `channelRead` — the frame error, a denial, a watch/state refusal, the
    /// `state_snapshot`, a pong, the close echo, the keepalive ping — ran on the
    /// loop itself and landed BETWEEN the two enqueued writes of a
    /// flush-then-reply step. Python has ONE writer per connection and cannot
    /// produce that interleave. So every outbound frame of this connection is
    /// issued from inside this lock; the critical sections only enqueue, so the
    /// loop never blocks on real IO.
    private let outLock = OutboundLock()
    /// Sources watched onto this client's inbox. Mutated ONLY on the event
    /// loop (handlerAdded / channelRead / teardown all run there).
    private var watching: Set<AgentId> = []
    /// The kernel task ids of the two REGISTERED loops python spawns on the HOST
    /// agent — `ws_drain` (the inbox pump) and one `ws_call` per dispatching
    /// frame. A bare `Task` was in no `ps` row and no `cancel` reached it.
    private var drainTask: TaskId?
    /// In-flight inbound `call` dispatches — kernel task ids, cancelled through
    /// the table on disconnect. Mutated ONLY on the event loop (channelRead +
    /// the completion hop).
    private var callTasks: [TaskId: Bool] = [:]
    /// State-stream subscriptions opened by `state_subscribe` frames on THIS
    /// socket; dropped on `state_unsubscribe` and on close.
    private var stateTokens: [SubscriberToken] = []
    /// uvicorn's keepalive: ping every `ping_interval`, close 1011 when no pong
    /// arrives within `ping_timeout` (both 20 s, `uvicorn/config.py:191-192`).
    static let pingIntervalSeconds: Int64 = 20
    static let pongTimeoutSeconds: Int64 = 20
    private var keepalive: RepeatedTask?
    private var awaitingPong = false

    init(hostId: String, legId: AgentId, kernel: Kernel, bootParent: TaskId?) {
        self.hostId = hostId
        self.legId = legId
        self.kernel = kernel
        self.bootParent = bootParent
        // python `_make_client_id`: `f"_ws_{secrets.token_hex(3)}"` — six hex
        // digits, not a UUID prefix. The id appears in `list_agents`-adjacent
        // reads and in every watcher denial, so its SHAPE is part of the wire.
        self.clientId = AgentId("_ws_\(String(format: "%06x", UInt32.random(in: 0...0xff_ffff)))")
    }

    func handlerAdded(context: ChannelHandlerContext) {
        let channel = context.channel
        let hostAgent = AgentId(hostId)
        let inbox = kernel.ensureInbox(clientId)
        self.inbox = inbox
        // THE CONNECT-TIME AUTO-WATCH IS GATED (C41). Only mirror the host's
        // inbox when THIS leg's rule admits an inbound `watch` with NO
        // credential (an open `allow_all` leg). On a sealed or credentialed leg
        // a client that merely completed the handshake would otherwise receive
        // every event the host sees without ever passing the gate — measured,
        // the sealed swift socket delivered term boot/output/write/stop events
        // python never sent.
        if kernel.agent(hostAgent) != nil,
            gateWebLeg(
                kernel: kernel, legId: legId, kind: "watch", target: hostId, verb: "watch",
                token: nil) == nil
        {
            _ = KernelTaskLocals.$currentSender.withValue(legId) {
                kernel.watch(src: hostAgent, watcher: clientId)
            }
            watching.insert(hostAgent)
        }
        // Drain the inbox → `{type:"event", payload}` text frames. REGISTERED as
        // the kernel task `ws_drain` on the HOST agent (python
        // `spawn_task(host_agent_id, "ws_drain", …)`), so an open socket is a
        // `ps` row and its pump is cancellable like any other work.
        //
        // Forward what is QUEUED while holding `outLock`, then wait for more
        // with it RELEASED — a drain that waited under the lock would block
        // every reply forever.
        let outLock = self.outLock
        drainTask = KernelTaskLocals.$currentTask.withValue(bootParent) {
            kernel.spawnTask(agent: hostAgent, verb: "ws_drain", keepalive: true) {
                while true {
                    outLock.sync { Self.flushEvents(inbox: inbox, channel: channel) }
                    if await inbox.ready() == false { return }
                }
            }
        }
        // SERVER keepalive — uvicorn pings every 20 s and closes 1011
        // `keepalive ping timeout` when the pong does not come back in time.
        // Swift answered client pings and ignored pongs, but never pinged, so a
        // half-open socket was never detected.
        keepalive = context.eventLoop.scheduleRepeatedTask(
            initialDelay: .seconds(Self.pingIntervalSeconds),
            delay: .seconds(Self.pongTimeoutSeconds)
        ) { [weak self] _ in
            guard let self = self else { return }
            if self.awaitingPong {
                var reason = channel.allocator.buffer(capacity: 24)
                reason.writeInteger(UInt16(1011))
                reason.writeString("keepalive ping timeout")
                let close = WebSocketFrame(fin: true, opcode: .connectionClose, data: reason)
                self.outLock.sync {
                    channel.writeAndFlush(close).whenComplete { _ in channel.close(promise: nil) }
                }
                return
            }
            self.awaitingPong = true
            let ping = WebSocketFrame(
                fin: true, opcode: .ping, data: channel.allocator.buffer(capacity: 0))
            self.outLock.sync { channel.writeAndFlush(ping, promise: nil) }
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
        keepalive?.cancel()
        keepalive = nil
        // python's `finally`, in its order: cancel the in-flight calls, unwatch
        // every src, drop the state subscriptions, then the drain.
        //
        // Cancel every in-flight dispatch THROUGH THE TABLE, so `onCancel` runs
        // as for any cancel. A `call` can run for minutes (an LLM stream);
        // untracked, a client that closed its tab left it generating — billing
        // tokens — into a dead channel.
        let calls = Array(callTasks.keys)
        callTasks.removeAll()
        let drain = drainTask
        drainTask = nil
        let srcs = watching
        watching.removeAll()
        let tokens = stateTokens
        stateTokens.removeAll()
        for t in tokens { kernel.unsubscribe(t) }
        for src in srcs {
            kernel.unwatch(src: src, watcher: clientId)
        }
        let kernel = self.kernel
        let clientId = self.clientId
        Task {
            for t in calls { _ = await kernel.cancelTask(t) }
            kernel.removeInbox(clientId)
            if let drain { _ = await kernel.cancelTask(drain) }
        }
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let frame = unwrapInboundIn(data)
        let channel = context.channel
        switch frame.opcode {
        case .text:
            handleFrame(.text(String(buffer: frame.unmaskedData)), channel: channel)
        case .binary:
            var payload = frame.unmaskedData
            let bytes = payload.readData(length: payload.readableBytes) ?? Data()
            handleFrame(.binary(bytes), channel: channel)
        case .ping:
            let pong = WebSocketFrame(fin: true, opcode: .pong, data: frame.unmaskedData)
            outLock.sync { context.writeAndFlush(self.wrapOutboundOut(pong), promise: nil) }
        case .connectionClose:
            // ECHO the client's close payload — python answers 1000 with the
            // code it received; an empty buffer read as 1005 on the client.
            let close = WebSocketFrame(
                fin: true, opcode: .connectionClose, data: frame.unmaskedData)
            outLock.sync {
                context.writeAndFlush(self.wrapOutboundOut(close)).whenComplete { _ in
                    context.close(promise: nil)
                }
            }
        case .pong:
            awaitingPong = false
        case .continuation:
            break  // fragmentation not used by the JS client
        default:
            break
        }
    }

    // MARK: - frame dispatch

    /// A raw inbound WS message, before decoding.
    enum InboundRaw {
        case text(String)
        case binary(Data)
    }

    /// A decoded inbound frame: the envelope, plus the raw body when the frame
    /// carried one at `payload.bytes`.
    private struct Inbound {
        let envelope: JSON
        /// `.some` only when `_binary_path` named the payload's own `bytes`
        /// field — the one path this runtime's carrier (`Item.binary`) can
        /// place.
        let body: Data?
    }

    /// The path an inbound binary frame must name for its body to reach the
    /// target: the payload's own `bytes` field, which is where python's
    /// `set_path` puts it before `kernel.send`.
    private static let inboundBinaryPath = "payload.bytes"

    /// Where a binary reply's chunk sits in the reply ENVELOPE — python's
    /// `find_bytes_path` walks `{type:reply, id, data:{…bytes…}}` and finds
    /// exactly this.
    private static let replyBinaryPath = "data.bytes"

    /// The verbs a TEXT `call` frame must route down the BINARY channel, chosen
    /// STATICALLY by verb (A11.9/A13.1).
    ///
    /// Python needs no such list: its payload dict simply holds a `bytes` value
    /// and `encode_outbound` finds it. Swift's `JSON` cannot, so the carrier is
    /// the item's binary channel and the decision has to be made from the verb —
    /// the same pair on both wire legs, so a browser and a curl client get the
    /// same answer for the same request.
    static let binaryEdgeVerbs: Set<String> = ["read_stream", "write_stream"]

    /// python `decode_inbound` + `isinstance(frame, dict)`: parse a text frame
    /// as JSON or a binary frame as `[4B len | header | body]`, pop
    /// `_binary_path` and place the body there. `nil` is the ONE
    /// malformed-frame answer (A11.7/A13.15).
    private static func decodeInbound(_ raw: InboundRaw) -> Inbound? {
        var envelope: JSON
        var body: Data?
        switch raw {
        case .text(let t):
            guard let parsed = try? JSON.parse(t) else { return nil }
            envelope = parsed
            body = nil
        case .binary(let b):
            guard let (h, blob) = try? Codec.decodeBinaryFrame(b) else { return nil }
            envelope = h
            body = blob
        }
        guard case .object(var members) = envelope else { return nil }
        // `head.pop("_binary_path", None)`: a JSON null IS python's "absent".
        let rawPath = members["_binary_path"]
        members.removeValue(forKey: "_binary_path")
        envelope = .object(members)
        guard let rawPath, !rawPath.isNull else {
            // No path ⇒ a plain envelope; a body that came with one is dropped,
            // exactly as python drops it (nothing names where it would go).
            return Inbound(envelope: envelope, body: nil)
        }
        // A non-string path raises out of python's `path.split(".")`.
        guard let path = rawPath.asString else { return nil }
        // VALIDATE THE PLACEMENT — python actually performs `set_path(head,
        // path, body)` and a `KeyError`/`IndexError`/`TypeError` becomes the
        // frame error. Swift cannot store the bytes in the `JSON`, so it places
        // a `null` in the same slot and keeps the body beside the envelope.
        do {
            try Codec.setPath(&envelope, path, .null)
        } catch {
            return nil
        }
        // Only the payload's own `bytes` field reaches a handler here: that is
        // the slot `Item.binary` fills. Any other placeable path keeps its
        // `null` and the body is dropped.
        return Inbound(
            envelope: envelope, body: path == inboundBinaryPath ? (body ?? Data()) : nil)
    }

    /// python `bad("frame", "frame", "a well-formed call/emit/watch envelope")`
    /// under a `type: error` head — the ONE answer to every undecodable frame,
    /// sent BEFORE any gate (a sealed leg answers it too) and with the socket
    /// KEPT. Key order: type, error, reason, arg.
    private static func frameErrorFrame() -> JSON {
        var out: OrderedDictionary<String, JSON> = ["type": .string("error")]
        if case .object(let bad) = Refusal.bad(
            "frame", "frame", "a well-formed call/emit/watch envelope")
        {
            for (k, v) in bad { out[k] = v }
        }
        return .object(out)
    }

    /// python `{"type": "error", "id": <frame id or "">, **denial}` — the
    /// denial's own keys are FLATTENED onto the frame, not wrapped in `data`.
    private static func errorFrame(id: JSON, denial: JSON) -> JSON {
        var out: OrderedDictionary<String, JSON> = ["type": .string("error"), "id": id]
        if case .object(let d) = denial {
            for (k, v) in d { out[k] = v }
        }
        return .object(out)
    }

    /// Spawn one inbound dispatch as a REGISTERED kernel task on the HOST agent,
    /// under the LEG'S OWN IDENTITY.
    ///
    /// Two things used to be missing. (1) `Task { ... }` does not inherit a
    /// task-local, so `KernelTaskLocals.currentSender` was nil inside and
    /// `BatchSend.launch` skipped the two-sided gate entirely — every browser
    /// call ran as the OPERATOR, bounded only by the leg's ingress rule. (2) the
    /// task was invisible to `ps` and unreachable by `cancel`. Both are closed
    /// by going through here; callers never build a bare `Task`.
    ///
    /// Must be called ON the event loop (channelRead is).
    private func trackCall(channel: Channel, _ body: @escaping @Sendable () async -> Void) {
        let legId = self.legId
        // The id is only known AFTER `spawnTask` returns, and the closure needs
        // it to drop its own entry — so it travels through a holder rather than
        // a `var` the concurrent closure would capture.
        let slot = TaskIdSlot()
        // Under the web boot pid, like `ws_drain` and every REST request task.
        let id = KernelTaskLocals.$currentTask.withValue(bootParent) {
            kernel.spawnTask(agent: AgentId(hostId), verb: "ws_call") { [weak self] in
                await KernelTaskLocals.$currentSender.withValue(legId) {
                    await body()
                }
                // Rebind before the hop: a captured `weak var self` is a mutable
                // capture, which is not Sendable. The handler itself is
                // (unchecked).
                let handler = self
                let done = slot.value
                channel.eventLoop.execute {
                    if let done { handler?.callTasks[done] = nil }
                }
            }
        }
        slot.value = id
        callTasks[id] = true
    }

    /// The ONE inbound path — text and binary frames decode the same way and
    /// route by `type` the same way (A13.15). A binary `call` rides
    /// `kernel.send` with the body as the item's binary, never `sendWithBinary`
    /// (which skips the ledger, the dedup and the reflect flags).
    private func handleFrame(_ raw: InboundRaw, channel: Channel) {
        guard let inbound = Self.decodeInbound(raw) else {
            outLock.sync {
                Self.writeText(channel: channel, text: Codec.encodeText(Self.frameErrorFrame()))
            }
            return
        }
        let frame = inbound.envelope
        // python `frame.get("target", "")` — the URL host is NEVER the default.
        let target = frame["target"].asString ?? ""
        // python `frame.get("payload", {})` — an ABSENT payload is `{}`, on
        // `call` and on `emit` alike (a `null` payload made every watcher see
        // `{"type":"event","payload":null}`).
        let payload = frame.asObject?["payload"] ?? .object([:])
        // python `frame.get("id", "")` — kept RAW (any JSON type) and always
        // echoed; only a non-empty STRING names the execution.
        let fid = frame.asObject?["id"] ?? .string("")
        let token = frame["auth_token"].asString
        let kernel = self.kernel
        let legId = self.legId
        // python `frame.get("target") or frame.get("src") or ""` — a falsy
        // target falls through to `src`, which is what a watch frame carries.
        let addr =
            pyTruthy(frame["target"])
            ? (frame["target"].asString ?? "") : (frame["src"].asString ?? "")

        switch frame["type"].asString {
        case "call":
            if let denied = gateWebLeg(
                kernel: kernel, legId: legId, kind: "call", target: addr,
                verb: payload["type"].asString ?? "", token: token)
            {
                // TEACHING DENIAL: the caller's promise resolves with the reason
                // plus the pointer to the readme that says how to open the edge.
                outLock.sync { Self.writeReply(channel: channel, id: fid, data: denied) }
                return
            }
            let body = inbound.body
            // The queue this dispatch's own fanout fills, and the lock that
            // keeps the flush and the reply one step.
            guard let inbox = self.inbox else { return }
            let outLock = self.outLock
            trackCall(channel: channel) {
                // THE FRAME ID IS THE EXECUTION ID. One id space: the same id
                // arriving twice — a double-click, a second tab, a reconnect
                // replaying its queue — is ONE intent, deduped and joined by the
                // ledger instead of running the side effect twice.
                var item = Item(AgentId(target), payload)
                if let s = fid.asString, !s.isEmpty { item.id = s }
                // A binary-edge verb rides the BINARY channel even when the
                // client sent a text frame: that is the only way the bundle
                // returns (or receives) a raw chunk (A11.9/A13.1).
                let verb = payload["type"].asString ?? ""
                if body != nil || Self.binaryEdgeVerbs.contains(verb) {
                    // python's `payload[field] = body` OVERWRITES whatever the
                    // client put there — but ONLY when a body actually arrived
                    // (`_binary_path` named the slot). A TEXT frame carries no
                    // body, so python leaves the client's JSON `bytes` value in
                    // the payload and `file_bridge` types it: `write_stream`
                    // with `bytes:"abc"` is `missing_arg`, nothing written
                    // (A13.19). Stripping the key unconditionally turned that
                    // refusal into a silent 0-byte write.
                    if body != nil, case .object(var p) = item.payload {
                        p.removeValue(forKey: "bytes")
                        item.payload = .object(p)
                    }
                    item.binary = body ?? Data()
                }
                let out = await kernel.send([item])[0]
                // Verb-level errors travel inside `data.error` (python's wire
                // shape); a `type:"error"` frame is reserved for out-of-band
                // failures.
                let env: JSON = .object([
                    "type": .string("reply"), "id": fid, "data": out.result,
                ])
                // PYTHON'S ORDER, ENFORCED: the watch events this dispatch
                // produced are already queued (the kernel fans out BEFORE the
                // handler), so forward them here — under the drain's own lock,
                // so the drain cannot be mid-frame — and only then write the
                // reply. One order, one wire.
                outLock.sync {
                    Self.flushEvents(inbox: inbox, channel: channel)
                    Self.sendEnvelope(
                        channel: channel, envelope: env, body: out.body,
                        path: Self.replyBinaryPath)
                }
            }
        case "emit":
            if gateWebLeg(
                kernel: kernel, legId: legId, kind: "emit", target: addr, verb: "emit",
                token: token) != nil
            {
                return  // a denied emit is DROPPED, never answered
            }
            let item: InboxItem
            if let b = inbound.body {
                var head = payload
                if case .object(var o) = head {
                    o["bytes"] = .null
                    head = .object(o)
                }
                item = .binary(head, "bytes", b)
            } else {
                item = .text(payload)
            }
            Task {
                // Same identity discipline as `call` — an emit dispatched with a
                // nil sender is an OPERATOR emit.
                await KernelTaskLocals.$currentSender.withValue(legId) {
                    await kernel.emitItem(AgentId(target), item)
                }
            }
        case "watch":
            if gateWebLeg(
                kernel: kernel, legId: legId, kind: "watch", target: addr, verb: "watch",
                token: token) != nil
            {
                return
            }
            guard let src = frame["src"].asString, !src.isEmpty else { return }
            let s = AgentId(src)
            if watching.contains(s) { return }
            // UNDER THE LEG'S OWN IDENTITY, so the kernel's gate sees a real
            // caller. Without this `currentSender` is nil, the kernel reads the
            // frame as an OPERATOR watch, and the two-sided gate never runs —
            // which is exactly how a leg granted only `assistant:[send]` could
            // mirror any agent's inbox and read the model's prompt out of it.
            // THE LEG, not the host agent: the ingress rule and the `can_call`
            // grants both hang off the `web_ws` record.
            let denial = KernelTaskLocals.$currentSender.withValue(legId) {
                kernel.watch(src: s, watcher: clientId)
            }
            if let d = denial {
                outLock.sync {
                    Self.writeText(
                        channel: channel,
                        text: Codec.encodeText(Self.errorFrame(id: fid, denial: d)))
                }
                return
            }
            watching.insert(s)
        case "unwatch":
            // NO GATE — python gates the four inbound actions and leaves
            // `unwatch` (a client giving something up) ungated.
            guard let src = frame["src"].asString, !src.isEmpty else { return }
            let s = AgentId(src)
            if watching.contains(s) {
                kernel.unwatch(src: s, watcher: clientId)
                watching.remove(s)
            }
        case "state_subscribe":
            // A SEALED LEG LEAKS NO TELEMETRY, not merely no dispatch.
            if gateWebLeg(
                kernel: kernel, legId: legId, kind: "state_subscribe", target: addr,
                verb: "state_subscribe", token: token) != nil
            {
                return
            }
            // The state stream is a KERNEL-WIDE read: every send's sender,
            // target, verb and payload summary, for every agent in the tree. So
            // it is gated on the ROOT — an ingress rule answers "may this client
            // reach this leg", never "what may it READ".
            let denial = KernelTaskLocals.$currentSender.withValue(legId) {
                kernel.authorizeStateSubscribe(legId)
            }
            if let d = denial {
                outLock.sync {
                    Self.writeText(
                        channel: channel,
                        text: Codec.encodeText(Self.errorFrame(id: fid, denial: d)))
                }
                return
            }
            // python parity: FIRST a `state_snapshot` carrying every agent's
            // identity so a state-watching view can bootstrap before the first
            // event, THEN the `state_event` stream.
            outLock.sync {
                Self.writeText(
                    channel: channel,
                    text: Codec.encodeText(
                        .object([
                            "type": .string("state_snapshot"),
                            "agents": .array(kernel.stateSnapshot()),
                        ])))
            }
            // THE SAME ONE WRITER (python pushes state frames onto the single
            // outbound queue): a state event written from the emitting thread
            // could otherwise land between two events of one flush, or between a
            // flush and the reply it belongs with.
            let outLock = self.outLock
            // Conversation rows are not on this stream — an AI agent announces
            // its own rows to its watchers (`watch` on that agent, gated there).
            let token = kernel.subscribe {
                [weak kernel] (event: JSON, body: InboxBody?) in
                guard case .object(let ev) = event else { return }
                // `type` FIRST, then the event's own keys — python's
                // `{"type": "state_event", **event}`, which is the byte order a
                // frame diff reads.
                var out: OrderedDictionary<String, JSON> = ["type": .string("state_event")]
                for (k, v) in ev { out[k] = v }
                // Lazy `name` fill — python derives it from the agent record
                // when the event lacks one, so the browser always renders one
                // shape. An agent already gone falls back to its id rather than
                // dropping the key.
                if out["name"] == nil, let aid = ev["agent_id"]?.asString {
                    let display = kernel?.agent(AgentId(aid))?.displayName
                    out["name"] = .string(
                        (display?.isEmpty == false ? display : nil) ?? aid)
                }
                outLock.sync {
                    Self.sendEnvelope(
                        channel: channel, envelope: .object(out), body: body?.data,
                        path: body.map { "payload.\($0.path)" } ?? "")
                }
            }
            stateTokens.append(token)
        case "state_unsubscribe":
            let tokens = stateTokens
            stateTokens.removeAll()
            for t in tokens { kernel.unsubscribe(t) }
        default:
            break  // unknown frame types are silently dropped (forward-compat)
        }
    }

    // MARK: - writes (channel is Sendable; writeAndFlush hops to the loop)

    /// Forward every payload ALREADY QUEUED for this client, oldest first.
    /// Non-blocking: it stops at the first empty take, never waits. The caller
    /// holds `outLock`, so no other writer of this connection can interleave.
    static func flushEvents(inbox: Inbox, channel: Channel) {
        while let item = inbox.tryTake() {
            writeEvent(item: item, channel: channel)
        }
    }

    /// One queued payload → one `{type:"event", payload}` frame.
    ///
    /// A QUEUED PAYLOAD HOLDING BYTES IS A BINARY FRAME — python's
    /// `encode_outbound` finds the bytes inside the payload dict and sends
    /// `[4B len | head with _binary_path | body]`. The swift carrier keeps the
    /// body beside the payload, so the path is the item's own path nested under
    /// this envelope's `payload`.
    static func writeEvent(item: InboxItem, channel: Channel) {
        let frame: JSON = .object(["type": .string("event"), "payload": item.payload])
        guard let body = item.body else {
            writeText(channel: channel, text: Codec.encodeText(frame))
            return
        }
        var head = frame
        if case .object(var o) = head {
            o["_binary_path"] = .string("payload.\(body.path)")
            head = .object(o)
        }
        writeBinary(channel: channel, bytes: Codec.encodeBinaryFrame(header: head, body: body.data))
    }

    /// `{type:"reply", id, data}` — the frame `id` is echoed RAW, whatever JSON
    /// type it arrived as (A11.8); an absent one is `""`.
    private static func writeReply(channel: Channel, id: JSON, data: JSON) {
        let frame: JSON = .object(["type": .string("reply"), "id": id, "data": data])
        writeText(channel: channel, text: Codec.encodeText(frame))
    }

    /// Send one server→client envelope, picking the frame kind from the
    /// CARRIER (C37/A11.9): a reply that HOLDS a chunk — a 0-byte one included —
    /// is a binary frame whose head keeps `bytes: null` in its python position
    /// and names it in `_binary_path`. Testing the body's LENGTH instead served
    /// an empty file as JSON.
    private static func sendEnvelope(
        channel: Channel, envelope: JSON, body: Data?, path: String
    ) {
        guard let body else {
            writeText(channel: channel, text: Codec.encodeText(envelope))
            return
        }
        var head = envelope
        if case .object(var o) = head {
            o["_binary_path"] = .string(path)
            head = .object(o)
        }
        writeBinary(channel: channel, bytes: Codec.encodeBinaryFrame(header: head, body: body))
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

/// The ONE write order of a connection. Its critical section is SYNCHRONOUS —
/// `NSLock.lock()` is `noasync`, and every frame under it is a plain
/// `writeAndFlush`, which only hands the frame to the channel's event loop.
final class OutboundLock: @unchecked Sendable {
    private let lock = NSLock()

    func sync<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }
}

/// One `TaskId`, written by the spawner and read by the task it spawned.
final class TaskIdSlot: @unchecked Sendable {
    var value: TaskId?
}
