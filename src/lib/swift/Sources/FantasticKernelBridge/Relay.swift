// relay_connector transport seam — a relay-KERNEL router (../fantastic_relay).
//
// Python `relay_connector/_relay.py`, shape for shape. We dial
// `ws://<host>/<guid>` (subprotocol `fantastic.relay.v1`, header
// `X-Fantastic-Auth: <group password>` checked once at the WS upgrade) and reach
// a fixed `partner` GUID. The relay routes by `target` and delivers peer→peer
// ONE-WAY as `{type:"event", source, payload}` — there is NO relay-level reply
// correlation for peer→peer.
//
// So this transport TUNNELS the io_bridge engine's frames: `send` wraps each in
// `{type:"send", target:<partner>, payload:<frame>}`, `recv` unwraps the
// partner's `event.payload` back into a bridge frame. Correlation, the inbound
// gate and the verbs are the ENGINE's — this file owns only the socket, the
// self-healing re-dial, the heartbeat and the relay's own directory surface.
//
// PURE STREAMS: a control frame rides as a TEXT WS frame, a raw `read_stream`
// chunk as a BINARY WS frame `[4B len|header|body]` (no base64); the bridge
// frame's `_binary_path` is lifted to `payload.<path>` on the wire.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Python `_relay.SUBPROTOCOL` / `DEFAULT_HEARTBEAT` / `DEFAULT_RECONNECT`.
public let RELAY_SUBPROTOCOL = "fantastic.relay.v1"
public let DEFAULT_HEARTBEAT = 30.0
public let DEFAULT_RECONNECT = 10.0
/// Python `ROLES`, rendered as python renders the tuple in its refusals.
let RELAY_ROLES = ["manager", "kernel"]
let RELAY_ROLES_TEXT = "('manager', 'kernel')"

/// The WS message surface the transport rides on — `NIOWebSocketClient` in
/// production, a paired in-process hub in tests (so the envelope wrap/unwrap is
/// unit-testable without a network).
public protocol RelayWire: Sendable {
    func send(_ message: NIOWebSocketClient.Message) async throws
    func receive() async throws -> NIOWebSocketClient.Message
    func close() async
}

extension NIOWebSocketClient: RelayWire {}

