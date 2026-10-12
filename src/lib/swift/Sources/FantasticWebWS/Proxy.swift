// WebSocket proxy handler (swift-nio) — python `web_ws/_proxy.py`, and like it
// it lives in the `web_ws` leg, not in the host: the leg hands this handler to
// its parent `web` as the LIVE endpoint of its declared `/{host_id}/ws` route
// (`WebWSBundle.routeEndpoints`, python `route_endpoints(agent)`).
//
// NIO's WebSocket upgrader (installed in the host's pipeline) owns the
// handshake (Sec-WebSocket-Accept) + frame codec; this handler only carries the
// PROXY semantics — relaying JSON/binary frames to the kernel and streaming
// watched events back. Cross-platform (Linux too).
//
// Frame protocol (matches python `PB/io/web_ws/src/web_ws/_proxy.py::run`):
//   client → server : {"type":"call",   "target":"<id>", "payload":{...}, "id":"<id>"}
//                     (the leg sends it → reply)
//   client → server : {"type":"emit",   "target":"<id>", "payload":{...}}  (the leg emits it)
//   client → server : {"type":"watch",  "src":"<id>"}
//   client → server : {"type":"unwatch","src":"<id>"}
//   client → server : {"type":"state_subscribe"|"state_unsubscribe"}
//   server → client : {"type":"reply",  "id":"<id>", "data":{...}}
//   server → client : {"type":"error",  "id":"<id>", ...denial}
//   server → client : {"type":"event",  "src":…, "payload":{...}}  (a watched agent;
//                     `src` = the name the client watched)
//                     {"type":"event",  "payload":{...}}   (emitted to this connection)
//   server → client : {"type":"state_snapshot"|"state_event", ...}
//   client → server : {"type":"hello",  "guid":"<page kernel guid>"}   (the page as a peer)
//   server → client : {"type":"call",   "id":"h<n>", "target", "payload"}
//   client → server : {"type":"reply"|"error", "id":"h<n>", "data":{...}}
//   server → client : {"type":"emit",   "target", "payload"}  (a host emit to
//                     `<guid>|<agent>` — to EVERY live connection of that guid)
//   server → client : {"type":"reply_mirror", "id", "data"}  (the reply a SIBLING
//                     connection of this hello guid got — never a call of this one)
//
// **THE PAGE AS A PEER KERNEL** (owner, 2026-09-30). A client that is itself a
// kernel says `hello {guid}`; this leg then registers that guid with the host
// kernel, which routes `send`s to `<guid>|<agent>` over this socket as outbound
// `call` frames answered by inbound `reply`/`error` frames with the same id. One
// guid per connection (pinned by the first hello). When the socket closes the
// guid stops being an address and every call still waiting on it settles as a
// `transport_error`. This leg only carries the frames — the host kernel routes and
// gates the sender, the page kernel gates its own agents.
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
//
// **IDENTITY: THIS HANDLER IS THE TRANSPORT** (owner 2026-10-04). Its frame lane
// runs outside every dispatch, so what it sends is the operator's. It sends
// nothing onward itself: every admitted frame is handed to the leg
// (`Inbound.handIn` → the leg's `inbound_*` verbs), and the leg performs it as
// itself — gated as the leg. The lane is python's read loop: frames are handled
// one at a time, in arrival order; an emit, a watch and a state subscription are
// awaited before the next frame is read, a call is spawned and runs on.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import NIOCore
import NIOFoundationCompat
import NIOWebSocket
import OrderedCollections

