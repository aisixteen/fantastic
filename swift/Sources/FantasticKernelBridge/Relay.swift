// relay_connector transport — a relay-KERNEL router (../fantastic_relay).
//
// Swift mirror of python `relay_connector/_relay.py` + rust `transport/relay.rs`.
// We dial `ws://<host>/<guid>` (subprotocol `fantastic.relay.v1`, header
// `X-Fantastic-Auth: <group password>` checked once at the WS upgrade) over the
// cross-platform `NIOWebSocketClient`, and reach a fixed `partner` GUID. The relay
// routes by `target` and delivers peer→peer ONE-WAY as `{type:"event", source,
// payload}` — no relay-level reply correlation.
//
// So this transport TUNNELS the kernel-bridge frames (`call`/`reply`/`event`):
// `send` wraps each in a relay envelope `{type:"send", target:<partner>,
// payload:<frame>}`; `receive` accepts only `{type:"event", source==partner}` and
// unwraps the inner frame. Like the canonical kernel it is a SYMMETRIC peer — it
// both `forward`s (call → await reply) AND serves inbound `call` frames by
// dispatching them on the local kernel and replying. The per-leg ingress/egress
// rules gate the tunneled calls (this is the ONLY inbound-call path in swift, so
// the gate lives here). PURE STREAMS: a control frame rides as a TEXT WS frame, a
// raw `read_stream` chunk as a BINARY WS frame `[4B len|header|body]` (no base64);
// the bridge frame's `_binary_path` is lifted to `payload.<path>` on the wire and
// shifted back on the far side (the relay forwards the body verbatim).

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation

private let HEARTBEAT_SECONDS: UInt64 = 30

/// The WS message surface the transport rides on — `NIOWebSocketClient` in
/// production, a paired in-process hub in tests (so the symmetric dispatch +
/// envelope wrap/unwrap are unit-testable without a network).
public protocol RelayWire: Sendable {
    func send(_ message: NIOWebSocketClient.Message) async throws
    func receive() async throws -> NIOWebSocketClient.Message
    func close() async
}

extension NIOWebSocketClient: RelayWire {}