public actor RelayTransport: BridgeTransport {
    /// The current WS surface, or nil while disconnected / mid-reconnect.
    private var client: (any RelayWire)?
    private let partner: String
    /// Re-dials a fresh connection (nil for a test-attached fixed wire).
    private let dialer: (@Sendable () async throws -> any RelayWire)?
    /// Backoff before each re-dial (0 = one-shot: a drop is terminal).
    private let reconnectSecs: Double
    private let heartbeatSecs: Double
    /// How this transport FORKS. Production passes the kernel's `spawnTask`, so
    /// the heartbeat is a `ps` row under its connector and `cancel` reaches it.
    private let spawn: (@Sendable (String, @escaping @Sendable () async -> Void) -> Void)?
    /// Directory attributes we advertise (opaque to the relay). `{}` ⇒ a plain
    /// peer, no announce sent.
    private var identity: JSON
    /// Relay-LEVEL request correlation (the directory `call`/`watch
    /// target:relay`) — distinct from the engine's bridge-frame correlation.
    private var relayPending: [String: RelayWaiter] = [:]
    private var relayNextId: UInt64 = 0
    private var heartbeatGeneration = 0
    private var closedFlag = false

    private init(
        client: (any RelayWire)?, partner: String,
        dialer: (@Sendable () async throws -> any RelayWire)?, reconnectSecs: Double,
        heartbeatSecs: Double, identity: JSON,
        spawn: (@Sendable (String, @escaping @Sendable () async -> Void) -> Void)?
    ) {
        self.client = client
        self.partner = partner
        self.dialer = dialer
        self.reconnectSecs = reconnectSecs
        self.heartbeatSecs = heartbeatSecs
        self.identity = identity
        self.spawn = spawn
    }

    /// Test seam: attach to a pre-built wire (a paired in-process hub) instead of
    /// dialing a real relay. One-shot (no dialer), like python's fixed-`ws`
    /// construction.
    public static func attach(
        wire: any RelayWire,
        partnerGuid: String,
        identity: JSON = [:]
    ) async -> RelayTransport {
        let t = RelayTransport(
            client: wire, partner: partnerGuid, dialer: nil, reconnectSecs: 0,
            heartbeatSecs: 0, identity: identity, spawn: nil)
        await t.onSocketOpen()
        return t
    }

    /// Python `RelayTransport.connect`: eager first dial. On failure — one-shot
    /// (`reconnect <= 0`) raises so boot fails loudly; otherwise the leg boots
    /// and `recv()` auto-connects in the background.
    public static func connect(
        relayURL: URL,
        guid: String,
        token: String,
        partnerGuid: String,
        heartbeat: Double = DEFAULT_HEARTBEAT,
        reconnect: Double = DEFAULT_RECONNECT,
        identity: JSON = [:],
        spawn: (@Sendable (String, @escaping @Sendable () async -> Void) -> Void)? = nil,
        dialer: (@Sendable () async throws -> any RelayWire)? = nil
    ) async throws -> RelayTransport {
        // Python `connect(..., dialer=None)`: `dialer or _default_dialer(...)`.
        // The caller-supplied dialer is how the self-healing loop is driven
        // without a network, and it is python's own signature.
        let dialURL = relayURL.appendingPathComponent(guid)
        let dialer: @Sendable () async throws -> any RelayWire = dialer ?? {
            try await NIOWebSocketClient.connect(
                url: dialURL, subprotocols: [RELAY_SUBPROTOCOL], authToken: token)
        }
        var initial: (any RelayWire)?
        do {
            initial = try await dialer()
        } catch {
            if reconnect <= 0 { throw ConnectionClosed("\(error)") }
            initial = nil  // heal in the background
        }
        let t = RelayTransport(
            client: initial, partner: partnerGuid, dialer: dialer, reconnectSecs: reconnect,
            heartbeatSecs: heartbeat, identity: identity, spawn: spawn)
        if initial != nil { await t.onSocketOpen() }
        return t
    }

    /// Python `is_live`: a usable socket exists right now (not closed, not
    /// mid-reconnect). `reflect.connected` reads THIS, not `closed`.
    public var isLive: Bool { !closedFlag && client != nil }

    /// Python `closed`: EXPLICIT close only — a transient drop is healed by
    /// `recv()`, so the leg survives blips and the engine does not tear it down.
    public func isClosed() async -> Bool { closedFlag }

    // MARK: - socket lifecycle

    /// Python `_on_socket_open`: run after EVERY successful (re)dial — start the
    /// heartbeat and re-advertise our identity (the relay drops per-connection
    /// state, so the announce must ride each new socket).
    private func onSocketOpen() async {
        startHeartbeat()
        await announce()
    }

    private func startHeartbeat() {
        heartbeatGeneration += 1
        guard heartbeatSecs > 0, let spawn else { return }
        let generation = heartbeatGeneration
        spawn("relay_heartbeat") { [weak self] in
            await self?.heartbeatLoop(generation: generation)
        }
    }

    /// Python `_heartbeat_loop`: keep the relay peer `green`. Any inbound text
    /// frame refreshes `last_seen`; the relay's `keepalive` verb is a no-reply
    /// refresh — exactly that, with no wire noise.
    private func heartbeatLoop(generation: Int) async {
        while !closedFlag && generation == heartbeatGeneration {
            try? await Task.sleep(nanoseconds: UInt64(heartbeatSecs * 1_000_000_000))
            if closedFlag || generation != heartbeatGeneration { return }
            guard let client else { return }  // disconnected; recv() restarts it
            do {
                try await client.send(
                    .text(Codec.encodeText(.object(["type": .string("keepalive")]))))
            } catch {
                return
            }
        }
    }

    /// Python `_announce`: advertise our attrs (`target:"relay"`,
    /// fire-and-forget). No-op for a plain peer or while disconnected.
    private func announce() async {
        guard case .object(let m) = identity, !m.isEmpty, let client else { return }
        let frame: JSON = .object([
            "type": .string("announce"), "attrs": identity, "target": .string("relay"),
        ])
        do {
            try await client.send(.text(Codec.encodeText(frame)))
        } catch {
            self.client = nil  // the socket is gone; recv() re-dials + re-announces
        }
    }

    /// Python `set_identity`: replace the advertised attrs and re-announce now.
    public func setIdentity(_ attrs: JSON) async -> JSON {
        identity = pyTruthy(attrs) ? attrs : .object([:])
        await announce()
        return .object(["ok": .bool(true), "attrs": identity])
    }

    // MARK: - send / recv

    public func send(_ frame: JSON) async throws {
        guard let client else { throw ConnectionClosed("relay_connector: not connected") }
        let envelope: JSON = .object([
            "type": .string("send"), "target": .string(partner), "payload": frame,
        ])
        do {
            try await client.send(.text(Codec.encodeText(envelope)))
        } catch {
            self.client = nil  // the socket is gone; recv() will re-dial
            throw ConnectionClosed("\(error)")
        }
    }

    public func sendBinary(header: JSON, body: Data) async throws {
        guard let client else { throw ConnectionClosed("relay_connector: not connected") }
        var inner = header
        let innerPath = inner["_binary_path"].asString
        if case .object(var m) = inner {
            m["_binary_path"] = nil
            inner = .object(m)
        }
        var envelope: JSON = .object([
            "type": .string("send"), "target": .string(partner), "payload": inner,
        ])
        if let innerPath { envelope["_binary_path"] = .string("payload.\(innerPath)") }
        do {
            try await client.send(
                .binary([UInt8](Codec.encodeBinaryFrame(header: envelope, body: body))))
        } catch {
            self.client = nil
            throw ConnectionClosed("\(error)")
        }
    }

    /// Python `recv`: own the socket lifecycle. Only `{type:"event"}` matters —
    /// `source == "relay"` is a directory event (surfaced as a bridge `event`),
    /// `source == partner` is the tunnelled bridge frame. Everything else is
    /// skipped; a relay-level `reply` resolves a directory request and never
    /// reaches the engine.
    public func recv() async throws -> (frame: JSON, body: Data, isBinary: Bool) {
        while !closedFlag {
            guard let wire = client else {
                guard let redialled = await reconnect() else { break }
                client = redialled
                await onSocketOpen()
                continue
            }
            let msg: NIOWebSocketClient.Message
            do {
                msg = try await wire.receive()
            } catch {
                client = nil
                if reconnectSecs <= 0 {
                    throw ConnectionClosed("relay_connector: connection dropped")
                }
                try? await Task.sleep(nanoseconds: UInt64(reconnectSecs * 1_000_000_000))
                continue
            }
            let envelope: JSON
            let body: Data
            let isBinary: Bool
            switch msg {
            case .text(let s):
                guard let parsed = try? JSON.parseStrict(s), case .object = parsed else { continue }
                envelope = parsed
                body = Data()
                isBinary = false
            case .binary(let bytes):
                guard let (header, raw) = try? Codec.decodeBinaryFrame(Data(bytes)),
                    case .object = header
                else { continue }
                envelope = header
                body = raw
                isBinary = true
            }
            switch envelope["type"].asString {
            case "reply":
                // Relay-LEVEL reply (a directory ack) — resolve the pending
                // request; python answers `msg.get("data") or {}`.
                if let rid = envelope["id"].asString,
                    let waiter = relayPending.removeValue(forKey: rid)
                {
                    let data = envelope["data"]
                    waiter.finish(pyTruthy(data) ? data : .object([:]))
                }
            case "event":
                let payload = envelope["payload"]
                guard case .object = payload else { continue }
                let source = envelope["source"].asString
                if source == "relay" {
                    // A DIRECTORY event — surfaced as a bridge `event` so the
                    // engine re-emits it on this connector's inbox.
                    return (
                        .object(["type": .string("event"), "payload": payload]), Data(), false
                    )
                }
                guard source == partner else { continue }
                // The tunnelled frame's OWN `_binary_path`: `sendBinary` lifted
                // it to `payload.<path>` on the envelope, so it comes back down —
                // python's `decode_frame` places the body inside the inner frame
                // at exactly that path.
                var frame = payload
                if isBinary, let p = envelope["_binary_path"].asString, p.hasPrefix("payload.") {
                    frame["_binary_path"] = .string(String(p.dropFirst("payload.".count)))
                }
                return (frame, body, isBinary)
            default:
                break
            }
        }
        throw ConnectionClosed("relay_connector: closed")
    }

    /// Python `_reconnect_ws`: (re)dial with the backoff. nil = explicitly
    /// closed or one-shot.
    ///
    /// THE DIAL COMES FIRST AND THE SLEEP ONLY AFTER A FAILED ONE, exactly as
    /// python's loop is written (`try: dial ... except: await sleep; continue`).
    /// Sleeping before the first attempt charged a full `reconnect` interval to
    /// a socket that was already gone — and `recv`'s own catch has ALREADY slept
    /// one, so a drop healed at 2x the backoff. Measured on the parity `bridges`
    /// capture: `relay_connections` 1 where python opened 2, because python's
    /// re-dial landed inside the window and this one did not.
    private func reconnect() async -> (any RelayWire)? {
        guard reconnectSecs > 0, let dialer else { return nil }
        while !closedFlag {
            if let c = try? await dialer() { return c }
            if closedFlag { return nil }
            try? await Task.sleep(nanoseconds: UInt64(reconnectSecs * 1_000_000_000))
        }
        return nil
    }

    public func close() async {
        guard !closedFlag else { return }
        closedFlag = true
        heartbeatGeneration += 1
        let waiters = relayPending
        relayPending.removeAll()
        for (_, w) in waiters {
            w.finish(
                .object([
                    "error": .string("relay_connector: closed"),
                    "reason": .string("transport_closed"),
                ]))
        }
        await client?.close()
        client = nil
    }

    // MARK: - directory surface (the relay's own `relay` agent)

    /// Python `_relay_request`: a relay-LEVEL frame with a minted id, awaiting
    /// the correlated `{type:"reply", id, data}`. Directory frames bypass the
    /// partner tunnel — they are not `send`-wrapped.
    private func relayRequest(_ frame: JSON, timeout: Double) async -> JSON {
        guard let client else {
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
            try await client.send(.text(Codec.encodeText(out)))
        } catch {
            self.client = nil
            return .object([
                "error": .string("relay_connector: directory send failed: \(error)"),
                "reason": .string("transport_error"),
            ])
        }
        return await withCheckedContinuation { (c: CheckedContinuation<JSON, Never>) in
            let waiter = RelayWaiter(c)
            relayPending[rid] = waiter
            Task { [weak self] in
                try? await Task.sleep(nanoseconds: UInt64(max(0, timeout) * 1_000_000_000))
                await self?.timeoutRelayPending(rid, timeout)
            }
        }
    }

    private func timeoutRelayPending(_ rid: String, _ timeout: Double) {
        guard let waiter = relayPending.removeValue(forKey: rid) else { return }
        waiter.finish(
            .object([
                "error": .string(
                    "relay_connector: directory timeout after \(pyFloatRepr(timeout))s"),
                "reason": .string("timeout"),
            ]))
    }

    /// Python `list_peers`: one-shot snapshot → `{peers:[{guid,status,…}]}`.
    public func listPeers(timeout: Double = 30) async -> JSON {
        await relayRequest(
            .object([
                "type": .string("call"),
                "payload": .object(["type": .string("list_peers")]),
            ]), timeout: timeout)
    }

    /// Python `watch_directory`: subscribe; `peer_*` events re-emit on the
    /// connector's inbox. The timeout is FIXED at 10.0 — the verb takes none.
    public func watchDirectory(timeout: Double = 10) async -> JSON {
        await relayRequest(.object(["type": .string("watch")]), timeout: timeout)
    }

    /// Python `unwatch_directory`: the relay sends no reply for unwatch.
    public func unwatchDirectory() async -> JSON {
        guard let client else { return .object(["ok": .bool(true)]) }
        do {
            try await client.send(
                .text(
                    Codec.encodeText(
                        .object([
                            "type": .string("unwatch"), "target": .string("relay"),
                        ]))))
        } catch {
            self.client = nil
        }
        return .object(["ok": .bool(true), "unwatched": .string("relay")])
    }
}