/// The WebSocket proxy — the endpoint the host installs after an upgrade onto
/// the leg's `/{host_id}/ws` route. `hostId` is the auto-watched agent; `legId`
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
    /// The connection's FRAME LANE — python's read loop: every inbound frame is
    /// handled here, one at a time, in arrival order, outside every dispatch.
    private let lane = FrameLane()
    /// Guards the connection state below: the lane and the event loop (teardown)
    /// both touch it.
    private let state = NSLock()
    /// Set by `teardown`: a watch or a subscription that completes after the
    /// socket is gone gives itself back instead of registering.
    private var closed = false
    /// The name the client watched → the RESOLVED agent id actually watched.
    /// Only a watch that REALLY registered is recorded (see `watchPath`).
    /// Guarded by `state`.
    private var watching: OrderedDictionary<String, WatchEntry> = [:]
    /// EVERY READER THIS SOCKET DRAINS — its own inbox (events emitted to the
    /// connection, no `src`) and ONE PER WATCH (so each mirrored event goes out
    /// stamped with the `src` it came from; a bridge on the other end re-emits
    /// only what it subscribed to). A reply flushes them ALL first.
    private let readers = ReaderSet()
    /// The kernel task ids of the two REGISTERED loops python spawns on the HOST
    /// agent — `ws_drain` (the inbox pump) and one `ws_call` per dispatching
    /// frame. A bare `Task` was in no `ps` row and no `cancel` reached it.
    private var drainTask: TaskId?
    /// In-flight inbound `call` dispatches — kernel task ids, cancelled through
    /// the table on disconnect. Guarded by `state`.
    private var callTasks: [TaskId: Bool] = [:]
    /// State-stream subscriptions opened by `state_subscribe` frames on THIS
    /// socket; dropped on `state_unsubscribe` and on close. Guarded by `state`.
    private var stateTokens: [SubscriberToken] = []
    /// uvicorn's keepalive: ping every `ping_interval`, close 1011 when no pong
    /// arrives within `ping_timeout` (both 20 s, `uvicorn/config.py:191-192`).
    static let pingIntervalSeconds: Int64 = 20
    static let pongTimeoutSeconds: Int64 = 20
    private var keepalive: RepeatedTask?
    private var awaitingPong = false
    /// THE PEER HALF: the guid the first accepted `hello` pinned and the route
    /// registered for it. Guarded by `state`.
    private var pinned: (guid: String, route: PeerRoute)?
    /// The calls the host made INTO the page, waiting for their answers —
    /// shared with the route, which runs on kernel tasks, hence its own lock.
    private let peerCalls = PeerCalls()

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
        readers.add(nil, inbox)
        // The frame lane, outside every dispatch (a NIO event loop carries no
        // task-local), parented to the web boot pid like every task of this
        // socket.
        lane.start(parent: bootParent)
        // THE CONNECT-TIME AUTO-WATCH IS GATED (C41). Only mirror the host's
        // inbox when THIS leg's rule admits an inbound `watch` with NO
        // credential (an open `allow_all` leg). On a sealed or credentialed leg
        // a client that merely completed the handshake would otherwise receive
        // every event the host sees without ever passing the gate — measured,
        // the sealed swift socket delivered term boot/output/write/stop events
        // python never sent. It goes through THE ONE WATCH PATH, so the `kernel` address
        // resolves and a refusal is an `error` frame, never silence.
        if gateWebLeg(
            kernel: kernel, legId: legId, kind: "watch", target: hostId, verb: "watch",
            token: nil) == nil
        {
            let hostId = self.hostId
            lane.enqueue { [self] in
                await self.watchPath(hostId, frameId: .string(""), channel: channel)
            }
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
                    outLock.sync { Self.flushEvents(inbox: inbox, channel: channel, src: nil) }
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
        lane.finish()
        // The page is gone: it is no longer an address, and every call still
        // waiting on it settles NOW with the reason — never a hang, never a retry.
        // Idempotent: `channelInactive` and `handlerRemoved` both land here.
        let (p, calls, watched, tokens): (
            (guid: String, route: PeerRoute)?, [TaskId], [(String, WatchEntry)],
            [SubscriberToken]
        ) = state.withLock {
            closed = true
            let p = pinned
            pinned = nil
            let calls = Array(callTasks.keys)
            callTasks.removeAll()
            let watched = Array(watching).map { ($0.key, $0.value) }
            watching.removeAll()
            let tokens = stateTokens
            stateTokens.removeAll()
            return (p, calls, watched, tokens)
        }
        let guid = p?.guid
        if let p {
            kernel.unregisterPeer(p.guid, p.route)
            Tabs.shared.leave(tabKey(p.guid), ObjectIdentifier(self))
        }
        peerCalls.close(guid: guid)
        // python's `finally`, in its order: cancel the in-flight calls, unwatch
        // every src, drop the state subscriptions, then the drain.
        //
        // Cancel every in-flight dispatch THROUGH THE TABLE, so `onCancel` runs
        // as for any cancel. A `call` can run for minutes (an LLM stream);
        // untracked, a client that closed its tab left it generating — billing
        // tokens — into a dead channel.
        let drain = drainTask
        drainTask = nil
        for t in tokens { kernel.unsubscribe(t) }
        for (src, entry) in watched { dropWatch(src, entry) }
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
            let raw = InboundRaw.text(String(buffer: frame.unmaskedData))
            lane.enqueue { [self] in await self.handleFrame(raw, channel: channel) }
        case .binary:
            var payload = frame.unmaskedData
            let bytes = payload.readData(length: payload.readableBytes) ?? Data()
            lane.enqueue { [self] in await self.handleFrame(.binary(bytes), channel: channel) }
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
            break  // never arrives: the host reassembles fragments (`WebSocketMessageGuard`)
        default:
            break
        }
    }

    // MARK: - frame dispatch

    /// A raw inbound WS message, before decoding.
    enum InboundRaw: Sendable {
        case text(String)
        case binary(Data)
    }

    /// A decoded inbound frame: the envelope, plus the raw body when the frame
    /// carried one under its `payload` or its `data`.
    private struct DecodedFrame {
        let envelope: JSON
        /// `.some` when `_binary_path` named a slot under the frame's `payload` —
        /// the body AT its steps in the payload (`Item.binary` carries both).
        let body: Blob?
        /// `.some` when `_binary_path` named a slot under an ANSWER's `data` — a
        /// page replying to one of the host's calls with raw bytes, AT its steps
        /// in the reply (`["data", "x.y"]` is the reply key `"x.y"`).
        var replyBody: Blob? = nil
    }

    /// python `decode_inbound` + `isinstance(frame, dict)`: parse a text frame
    /// as JSON or a binary frame as `[4B len | header | body]`, pop
    /// `_binary_path` and place the body there. `nil` is the ONE
    /// malformed-frame answer (A11.7/A13.15).
    private static func decodeInbound(_ raw: InboundRaw) -> DecodedFrame? {
        let b: Data
        switch raw {
        case .text(let t):
            // A TEXT frame is `loads(text)` and nothing more (python
            // `decode_frame` on a str): a `_binary_path` in it is an inert key,
            // never a slot to null or a body to place.
            guard let parsed = try? JSON.parseStrict(t), case .object = parsed else { return nil }
            return DecodedFrame(envelope: parsed, body: nil)
        case .binary(let bytes):
            b = bytes
        }
        guard let (head, body) = try? Codec.decodeBinaryFrame(b) else { return nil }
        guard case .object = head else { return nil }
        // `head.pop("_binary_path", None)` + `set_path(head, path, body)`: swift
        // cannot store the bytes in the `JSON`, so `null` takes the slot and the
        // body rides beside the envelope. A path python cannot place is the
        // frame error (`decodeBinaryFrame` already refused it).
        var envelope = head
        let placed: [PathStep]?
        do {
            placed = try Codec.placeBody(&envelope)
        } catch {
            return nil
        }
        guard let path = placed else {
            // No path ⇒ a plain envelope; a body that came with one is dropped,
            // exactly as python drops it (nothing names where it would go).
            return DecodedFrame(envelope: envelope, body: nil)
        }
        // A body under the frame's `payload` reaches the handler AT ITS STEPS
        // there (`Item.binary` carries both). A body under an answer's `data` is
        // the chunk of a reply to the host's own call, at its slot of that reply.
        // Any other placeable path keeps its `null` and the body is dropped.
        let blob = Blob(parts: path, body)
        if let inPayload = blob.under("payload") {
            return DecodedFrame(envelope: envelope, body: inPayload)
        }
        return DecodedFrame(envelope: envelope, body: nil, replyBody: blob.under("data"))
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

    /// THE ONE WATCH PATH — the connect-time auto-watch and an explicit `watch`
    /// frame both come here. FAIL LOUD: a name that resolves to no agent, or a
    /// gate refusal, is answered with an `error` frame and NOT recorded as
    /// watched. (The connect-time watch used to discard the result and mark the
    /// name watched anyway; a name the kernel's `watch` did not resolve mirrored
    /// nothing, said nothing, and the explicit watch that followed was then
    /// skipped as a duplicate.)
    ///
    /// The name resolves the way `send` resolves a target (`kernel` or a literal
    /// id). THE LEG WATCHES (`inbound_watch`), so the kernel's gate sees a real
    /// caller — an ungated watch is how a leg granted only `assistant:[send]`
    /// could mirror any agent's inbox and read the model's prompt out of it. THE
    /// LEG, not the host agent: the ingress rule and the `can_call` grants both
    /// hang off the `web_ws` record. Python `_proxy._watch`. Runs on the lane.
    private func watchPath(_ src: String, frameId: JSON, channel: Channel) async {
        if src.isEmpty || state.withLock({ watching[src] != nil }) { return }
        guard let target = kernel.resolveAgent(src) else {
            let r = Refusal.make(
                Refusal.unknownAgent,
                "watch: no agent \(pyRepr(src)) (an id, or `kernel`)",
                extra: [("src", .string(src))])
            outLock.sync {
                Self.writeText(
                    channel: channel, text: Codec.encodeText(Self.errorFrame(id: frameId, denial: r)))
            }
            return
        }
        // THIS WATCH'S OWN READER — registered before the watch asks for one.
        let reader = AgentId("\(clientId.value)>\(target.id.value)")
        let rInbox = kernel.ensureInbox(reader)
        let out = await Inbound.handIn(
            kernel, legId: legId,
            .object([
                "type": .string("inbound_watch"), "src": .string(target.id.value),
                "reader": .string(reader.value),
            ]))
        if pyTruthy(out.result["error"]) {
            kernel.removeInbox(reader)
            outLock.sync {
                Self.writeText(
                    channel: channel,
                    text: Codec.encodeText(Self.errorFrame(id: frameId, denial: out.result)))
            }
            return
        }
        let outLock = self.outLock
        let task = KernelTaskLocals.$currentTask.withValue(bootParent) {
            kernel.spawnTask(agent: AgentId(hostId), verb: "ws_drain", keepalive: true) {
                while true {
                    outLock.sync { Self.flushEvents(inbox: rInbox, channel: channel, src: src) }
                    if await rInbox.ready() == false { return }
                }
            }
        }
        let entry = WatchEntry(target: target.id, reader: reader, task: task)
        // The socket may have closed while the leg was asked: then the watch is
        // given straight back instead of registered.
        let kept: Bool = state.withLock {
            if closed { return false }
            watching[src] = entry
            return true
        }
        if kept {
            readers.add(src, rInbox)
        } else {
            dropWatch(src, entry)
        }
    }

    /// Give a watch back: unwatch, stop its drain, DROP ITS INBOX, forget the reader.
    private func dropWatch(_ src: String, _ entry: WatchEntry) {
        kernel.unwatch(src: entry.target, watcher: entry.reader)
        readers.remove(src)
        let kernel = self.kernel
        Task {
            _ = await kernel.cancelTask(entry.task)
            kernel.removeInbox(entry.reader)
        }
    }

    private static func errorFrame(id: JSON, denial: JSON) -> JSON {
        var out: OrderedDictionary<String, JSON> = ["type": .string("error"), "id": id]
        if case .object(let d) = denial {
            for (k, v) in d { out[k] = v }
        }
        return .object(out)
    }

    /// Spawn one inbound call as a REGISTERED kernel task on the HOST agent, so
    /// it is in `ps` and reachable by `cancel` (callers never build a bare
    /// `Task`). Spawned from the lane — outside every dispatch, so its hand-in to
    /// the leg is the operator's and the leg's onward call is the leg's.
    private func trackCall(channel: Channel, _ body: @escaping @Sendable () async -> Void) {
        // The id is only known AFTER `spawnTask` returns, and the closure needs
        // it to drop its own entry — so it travels through a holder rather than
        // a `var` the concurrent closure would capture.
        let slot = TaskIdSlot()
        // Under the web boot pid, like `ws_drain` and every REST request task.
        let id = KernelTaskLocals.$currentTask.withValue(bootParent) {
            kernel.spawnTask(agent: AgentId(hostId), verb: "ws_call") { [weak self] in
                await body()
                if let handler = self, let done = slot.value {
                    handler.state.withLock { _ = handler.callTasks.removeValue(forKey: done) }
                }
            }
        }
        slot.value = id
        state.withLock { callTasks[id] = true }
    }

    /// The ONE inbound path — text and binary frames decode the same way and
    /// route by `type` the same way (A13.15). A binary `call` is handed to the
    /// leg with the body at its slot of the item's payload — the one send, with
    /// its ledger, dedup and reflect flags. Runs on the lane.
    private func handleFrame(_ raw: InboundRaw, channel: Channel) async {
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
            guard self.inbox != nil else { return }
            let readers = self.readers
            let outLock = self.outLock
            // The identity this connection said hello as — its sibling tabs see
            // the reply as a `reply_mirror`.
            let tabs = state.withLock { pinned.map { tabKey($0.guid) } }
            let me = ObjectIdentifier(self)
            trackCall(channel: channel) {
                // THE FRAME ID IS THE EXECUTION ID. One id space: the same id
                // arriving twice — a double-click, a second tab, a reconnect
                // replaying its queue — is ONE intent, deduped and joined by the
                // ledger instead of running the side effect twice.
                var item: OrderedDictionary<String, JSON> = [
                    "target": .string(target), "payload": payload,
                ]
                if let s = fid.asString, !s.isEmpty { item["id"] = .string(s) }
                // HANDED TO THE LEG (`inbound_call`), which sends it AS ITSELF.
                //
                // EVERY CALL MAY BE ANSWERED WITH RAW BYTES, whatever its verb:
                // python's reply dict holds them and `encode_frame` finds them,
                // with no list of verbs. A text frame is a call with no request
                // body whose reply body comes back raw; the reply goes out binary
                // exactly when it holds one.
                //
                // ONLY A BINARY FRAME OVERWRITES THE KEY. python's
                // `payload[field] = body` runs where a body EXISTS; a TEXT
                // frame's `bytes` reaches the target as the JSON value the
                // client sent, and `write_stream` answers `missing_arg` for it
                // (an ABSENT key stays an empty chunk) (A13.19). The decode left
                // `null` in the slot python's `set_path` fills — kept, so the
                // call row and the watcher frame keep python's key order.
                let out = await Inbound.handIn(
                    kernel, legId: legId,
                    .object(["type": .string("inbound_call"), "item": .object(item)]),
                    binary: body.map {
                        Blob(parts: [.key("item"), .key("payload")] + $0.parts, $0.bytes)
                    })
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
                    for (src, rInbox) in readers.snapshot() {
                        Self.flushEvents(inbox: rInbox, channel: channel, src: src)
                    }
                    Self.sendEnvelope(
                        channel: channel, envelope: env, body: out.body?.nested(in: "data"))
                }
                // REPLY MIRRORING: every OTHER live connection of this hello guid
                // gets the reply too — the tabs of one identity are one principal.
                // After this connection's lock is released: one lock at a time.
                // TODO (owner, open): DESYNC between the tabs of one identity is
                // envisioned and unsolved — a mirror is not a merge; think later.
                if let tabs {
                    for mirror in Tabs.shared.siblings(tabs, except: me) {
                        mirror(fid, out.result, out.body)
                    }
                }
            }
        case "emit":
            if gateWebLeg(
                kernel: kernel, legId: legId, kind: "emit", target: addr, verb: "emit",
                token: token) != nil
            {
                return  // a denied emit is DROPPED, never answered
            }
            // An emit has no reply channel: a malformed one is dropped.
            guard !target.isEmpty, case .object = payload else { return }
            // HANDED TO THE LEG (`inbound_emit`), which emits it AS ITSELF. The
            // body rides at ITS path; the decode already left `null` in that slot
            // of `payload`, in python's position. AWAITED before the next frame
            // is read: python awaits the hand-in in the read loop, so two emits —
            // or an emit and the call after it — reach their watchers in the
            // order they were sent.
            _ = await Inbound.handIn(
                kernel, legId: legId,
                .object([
                    "type": .string("inbound_emit"), "target": .string(target),
                    "payload": payload,
                ]),
                binary: inbound.body.map { $0.nested(in: "payload") })
        case "watch":
            if gateWebLeg(
                kernel: kernel, legId: legId, kind: "watch", target: addr, verb: "watch",
                token: token) != nil
            {
                return
            }
            await watchPath(frame["src"].asString ?? "", frameId: fid, channel: channel)
        case "unwatch":
            // NO GATE — python gates the four inbound actions and leaves
            // `unwatch` (a client giving something up) ungated.
            guard let src = frame["src"].asString, !src.isEmpty else { return }
            if let entry = state.withLock({ watching.removeValue(forKey: src) }) {
                dropWatch(src, entry)
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
            // reach this leg", never "what may it READ". THE LEG ASKS
            // (`inbound_state_subscribe`) — the lane's own context is the
            // operator's, which the gate never judges (audit ARCH2-7).
            let asked = await Inbound.handIn(
                kernel, legId: legId, .object(["type": .string("inbound_state_subscribe")]))
            if pyTruthy(asked.result["error"]) {
                outLock.sync {
                    Self.writeText(
                        channel: channel,
                        text: Codec.encodeText(Self.errorFrame(id: fid, denial: asked.result)))
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
                [weak kernel] (event: JSON, body: Blob?) in
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
                        channel: channel, envelope: .object(out),
                        body: body?.nested(in: "payload"))
                }
            }
            // A socket that closed meanwhile drops the tap at once.
            let kept: Bool = state.withLock {
                if closed { return false }
                stateTokens.append(token)
                return true
            }
            if !kept { kernel.unsubscribe(token) }
        case "state_unsubscribe":
            let tokens: [SubscriberToken] = state.withLock {
                let t = stateTokens
                stateTokens.removeAll()
                return t
            }
            for t in tokens { kernel.unsubscribe(t) }
        case "hello":
            hello(frame, frameId: fid, token: token, addr: addr, channel: channel)
        case "reply", "error":
            peerCalls.answer(frame, body: inbound.replyBody)
        default:
            break  // unknown frame types are silently dropped (forward-compat)
        }
    }

    // MARK: - the peer half: calls the host makes INTO the page

    /// `hello {guid}` — make this connection the page kernel's address. Python
    /// `_on_hello`: the guid is checked first (a malformed one is answered even on
    /// a sealed leg), then the leg's gate (a sealed leg carries no peer, and says
    /// nothing — the calls it refuses teach), then the pin. A guid already live on
    /// another connection is another tab of that identity: this route JOINS it
    /// (one principal, kernel `registerPeer`). Runs on the lane.
    private func hello(
        _ frame: JSON, frameId: JSON, token: String?, addr: String, channel: Channel
    ) {
        guard let guid = frame["guid"].asString, !guid.isEmpty, !guid.contains("|") else {
            answerError(
                Refusal.bad("hello", "guid", "the page kernel's guid, a non-empty id without `|`"),
                frameId: frameId, channel: channel)
            return
        }
        if gateWebLeg(
            kernel: kernel, legId: legId, kind: "hello", target: addr, verb: "hello",
            token: token) != nil
        {
            return
        }
        if let pinned = state.withLock({ self.pinned }) {
            if pinned.guid != guid {
                answerError(
                    Refusal.bad(
                        "hello", "guid", "\(pinned.guid) (pinned by this connection's first hello)"),
                    frameId: frameId, channel: channel)
            }
            return
        }
        let calls = peerCalls
        let outLock = self.outLock
        let route = PeerRoute(
            via: legId.value,
            call: { agentId, payload, body in
                await calls.call(
                    guid: guid, agentId: agentId, payload: payload, body: body,
                    channel: channel, outLock: outLock)
            },
            emit: { agentId, payload, body in
                calls.emit(
                    agentId: agentId, payload: payload, body: body, channel: channel,
                    outLock: outLock)
            })
        kernel.registerPeer(guid, route)
        state.withLock { pinned = (guid, route) }
        Tabs.shared.join(tabKey(guid), ObjectIdentifier(self)) { id, data, body in
            calls.mirror(id: id, data: data, body: body, channel: channel, outLock: outLock)
        }
    }

    /// This leg's tab table key for `guid`.
    private func tabKey(_ guid: String) -> Tabs.Key {
        Tabs.Key(kernel: ObjectIdentifier(kernel), leg: legId.value, guid: guid)
    }

    /// python `{"type": "error", "id": frame.get("id", ""), **refusal}`.
    private func answerError(_ refusal: JSON, frameId: JSON, channel: Channel) {
        outLock.sync {
            Self.writeText(
                channel: channel,
                text: Codec.encodeText(Self.errorFrame(id: frameId, denial: refusal)))
        }
    }

    // MARK: - writes (channel is Sendable; writeAndFlush hops to the loop)

    /// Forward every payload ALREADY QUEUED for this client, oldest first.
    /// Non-blocking: it stops at the first empty take, never waits. The caller
    /// holds `outLock`, so no other writer of this connection can interleave.
    static func flushEvents(inbox: Inbox, channel: Channel, src: String?) {
        while let item = inbox.tryTake() {
            writeEvent(item: item, channel: channel, src: src)
        }
    }

    /// One queued payload → one `{type:"event", payload}` frame.
    ///
    /// A QUEUED PAYLOAD HOLDING BYTES IS A BINARY FRAME — python's
    /// `encode_outbound` finds the bytes inside the payload dict and sends
    /// `[4B len | head with _binary_path | body]`. The swift carrier keeps the
    /// body beside the payload, so the path is the item's own path nested under
    /// this envelope's `payload`.
    static func writeEvent(item: InboxItem, channel: Channel, src: String?) {
        let frame: JSON =
            src.map {
                .object(["type": .string("event"), "src": .string($0), "payload": item.payload])
            } ?? .object(["type": .string("event"), "payload": item.payload])
        guard let body = item.body else {
            writeText(channel: channel, text: Codec.encodeText(frame))
            return
        }
        var head = frame
        if case .object(var o) = head {
            o["_binary_path"] = Codec.payloadPath(body.parts)
            head = .object(o)
        }
        writeBinary(channel: channel, bytes: Codec.encodeBinaryFrame(header: head, body: body.bytes))
    }

    /// `{type:"reply", id, data}` — the frame `id` is echoed RAW, whatever JSON
    /// type it arrived as (A11.8); an absent one is `""`.
    private static func writeReply(channel: Channel, id: JSON, data: JSON) {
        let frame: JSON = .object(["type": .string("reply"), "id": id, "data": data])
        writeText(channel: channel, text: Codec.encodeText(frame))
    }

    /// Send one server→client envelope, picking the frame kind from the
    /// CARRIER (C37/A11.9): an envelope that HOLDS a chunk — a 0-byte one
    /// included — is a binary frame whose head keeps `null` at the chunk's slot
    /// (`body.parts`, from the envelope root: `["data", "x.y"]` for a reply's
    /// chunk at the reply key `"x.y"`) in its python position — appended when the
    /// envelope did not name it — and names it in `_binary_path`. Testing the
    /// body's LENGTH instead served an empty file as JSON.
    static func sendEnvelope(channel: Channel, envelope: JSON, body: Blob?) {
        guard let body else {
            writeText(channel: channel, text: Codec.encodeText(envelope))
            return
        }
        writeBinary(
            channel: channel,
            bytes: Codec.encodeBinaryFrame(
                header: Codec.binaryHead(envelope, body), body: body.bytes))
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

/// One registered watch — python's `watching[src] = (aid, reader, drain)`.
struct WatchEntry {
    let target: AgentId
    let reader: AgentId
    let task: TaskId
}

/// The readers a socket drains, shared with its `ws_call` tasks (which flush them
/// all before a reply) — hence the lock.
final class ReaderSet: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [(String?, Inbox)] = []
    func add(_ src: String?, _ inbox: Inbox) {
        lock.lock()
        items.append((src, inbox))
        lock.unlock()
    }
    func remove(_ src: String) {
        lock.lock()
        items.removeAll { $0.0 == src }
        lock.unlock()
    }
    func snapshot() -> [(String?, Inbox)] {
        lock.lock()
        defer { lock.unlock() }
        return items
    }
}