public actor RelayTransport {
    /// The current WS surface, or nil while disconnected / mid-reconnect.
    private var client: (any RelayWire)?
    private let partner: String
    /// Re-dials a fresh connection on reconnect (nil for a test-attached fixed
    /// wire — that leg is one-shot).
    private let dialer: (@Sendable () async throws -> any RelayWire)?
    /// Backoff before each re-dial (0 = one-shot: a drop is terminal).
    private let reconnectSecs: Double

    /// Outstanding text forwards keyed by id; resumed on the matching reply.
    private var pending: [String: CheckedContinuation<JSON, Never>] = [:]
    /// Outstanding BINARY forwards keyed by id; resumed with `(reply, body)`.
    private var binaryPending: [String: CheckedContinuation<(JSON, Data), Never>] = [:]
    private var nextId: UInt64 = 1
    /// Outstanding relay-LEVEL requests (the directory `call`/`watch target:relay`),
    /// keyed by minted id — distinct from the partner bridge-frame `pending`.
    private var relayPending: [String: CheckedContinuation<JSON, Never>] = [:]
    private var relayNextId: UInt64 = 0
    private let forwardTimeoutSeconds: Double = 30.0

    /// Local sink for inbound `event` re-emit + inbound `call` dispatch.
    private var eventSink: AgentId?
    private weak var kernel: Kernel?

    /// Per-leg INGRESS rule (default AllowAll); consulted in `dispatch` before an
    /// inbound `call` reaches the kernel — the single auth choke point. EGRESS
    /// (default Silent) stamps the leg's credential on outbound frames.
    private var ingress: IngressRule = IngressRules.AllowAll()
    private var egress: EgressRule = EgressRules.Silent()

    private var receiveTask: Task<Void, Never>?
    private var heartbeatTask: Task<Void, Never>?
    private var open = true

    private init(
        client: (any RelayWire)?, partner: String,
        dialer: (@Sendable () async throws -> any RelayWire)?, reconnectSecs: Double
    ) {
        self.client = client
        self.partner = partner
        self.dialer = dialer
        self.reconnectSecs = reconnectSecs
    }

    /// Test seam: attach to a pre-built wire (a paired in-process hub) instead of
    /// dialing a real relay. One-shot (no dialer). Wires the local kernel sink +
    /// auth rules + loops, the same as `connect`'s tail.
    public static func attach(
        wire: any RelayWire,
        partnerGuid: String,
        localAgentId: AgentId? = nil,
        localKernel: Kernel? = nil,
        ingress: IngressRule = IngressRules.AllowAll(),
        egress: EgressRule = EgressRules.Silent()
    ) async -> RelayTransport {
        let t = RelayTransport(
            client: wire, partner: partnerGuid, dialer: nil, reconnectSecs: 0)
        await t.finishBoot(
            localAgentId: localAgentId, localKernel: localKernel,
            ingress: ingress, egress: egress)
        return t
    }

    /// Dial the relay as `guid`, tunnel to `partnerGuid`, wire the local kernel
    /// sink + auth rules, then start the receive + heartbeat loops. The initial
    /// dial is eager; with `reconnect > 0` a failed dial still returns a (healing)
    /// transport, and a later drop re-dials after the backoff.
    public static func connect(
        relayURL: URL,
        guid: String,
        token: String,
        partnerGuid: String,
        reconnect: Double = 10,
        localAgentId: AgentId? = nil,
        localKernel: Kernel? = nil,
        ingress: IngressRule = IngressRules.AllowAll(),
        egress: EgressRule = EgressRules.Silent()
    ) async throws -> RelayTransport {
        // The path GUID rides in the URL; the subprotocol + auth header are set by
        // the client. (appendingPathComponent keeps the ws:// scheme.)
        let dialURL = relayURL.appendingPathComponent(guid)
        let dialer: @Sendable () async throws -> any RelayWire = {
            try await NIOWebSocketClient.connect(
                url: dialURL, subprotocols: ["fantastic.relay.v1"], authToken: token)
        }
        var initial: (any RelayWire)?
        do {
            initial = try await dialer()
        } catch {
            if reconnect <= 0 { throw error }  // one-shot: boot fails loudly.
            initial = nil  // heal in the background.
        }
        let t = RelayTransport(
            client: initial, partner: partnerGuid, dialer: dialer, reconnectSecs: reconnect)
        await t.finishBoot(
            localAgentId: localAgentId, localKernel: localKernel,
            ingress: ingress, egress: egress)
        return t
    }

    /// A usable socket exists right now (not closed, not mid-reconnect).
    public var isLive: Bool { open && client != nil }

    private func finishBoot(
        localAgentId: AgentId?, localKernel: Kernel?,
        ingress: IngressRule, egress: EgressRule
    ) {
        if let localAgentId, let localKernel {
            self.eventSink = localAgentId
            self.kernel = localKernel
        }
        self.ingress = ingress
        self.egress = egress
        receiveTask = Task { [weak self] in await self?.receiveLoop() }
        heartbeatTask = Task { [weak self] in await self?.heartbeatLoop() }
    }

    // ── receive loop ───────────────────────────────────────────────

    private func receiveLoop() async {
        while open {
            if client == nil {
                guard let c = await reconnect() else { break }  // gave up / closed
                client = c
            }
            let msg: NIOWebSocketClient.Message
            do {
                msg = try await client!.receive()
            } catch {
                // Drop: heal (reconnect on the next loop) unless one-shot/closed.
                client = nil
                if reconnectSecs <= 0 { break }
                continue
            }
            switch msg {
            case .text(let s):
                guard let env = try? JSON.parse(s) else { continue }
                switch env["type"].asString {
                case "reply":
                    // Relay-LEVEL reply (a directory list_peers/watch ack) — resolve
                    // the pending request; never reaches the engine's bridge path.
                    if let id = env["id"].asString,
                        let cont = relayPending.removeValue(forKey: id)
                    {
                        cont.resume(returning: env["data"])
                    }
                case "event":
                    let source = env["source"].asString
                    if source == "relay" {
                        // Directory event → emit on THIS connector's inbox so a local
                        // watcher renders peer_joined|left|evicted|peer_status.
                        if let sink = eventSink, let kernel = kernel {
                            await kernel.emit(sink, env["payload"])
                        }
                    } else if source == partner {
                        await dispatch(env["payload"], body: Data(), isBinary: false)
                    }
                default:
                    break
                }
            case .binary(let bytes):
                guard let (env, body) = Codec.decodeBinaryFrame(Data(bytes)),
                    env["type"].asString == "event",
                    env["source"].asString == partner
                else { continue }
                // A binary frame → dispatch on the binary channel even if `body` is
                // empty (a read_stream REQUEST has no input bytes but its REPLY does).
                await dispatch(env["payload"], body: body, isBinary: true)
            }
        }
        failAllPending("relay_connector connection closed")
    }

    /// (Re)dial with the backoff. Returns a live wire, or nil if closed / one-shot.
    private func reconnect() async -> (any RelayWire)? {
        guard reconnectSecs > 0, let dialer = dialer else { return nil }
        while open {
            try? await Task.sleep(nanoseconds: UInt64(reconnectSecs * 1_000_000_000))
            if !open { return nil }
            if let c = try? await dialer() { return c }
        }
        return nil
    }

    private func heartbeatLoop() async {
        // Keep the relay peer `green`: the relay's `keepalive` verb is a no-reply
        // refresh — any inbound .text frame touches the peer's last_seen.
        while open {
            try? await Task.sleep(nanoseconds: HEARTBEAT_SECONDS * 1_000_000_000)
            if !open { break }
            guard let client = client else { continue }  // disconnected; receiveLoop heals.
            try? await client.send(
                .text(JSON.object(["type": .string("keepalive")]).serialize()))
        }
    }

    /// Route one unwrapped inbound bridge frame: a `reply`/`error` resolving a
    /// pending forward, an `event` re-emit, or an inbound `call` gated + dispatched
    /// on the local kernel (text or binary). `body` is empty for a text frame.
    private func dispatch(_ frame: JSON, body: Data, isBinary: Bool) async {
        switch frame["type"].asString {
        case "reply":
            guard let id = frame["id"].asString else { return }
            if let cont = pending.removeValue(forKey: id) {
                cont.resume(returning: frame["data"])
            } else if let cont = binaryPending.removeValue(forKey: id) {
                // read_stream → body carries raw bytes; write_stream → empty body.
                cont.resume(returning: (frame["data"], body))
            }
        case "error":
            guard let id = frame["id"].asString else { return }
            let err: JSON = .object([
                "error": .string("remote error: \(frame["error"].asString ?? "unknown")")
            ])
            if let cont = pending.removeValue(forKey: id) {
                cont.resume(returning: err)
            } else if let cont = binaryPending.removeValue(forKey: id) {
                cont.resume(returning: (err, Data()))
            }
        case "event":
            if let sink = eventSink, let kernel = kernel {
                await kernel.emit(sink, frame["payload"])
            }
        case "call":
            await dispatchCall(frame, body: body, isBinary: isBinary)
        default:
            break  // keepalive / unknown — ignored (py/rust parity)
        }
    }

    /// Inbound `call` — AUTH GATE first (the single choke point), then dispatch on
    /// the local kernel and tunnel the reply back. A call that arrived as a BINARY
    /// frame goes through `sendWithBinary` (so a read_stream reply can carry raw
    /// bytes) even when the request `body` is empty.
    private func dispatchCall(_ frame: JSON, body: Data, isBinary: Bool) async {
        let id = frame["id"]
        let target = frame["target"].asString ?? ""
        let inner = frame["payload"]
        let verb = inner["type"].asString ?? ""
        let decision = ingress.authorize(
            AuthAction(
                kind: "call", target: target, verb: verb,
                token: frame["auth_token"].asString))
        if target.isEmpty {
            _ = await sendReply(
                id: id, data: .object(["error": .string("relay_connector: empty call target")]))
            return
        }
        if case .deny(let reason) = decision {
            _ = await sendReply(
                id: id,
                data: .object([
                    "error": .string(reason), "reason": .string("unauthorized"),
                ]))
            return
        }
        guard let kernel = kernel else {
            _ = await sendReply(
                id: id, data: .object(["error": .string("relay_connector: no local kernel")]))
            return
        }
        if !isBinary {
            let reply = await kernel.send(AgentId(target), inner)
            _ = await sendReply(id: id, data: reply)
        } else {
            let (reply, replyBody) = await kernel.sendWithBinary(AgentId(target), inner, body)
            if replyBody.isEmpty {
                // write_stream status → a plain text reply (no body).
                _ = await sendReply(id: id, data: reply)
            } else {
                // read_stream reply → a binary reply, body raw at `data.bytes`.
                var header: JSON = .object(["type": .string("reply"), "id": id, "data": reply])
                header["_binary_path"] = .string("data.bytes")
                _ = await sendBinary(header: header, body: replyBody)
            }
        }
    }

    // ── send path (wrap in the relay envelope) ─────────────────────

    /// Wrap a text bridge frame in `{type:send, target:partner, payload:<frame>}`
    /// and ship it as a TEXT WS message.
    private func sendText(_ frame: JSON) async -> Bool {
        guard open, let client = client else { return false }
        let env: JSON = .object([
            "type": .string("send"), "target": .string(partner), "payload": frame,
        ])
        do {
            try await client.send(.text(env.serialize()))
            return true
        } catch {
            self.client = nil  // socket gone; receiveLoop re-dials.
            return false
        }
    }

    /// Wrap a binary bridge frame (header + raw body) in the relay envelope and
    /// ship it as a BINARY WS message. The header's `_binary_path` is lifted to
    /// `payload.<path>` so the partner restores the body inside the tunneled frame.
    private func sendBinary(header: JSON, body: Data) async -> Bool {
        guard open, let client = client else { return false }
        var inner = header
        let innerPath = inner["_binary_path"].asString
        if case .object(var m) = inner {
            m["_binary_path"] = nil
            inner = .object(m)
        }
        var env: JSON = .object([
            "type": .string("send"), "target": .string(partner), "payload": inner,
        ])
        if let p = innerPath {
            env["_binary_path"] = .string("payload.\(p)")
        }
        do {
            try await client.send(
                .binary([UInt8](Codec.encodeBinaryFrame(header: env, body: body))))
            return true
        } catch {
            self.client = nil  // socket gone; receiveLoop re-dials.
            return false
        }
    }

    private func sendReply(id: JSON, data: JSON) async -> Bool {
        await sendText(.object(["type": .string("reply"), "id": id, "data": data]))
    }

    // ── verbs ──────────────────────────────────────────────────────

    public func forward(target: AgentId, payload: JSON) async -> JSON {
        guard open else {
            return .object([
                "error": .string("relay_connector: not connected"),
                "reason": .string("not_connected"),
            ])
        }
        let id = mintId()
        var frame: JSON = .object([
            "type": .string("call"), "id": .string(id),
            "target": .string(target.value), "payload": payload,
        ])
        if let token = egress.credential() {
            frame["auth_token"] = .string(token)
        }
        let sent = await sendText(frame)
        if !sent {
            return .object([
                "error": .string("relay_connector.forward: send failed"),
                "reason": .string("transport_error"),
            ])
        }
        return await withCheckedContinuation { cont in
            pending[id] = cont
            Task { await self.timeoutPending(id: id) }
        }
    }

    public func binaryForward(target: AgentId, header: JSON, blob: Data) async -> (JSON, Data) {
        guard open else {
            return (
                .object([
                    "error": .string("relay_connector: not connected"),
                    "reason": .string("not_connected"),
                ]), Data()
            )
        }
        let id = mintId()
        // STANDARD call envelope (py/rust parity): inner call in `payload`,
        // `_binary_path` naming where the raw body belongs for the receiver.
        var wire: JSON = .object([
            "type": .string("call"), "id": .string(id),
            "target": .string(target.value), "payload": header,
            "_binary_path": .string("payload.bytes"),
        ])
        if let token = egress.credential() {
            wire["auth_token"] = .string(token)
        }
        let sent = await sendBinary(header: wire, body: blob)
        if !sent {
            return (
                .object([
                    "error": .string("relay_connector.binaryForward: send failed"),
                    "reason": .string("transport_error"),
                ]), Data()
            )
        }
        return await withCheckedContinuation { cont in
            binaryPending[id] = cont
            Task { await self.timeoutBinaryPending(id: id) }
        }
    }

    public func watchRemote(target: AgentId) async -> JSON {
        let ok = await sendText(
            .object(["type": .string("watch"), "src": .string(target.value)]))
        return ok
            ? .object(["ok": .bool(true), "watching": .string(target.value)])
            : .object(["error": .string("relay_connector.watch_remote: send failed")])
    }

    public func unwatchRemote(target: AgentId) async -> JSON {
        let ok = await sendText(
            .object(["type": .string("unwatch"), "src": .string(target.value)]))
        return ok
            ? .object(["ok": .bool(true), "unwatched": .string(target.value)])
            : .object(["error": .string("relay_connector.unwatch_remote: send failed")])
    }

    // ── directory surface (the relay's own `relay` agent) ─────────

    /// Send a relay-LEVEL frame to the directory (`target:"relay"` + a minted id)
    /// and await the correlated `{type:"reply", id, data}`. Bypasses the partner
    /// tunnel — directory frames are not `send`-wrapped.
    private func relayRequest(_ frame: JSON, timeout: Double) async -> JSON {
        guard open, let client = client else {
            return .object([
                "error": .string("relay_connector: not connected"),
                "reason": .string("not_connected"),
            ])
        }
        relayNextId += 1
        let rid = "dir_\(relayNextId)"
        var out = frame
        out["id"] = .string(rid)
        out["target"] = .string("relay")
        do {
            try await client.send(.text(out.serialize()))
        } catch {
            self.client = nil
            return .object([
                "error": .string("relay_connector: directory send failed"),
                "reason": .string("transport_error"),
            ])
        }
        return await withCheckedContinuation { cont in
            relayPending[rid] = cont
            Task { await self.timeoutRelayPending(id: rid, timeout: timeout) }
        }
    }

    /// Directory snapshot → `{peers:[{guid,status,last_seen,since}]}`.
    public func listPeers(timeout: Double = 30) async -> JSON {
        await relayRequest(
            .object(["type": .string("call"), "payload": .object(["type": .string("list_peers")])]),
            timeout: timeout)
    }

    /// Subscribe to the relay directory; `peer_*` events re-emit on the connector inbox.
    public func watchDirectory(timeout: Double = 10) async -> JSON {
        await relayRequest(.object(["type": .string("watch")]), timeout: timeout)
    }

    /// Stop the directory subscription (the relay sends no reply for unwatch).
    public func unwatchDirectory() async -> JSON {
        guard open, let client = client else { return .object(["ok": .bool(true)]) }
        try? await client.send(
            .text(
                JSON.object(["type": .string("unwatch"), "target": .string("relay")]).serialize()))
        return .object(["ok": .bool(true), "unwatched": .string("relay")])
    }

    private func timeoutRelayPending(id: String, timeout: Double) async {
        try? await Task.sleep(nanoseconds: UInt64(timeout * 1_000_000_000))
        if let cont = relayPending.removeValue(forKey: id) {
            cont.resume(
                returning: .object([
                    "error": .string("relay_connector: directory timeout after \(Int(timeout))s"),
                    "reason": .string("timeout"),
                ]))
        }
    }

    public func close() async {
        guard open else { return }
        open = false
        receiveTask?.cancel()
        heartbeatTask?.cancel()
        await client?.close()
        client = nil
        failAllPending("relay_connector closed")
    }

    // ── helpers ────────────────────────────────────────────────────

    private func timeoutPending(id: String) async {
        try? await Task.sleep(nanoseconds: UInt64(forwardTimeoutSeconds * 1_000_000_000))
        if let cont = pending.removeValue(forKey: id) {
            cont.resume(
                returning: .object([
                    "error": .string(
                        "relay_connector.forward: timeout after \(Int(forwardTimeoutSeconds))s"),
                    "reason": .string("timeout"),
                ]))
        }
    }

    private func timeoutBinaryPending(id: String) async {
        try? await Task.sleep(nanoseconds: UInt64(forwardTimeoutSeconds * 1_000_000_000))
        if let cont = binaryPending.removeValue(forKey: id) {
            cont.resume(
                returning: (
                    .object([
                        "error": .string(
                            "relay_connector.binaryForward: timeout after \(Int(forwardTimeoutSeconds))s"
                        ),
                        "reason": .string("timeout"),
                    ]), Data()
                ))
        }
    }

    private func failAllPending(_ reason: String) {
        for (_, cont) in pending {
            cont.resume(
                returning: .object([
                    "error": .string(reason), "reason": .string("transport_dropped"),
                ]))
        }
        pending.removeAll()
        for (_, cont) in binaryPending {
            cont.resume(
                returning: (
                    .object([
                        "error": .string(reason), "reason": .string("transport_dropped"),
                    ]), Data()
                ))
        }
        binaryPending.removeAll()
        for (_, cont) in relayPending {
            cont.resume(
                returning: .object([
                    "error": .string(reason), "reason": .string("transport_dropped"),
                ]))
        }
        relayPending.removeAll()
    }

    private func mintId() -> String {
        nextId &+= 1
        return "rc_\(nextId)"
    }
}