/// Carries one directory continuation into the pending table.
final class RelayWaiter: @unchecked Sendable {
    private let continuation: CheckedContinuation<JSON, Never>
    init(_ c: CheckedContinuation<JSON, Never>) { continuation = c }
    func finish(_ reply: JSON) { continuation.resume(returning: reply) }
}

// MARK: - the relay build_transport seam

extension KernelBridgeBundle {
    /// Python `relay_connector._relay.build_transport`. Required: `relay_url`,
    /// `partner_guid`. `guid` (our WS path) is auto-minted on first boot if
    /// absent and PERSISTED, so every later hydration re-dials the SAME path; an
    /// explicit guid always wins and a minted one is never regenerated.
    func buildRelayTransport(
        agentId: AgentId, meta: OrderedDictionary<String, JSON>, kernel: Kernel, st: BridgeState
    ) async throws -> any BridgeTransport {
        let relayURLValue = meta["relay_url"] ?? .null
        let partnerValue = meta["partner_guid"] ?? .null
        guard pyTruthy(relayURLValue), pyTruthy(partnerValue) else {
            throw BridgeConfigError("relay_connector requires relay_url, partner_guid")
        }
        // The guid is minted only AFTER the required fields validate, so a
        // half-configured leg leaves no durable identity behind.
        var guid = pyTruthy(meta["guid"] ?? .null) ? pyStr(meta["guid"]!) : ""
        if guid.isEmpty, let agent = kernel.agent(agentId) {
            guid = Kernel.mintGuid()
            // THROUGH THE KERNEL UPDATE PATH: it publishes `updated`, which is
            // what makes the record dirty for the root loader's writer. A direct
            // persist bypassed the writer's dirty set entirely.
            kernel.update(agent.id, ["guid": .string(guid)])
        }
        let roleValue = meta["role"] ?? .null
        if case .null = roleValue {
            // absent / null — python's `if role is not None` skips the check
        } else if !RELAY_ROLES.contains(pyStr(roleValue)) {
            throw BridgeConfigError(
                "relay_connector role must be one of \(RELAY_ROLES_TEXT), got \(pyRepr(roleValue))")
        }
        let tokenValue = meta["relay_token"] ?? .null
        let token = pyTruthy(tokenValue) ? pyStr(tokenValue) : ""
        let heartbeat = try floatArg(meta["heartbeat"] ?? .double(DEFAULT_HEARTBEAT))
        let reconnect = try floatArg(meta["reconnect"] ?? .double(DEFAULT_RECONNECT))
        guard let relayURL = URL(string: pyStr(relayURLValue)) else {
            throw BridgeConfigError("relay_connector: malformed relay_url")
        }
        return try await RelayTransport.connect(
            relayURL: relayURL, guid: guid, token: token, partnerGuid: pyStr(partnerValue),
            heartbeat: heartbeat, reconnect: reconnect, identity: relayIdentity(meta),
            spawn: { verb, work in
                kernel.spawnTask(agent: agentId, verb: verb, work: work)
            })
    }

    /// `float(x)` inside `build_transport`: python raises the bare ValueError.
    private func floatArg(_ v: JSON) throws -> Double {
        do {
            return try pyFloat(v)
        } catch let e as PyError {
            throw BridgeConfigError(e.message)
        }
    }
}