/// The outbound calls of ONE connection — python `_call_page` / `_on_answer` and
/// the disconnect settle in `run`'s `finally`. The route calls in from kernel
/// tasks while the event loop answers and tears down, hence the lock.
final class PeerCalls: @unchecked Sendable {
    private let lock = NSLock()
    /// The last frame id handed out — ids are `h1`, `h2`, … per connection.
    private var seq = 0
    /// Frame id → the waiting call.
    private var waiting: [String: CheckedContinuation<(JSON, Blob?), Never>] = [:]
    /// Ids whose caller gave up (its timeout or `cancel`) before the call was
    /// parked — the park then settles at once instead of waiting forever.
    private var abandoned: Set<String> = []
    /// Set once the socket is gone: a call that reaches the route after that
    /// settles at once instead of waiting on a page that can no longer answer.
    private var gone = false

    /// The one ending for a call the page will never answer.
    static func disconnected(_ guid: String?) -> JSON {
        Refusal.make(
            Refusal.transportError,
            "the page \(guid ?? "None") disconnected before answering")
    }

    /// ONE call into the page: an outbound `{type:"call", id, target, payload}`
    /// frame, resolved by the inbound answer carrying the same id. A body rides
    /// the binary channel at `payload.<its path>` — where python's payload dict
    /// holds it and where the page's decoder puts it back.
    func call(
        guid: String, agentId: String, payload: JSON, body: Blob?, channel: Channel,
        outLock: OutboundLock
    ) async -> (JSON, Blob?) {
        let fid: String? = lock.withLock {
            // A call that resolved this route just as the socket closed must not
            // wait on an answer the teardown has already swept — it settles now,
            // the same way the teardown settles the ones already waiting.
            if gone || !channel.isActive { return nil }
            seq += 1
            return "h\(seq)"
        }
        guard let fid else { return (Self.disconnected(guid), nil) }
        // The body's slot in the payload is `null` on the wire, in python's
        // position (`encode_frame` nulls the bytes where they sat); the kernel
        // admitted the path at launch.
        var p = payload
        if let body { try? pySetPath(&p, parts: body.parts, .null) }
        let frame: JSON = .object([
            "type": .string("call"), "id": .string(fid), "target": .string(agentId),
            "payload": p,
        ])
        return await withTaskCancellationHandler {
            await withCheckedContinuation { (cont: CheckedContinuation<(JSON, Blob?), Never>) in
                let settled: (JSON, Blob?)? = lock.withLock {
                    if gone { return (Self.disconnected(guid), nil) }
                    if abandoned.remove(fid) != nil {
                        return (.object(["cancelled": .bool(true)]), nil)
                    }
                    waiting[fid] = cont
                    return nil
                }
                if let settled {
                    cont.resume(returning: settled)
                    return
                }
                outLock.sync {
                    WebSocketProxyHandler.sendEnvelope(
                        channel: channel, envelope: frame, body: body?.nested(in: "payload"))
                }
            }
        } onCancel: {
            // The caller gave up (its deadline, a `cancel`): take the entry back
            // out — python's `finally: out_calls.pop(fid, None)`.
            let cont: CheckedContinuation<(JSON, Blob?), Never>? = lock.withLock {
                if let c = waiting.removeValue(forKey: fid) { return c }
                abandoned.insert(fid)
                return nil
            }
            cont?.resume(returning: (.object(["cancelled": .bool(true)]), nil))
        }
    }

    /// ONE host emit into the page: `{type:"emit", target, payload}`. A body
    /// rides the binary channel at `payload.<its path>`, its slot `null` in the
    /// head as the kernel's item already holds it. A gone socket drops it — an
    /// emit has no reply.
    func emit(
        agentId: String, payload: JSON, body: Blob?, channel: Channel, outLock: OutboundLock
    ) {
        if lock.withLock({ gone }) || !channel.isActive { return }
        let frame: JSON = .object([
            "type": .string("emit"), "target": .string(agentId), "payload": payload,
        ])
        outLock.sync {
            WebSocketProxyHandler.sendEnvelope(
                channel: channel, envelope: frame, body: body?.nested(in: "payload"))
        }
    }

    /// `{type:"reply_mirror", id, data}` — the reply a sibling connection of
    /// this hello guid got for its call `id`, its chunk at its slot of `data`.
    func mirror(id: JSON, data: JSON, body: Blob?, channel: Channel, outLock: OutboundLock) {
        if lock.withLock({ gone }) || !channel.isActive { return }
        let frame: JSON = .object(["type": .string("reply_mirror"), "id": id, "data": data])
        outLock.sync {
            WebSocketProxyHandler.sendEnvelope(
                channel: channel, envelope: frame, body: body?.nested(in: "data"))
        }
    }

    /// An inbound `reply`/`error` answers one of OUR outbound calls — its `data`
    /// is that call's outcome (a page-side refusal included), a non-object
    /// wrapped as `{result}`; its chunk, when it carried one, at its slot of
    /// that `data`. An id we are not waiting on is ignored. Python `_on_answer`.
    func answer(_ frame: JSON, body: Blob?) {
        guard let fid = frame.asObject?["id"]?.asString else { return }
        guard let cont = lock.withLock({ waiting.removeValue(forKey: fid) }) else { return }
        let data: JSON
        switch frame.asObject?["data"] {
        case .some(.object(let o)): data = .object(o)
        case .some(let other): data = .object(["result": other])
        case .none: data = .object(["result": .null])
        }
        cont.resume(returning: (data, body))
    }

    /// The socket is gone: every call still waiting settles with the reason, and
    /// every later call settles at once. Idempotent.
    func close(guid: String?) {
        let pending: [CheckedContinuation<(JSON, Blob?), Never>] = lock.withLock {
            gone = true
            let all = Array(waiting.values)
            waiting.removeAll()
            return all
        }
        for c in pending { c.resume(returning: (Self.disconnected(guid), nil)) }
    }
}

/// The live connections of each hello guid on ONE leg — the tabs of one
/// identity, so a reply to one is mirrored to the others. Keyed by the kernel and
/// the leg too: two kernels in one process, or two legs, never share tabs.
/// Joined at hello, left at teardown.
final class Tabs: @unchecked Sendable {
    static let shared = Tabs()

    struct Key: Hashable {
        let kernel: ObjectIdentifier
        let leg: String
        let guid: String
    }

    /// Writes one `reply_mirror` (frame id, reply data, reply chunk) on a tab.
    typealias Mirror = @Sendable (JSON, JSON, Blob?) -> Void

    private let lock = NSLock()
    private var table: [Key: [(ObjectIdentifier, Mirror)]] = [:]

    func join(_ key: Key, _ conn: ObjectIdentifier, _ mirror: @escaping Mirror) {
        lock.withLock { table[key, default: []].append((conn, mirror)) }
    }

    func leave(_ key: Key, _ conn: ObjectIdentifier) {
        lock.withLock {
            let rest = (table[key] ?? []).filter { $0.0 != conn }
            table[key] = rest.isEmpty ? nil : rest
        }
    }

    /// Every live tab of `key` but `conn`.
    func siblings(_ key: Key, except conn: ObjectIdentifier) -> [Mirror] {
        lock.withLock { (table[key] ?? []).filter { $0.0 != conn }.map { $0.1 } }
    }
}

/// ONE connection's FRAME LANE — python's websocket read loop. Inbound frames are
/// handled one at a time, in arrival order, by one task that runs OUTSIDE every
/// dispatch (it is started from the event loop, which carries no task-local), so
/// what it hands to the leg is the operator's. `finish` ends it after the jobs
/// already queued.
final class FrameLane: @unchecked Sendable {
    typealias Job = @Sendable () async -> Void
    private let jobs: AsyncStream<Job>
    private let feed: AsyncStream<Job>.Continuation

    init() {
        (jobs, feed) = AsyncStream<Job>.makeStream()
    }

    /// Start the lane's one task, parented to `parent` in the task table.
    func start(parent: TaskId?) {
        let jobs = self.jobs
        Task {
            await KernelTaskLocals.$currentTask.withValue(parent) {
                for await job in jobs { await job() }
            }
        }
    }

    func enqueue(_ job: @escaping Job) { feed.yield(job) }

    func finish() { feed.finish() }
}
