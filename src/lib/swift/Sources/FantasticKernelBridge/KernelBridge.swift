// The two io_bridge derivations — `ws_bridge` and `relay_connector`.
//
// Python ships them as two thin bundles over ONE engine: each supplies a
// `build_transport` seam, a reflect `sentence`, the family reflect fields and a
// readme; `io_bridge` supplies the verbs, the read loop, correlation and the
// inbound gate. This file is that thin half — `Engine.swift` is the engine,
// `Transports.swift` and `Relay.swift` are the two seams.
//
// Verbs (all six shared): boot · reconnect · forward · watch_remote ·
// unwatch_remote · reflect, plus the relay's four directory verbs
// (list_peers · watch_directory · unwatch_directory · set_identity), which on a
// ws_bridge are simply unknown types.

@_exported import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// `handler_module` of the WS derivation (ws / ssh+ws / memory transports).
public let WS_HANDLER_MODULE = "ws_bridge.tools"
/// `handler_module` of the RELAY derivation (the relay-kernel router transport).
public let RELAY_HANDLER_MODULE = "relay_connector.tools"

/// Which derivation a bundle instance is — python's two `make_verbs` calls.
public enum BridgeFamily: Sendable {
    case ws
    case relay

    var label: String { self == .ws ? "ws_bridge" : "relay_connector" }

    /// Python's `default_kind` — the reflect `transport` fallback only. `boot`
    /// defaults to `"ws"` for BOTH families (python `_engine.boot`).
    var defaultKind: String { self == .ws ? "ws" : "relay" }

    /// Python's reflect `sentence` for this derivation.
    var sentence: String {
        switch self {
        case .ws:
            return
                "Cross-kernel comms bridge — WS-only, asymmetric (no peer bridge needed); weak proxy."
        case .relay:
            return
                "Cross-kernel comms through a relay-kernel router — dial-out WS, group-password auth (X-Fantastic-Auth), GUID-routed; symmetric RPC tunneled over the relay; auto-reconnecting."
        }
    }

    /// The verb names this derivation answers, in python's table order.
    var verbNames: [String] {
        let shared = ["reflect", "boot", "reconnect", "forward", "watch_remote", "unwatch_remote"]
        return self == .ws
            ? shared
            : shared + ["list_peers", "watch_directory", "unwatch_directory", "set_identity"]
    }

    /// Python's reflect `verbs` — the first doc line of each, verbatim, in order.
    var verbs: JSON {
        var out: OrderedDictionary<String, JSON> = [:]
        for name in verbNames { out[name] = .string(BridgeDocs.first[name] ?? "") }
        return .object(out)
    }
}

/// The first line of each python verb docstring (`io_bridge/_engine.py` +
/// `relay_connector/tools.py`), which is what reflect publishes.
enum BridgeDocs {
    static let first: [String: String] = [
        "reflect": "Identity + transport + connectivity. No args.",
        "boot":
            "No args. Reads `transport` (kind) + transport-specific fields off the agent record, builds the transport, spawns the read loop, emits `bridge_up`. Idempotent: re-booting a connected bridge is a no-op.",
        "reconnect":
            "No args. Teardown + boot — explicit because we don't auto-reconnect on transport failure (keeps real network problems visible to operators / telemetry).",
        "forward":
            "args: target:str (req — id on the REMOTE kernel) + payload:dict (req — the call to run there; payload is ITSELF a verb frame and MUST carry its own \"type\") + timeout:float? (default 30s). Example: {\"type\":\"forward\",\"target\":\"src\",\"payload\":{\"type\":\"read_stream\",\"path\":\"data.bin\",\"offset\":0}}. Ships a raw `{type:'call', id, target, payload}` frame over the transport, awaits the matching reply, returns the unwrapped data. Local→local stays direct kernel.send (this verb is only for cross-kernel). Multi-hop loop detection is the caller's responsibility — the bridge ships whatever it's given.",
        "watch_remote":
            "args: target:str (req — id on the REMOTE kernel to watch). Sends `{type:'watch', src:<target>}` over the transport. Subsequent `{type:'event'}` frames from the remote arrive via the read loop and are re-emitted on THIS bridge agent's inbox. Local watchers subscribe to the bridge with `kernel.watch(<bridge_id>, ...)` and see the remote stream. Idempotent on the wire (web_ws de-dups via its own `watching` set).",
        "unwatch_remote":
            "args: target:str (req — id previously passed to watch_remote). Sends `{type:'unwatch', src:<target>}` to the remote so it stops emitting events for this subscription. Events already in-flight on the wire are still delivered + re-emitted.",
        "list_peers": "No args (optional timeout). One-shot snapshot of the relay directory:",
        "watch_directory":
            "No args. Subscribe to the relay directory; live `peer_joined`/`peer_left`/",
        "unwatch_directory":
            "No args. Stop the directory subscription started by `watch_directory`.",
        "set_identity":
            "Advertise/update this peer's DIRECTORY identity — the typed attrs the relay",
    ]

    /// Python `io_bridge.dispatch`'s teaching hint on an unknown type.
    static let envelopeHint =
        "every bridge frame needs a \"type\"; to reach a remote agent use "
        + "{\"type\":\"forward\",\"target\":\"<remote_id>\","
        + "\"payload\":{\"type\":\"<verb>\",...}} — the inner payload is itself a "
        + "verb frame and needs its own \"type\"."

    /// Python `_engine._reflect`'s `emits`.
    static let emits: JSON = [
        "bridge_up":
            "{type:'bridge_up'} — emitted on this agent's inbox after a successful boot",
        "bridge_down":
            "{type:'bridge_down'} — emitted when the transport drops (peer closed, tunnel died)",
        "<remote event>":
            "events from `watch_remote` subscriptions are re-emitted on this agent's inbox with their original `{type, ...}` shape",
    ]
}

public final class KernelBridgeBundle: AgentBundle, @unchecked Sendable {
    public let family: BridgeFamily
    public var name: String { family.label }
    public var verbs: [String] { family.verbNames }

    let stateLock = NSLock()
    var states: [AgentId: BridgeState] = [:]
    private var injectedTransports: [AgentId: any BridgeTransport] = [:]

    /// Default `.ws` keeps the in-memory test seam ergonomic.
    public init(family: BridgeFamily = .ws) {
        self.family = family
    }

    // MARK: - test seam (python `_test_transport_inject`)

    /// Inject a pre-built transport for `agentId`; the next `boot` of a leg whose
    /// record says `transport: "memory"` POPS it. Python's memory pair cannot be
    /// carried in `agent.yaml` either — the test wires it and then boots.
    public func injectTransport(agentId: AgentId, transport: any BridgeTransport) {
        stateLock.lock()
        injectedTransports[agentId] = transport
        stateLock.unlock()
    }

    /// Inject a `memory` transport serving `remote` in-process.
    public func injectMemoryTransport(agentId: AgentId, remote: Kernel) {
        injectTransport(agentId: agentId, transport: MemoryTransport(remote: remote))
    }

    func takeInjected(_ agentId: AgentId) -> (any BridgeTransport)? {
        stateLock.lock()
        defer { stateLock.unlock() }
        return injectedTransports.removeValue(forKey: agentId)
    }

    // MARK: - lifecycle

    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
        await teardown(agentId)
    }

    /// Python `on_delete` — cancels the read loop, closes the transport, kills
    /// the ssh tunnel (if any), rejects pending.
    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        await teardown(agentId)
    }

    public var readme: String? {
        family == .ws ? Self.wsReadme : Self.relayReadme
    }

    // MARK: - dispatch

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        guard family.verbNames.contains(verb) else { return unknownType(payload["type"]) }
        switch verb {
        case "reflect":
            return try await reflectVerb(agentId: agentId, kernel: kernel)
        case "boot":
            return await bootLeg(agentId: agentId, kernel: kernel)
        case "reconnect":
            // Python `_reconnect`: teardown + boot. Explicit, because a dropped
            // transport is never re-dialed on its own — a real network problem
            // stays visible to the operator instead of flapping in the dark.
            await teardown(agentId)
            return await bootLeg(agentId: agentId, kernel: kernel)
        case "forward":
            return try await forwardVerb(agentId: agentId, payload: payload, kernel: kernel)
        case "watch_remote":
            return await watchVerb(
                agentId: agentId, payload: payload, verb: "watch_remote", frameType: "watch",
                replyKey: "watching")
        case "unwatch_remote":
            return await watchVerb(
                agentId: agentId, payload: payload, verb: "unwatch_remote", frameType: "unwatch",
                replyKey: "unwatched")
        case "list_peers":
            guard let transport = try relayTransport(agentId, "list_peers") else {
                return notConnected("list_peers")
            }
            return await transport.listPeers(
                timeout: try pyFloat(payload.asObject?["timeout"] ?? .double(30.0)))
        case "watch_directory":
            guard let transport = try relayTransport(agentId, "watch_directory") else {
                return notConnected("watch_directory")
            }
            return await transport.watchDirectory()
        case "unwatch_directory":
            // Pre-boot this is simply OK — there is no subscription to stop.
            guard let transport = try relayTransport(agentId, "unwatch_directory") else {
                return .object(["ok": .bool(true)])
            }
            return await transport.unwatchDirectory()
        case "set_identity":
            return try await setIdentityVerb(agentId: agentId, payload: payload, kernel: kernel)
        default:
            return unknownType(payload["type"])
        }
    }

    /// `forward` carrying a raw request chunk / returning a raw reply chunk.
    /// Every other binary verb routes through the text `handle`.
    public func handleBinary(
        agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
    ) async throws -> (JSON?, Data?) {
        guard header["type"].asString == "forward" else {
            return (try await handle(agentId: agentId, payload: header, kernel: kernel), nil)
        }
        return try await binaryForwardVerb(
            agentId: agentId, header: header, blob: blob, kernel: kernel)
    }

    // MARK: - reflect

    /// Python `_engine._reflect` + the family's `reflect_fields`, in python's key
    /// order. The relay's fields OVERWRITE `connected` in place (a leg
    /// mid-reconnect reads false), so the key keeps its position.
    private func reflectVerb(agentId: AgentId, kernel: Kernel) async throws -> JSON {
        let meta = kernel.agent(agentId)?.meta ?? [:]
        let st = state(agentId)
        let recordKind = meta["transport"] ?? .null
        let kind =
            !st.transportKind.isEmpty
            ? st.transportKind : (pyTruthy(recordKind) ? pyStr(recordKind) : family.defaultKind)
        var connected = false
        if let t = st.transport { connected = await !t.isClosed() }
        var out: OrderedDictionary<String, JSON> = [
            "id": .string(agentId.value),
            "sentence": .string(family.sentence),
            "transport": .string(kind),
            "connected": .bool(connected),
            "pending_count": .integer(Int64(st.pendingCount)),
        ]
        // The active per-leg rule TYPE names — keyed by the field you SET, so
        // read-key == write-key and `reflect` → edit is a direct mirror.
        for (k, v) in describe(meta) { out[k] = v }
        switch family {
        case .ws:
            out["host"] = meta["host"] ?? .null
            out["peer_id"] = meta["peer_id"] ?? .null
            out["local_port"] = meta["local_port"] ?? .null
            out["remote_port"] = meta["remote_port"] ?? .null
            out["tunnel_pid"] = st.extra["tunnel_pid"] ?? .null
        case .relay:
            if let relay = st.transport as? RelayTransport {
                out["connected"] = .bool(await relay.isLive)
            } else {
                out["connected"] = .bool(false)
            }
            out["relay_url"] = meta["relay_url"] ?? .null
            out["guid"] = meta["guid"] ?? .null
            out["partner_guid"] = meta["partner_guid"] ?? .null
            out["reconnect"] = .double(try pyFloat(meta["reconnect"] ?? .double(DEFAULT_RECONNECT)))
            let role = meta["role"] ?? .null
            out["role"] = pyTruthy(role) ? role : .string("kernel")
            out["owner_guid"] = meta["owner_guid"] ?? .null
            let exposes = meta["exposes"] ?? .null
            out["exposes"] = pyTruthy(exposes) ? exposes : .array([])
        }
        out["verbs"] = family.verbs
        out["emits"] = BridgeDocs.emits
        return .object(out)
    }

    // MARK: - the relay directory verbs

    /// The live relay transport, or nil when the leg is not connected. A relay
    /// leg booted on another transport has no directory at all — python reaches
    /// for a method that is not there.
    private func relayTransport(_ agentId: AgentId, _ verb: String) throws -> RelayTransport? {
        guard let transport = state(agentId).transport else { return nil }
        guard let relay = transport as? RelayTransport else {
            throw PyError(
                "AttributeError", "'MemoryTransport' object has no attribute '\(verb)'")
        }
        return relay
    }

    private func notConnected(_ verb: String) -> JSON {
        Refusal.make(
            Refusal.notConnected, "relay_connector.\(verb): not connected",
            hint: "call boot first")
    }

    /// Python `_set_identity`: validate the role, merge the well-known keys into
    /// the record (persisted, so they re-announce next boot), push them live.
    private func setIdentityVerb(agentId: AgentId, payload: JSON, kernel: Kernel) async throws
        -> JSON
    {
        guard let transport = try relayTransport(agentId, "set_identity") else {
            return notConnected("set_identity")
        }
        // `"role" in payload and payload["role"] is not None and … not in ROLES`.
        if let role = payload.asObject?["role"], role != .null,
            !RELAY_ROLES.contains(pyStr(role))
        {
            return Refusal.bad("set_identity", "role", "one of \(RELAY_ROLES_TEXT)")
        }
        guard let agent = kernel.agent(agentId) else { return notConnected("set_identity") }
        var patch: OrderedDictionary<String, JSON> = [:]
        if case .object(let m) = payload {
            for k in ["role", "owner_guid", "exposes"] where m[k] != nil { patch[k] = m[k]! }
        }
        if !patch.isEmpty {
            // Through the kernel update path (publishes `updated`) — never a
            // direct persist, which the loader's writer never learns about.
            kernel.update(agent.id, patch)
        }
        return await transport.setIdentity(relayIdentity(kernel.agent(agentId)?.meta ?? [:]))
    }

    /// Python `_identity_from_record`: the attrs blob we advertise. Defaults are
    /// OMITTED (role=kernel / owner_guid=null / exposes=[]) so a plain peer
    /// advertises nothing and the wire stays identical to an un-typed leg.
    func relayIdentity(_ meta: OrderedDictionary<String, JSON>) -> JSON {
        var attrs: OrderedDictionary<String, JSON> = [:]
        let role = meta["role"] ?? .null
        if pyTruthy(role), pyStr(role) != "kernel" { attrs["role"] = role }
        if let owner = meta["owner_guid"], owner != .null { attrs["owner_guid"] = owner }
        let exposes = meta["exposes"] ?? .null
        if pyTruthy(exposes) { attrs["exposes"] = exposes }
        return .object(attrs)
    }

    /// Python `io_bridge.dispatch`'s miss: the common slip is a frame with no
    /// `type`, or a `forward` whose inner payload dropped its own.
    private func unknownType(_ t: JSON) -> JSON {
        .object([
            "error": .string("bridge: unknown type \(pyRepr(t))"),
            "hint": .string(BridgeDocs.envelopeHint),
        ])
    }

    // MARK: - readmes (python `readme.md`, byte for byte)

    static let wsReadme = """
        # ws_bridge — outbound WebSocket bridge

        Dials a remote `web_ws` surface. **Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`.
        Token on frame envelope (`auth_token`). Same `ingress_rule`/`egress_rule`/`auth` as every io_bridge leg.

        Verbs: `boot` · `reconnect` · `forward(target,payload[,timeout])` · `watch_remote(target)` · `unwatch_remote(target)` · `reflect`
        Transports: `ws` (default) · `ssh+ws` · `memory` (test)
        Emits: `bridge_up` · `bridge_down` · remote events via `watch_remote`

        """

    static let relayReadme = """
        # relay_connector — relay-kernel router bridge

        Dial a relay-KERNEL (`../fantastic_relay`) at `ws://<host>/<guid>` with the group
        password in `X-Fantastic-Auth` and subprotocol `fantastic.relay.v1`. Reach a
        partner kernel by its GUID; the relay routes by `target`. Same verbs/auth surface
        as `ws_bridge` (both ride the shared `io_bridge` engine) — `forward`,
        `watch_remote`/`unwatch_remote` work identically; replies and binary
        `read_stream` chunks are tunneled over the relay.

        **Sealed by default** — open the inbound leg: `update_agent <id> ingress_rule=allow_all_ingress`
        (or `password` for the group token). The bridge-leg `ingress_rule`/`egress_rule`
        gate the tunneled calls and are INDEPENDENT of the relay's own connection auth.

        **Self-healing** — the leg auto-connects (eager initial dial + background
        auto-connect) and auto-reconnects with the `reconnect` backoff (seconds, default
        10; `0` = one-shot/legacy, raise on first failure). `reflect.connected` tracks the
        LIVE socket. A no-reply `keepalive` verb is sent every `heartbeat` seconds to keep
        the relay's liveness view of us green.

        **Directory** (the relay's own `relay` agent, addressed `target:"relay"`, NOT the
        partner) — render/track the live peer list without polling:
        - `list_peers` → one-shot `{peers:[{guid, status(green|yellow|red), last_seen, since}]}`.
        - `watch_directory` / `unwatch_directory` → subscribe; live `peer_joined`/
          `peer_left`/`peer_evicted`/`peer_status` events re-emit on THIS connector's inbox
          (a local `kernel.watch(<connector_id>)` sees them).

        **Identity** — `guid` (our id = the WS path) is **auto-minted on first boot if
        absent and persisted** into the record, so every later hydration re-dials the SAME
        `ws://host/<guid>`; pass an explicit `guid` to pick your own (it always wins), and a
        minted one is never regenerated. `partner_guid` stays explicit (you can't invent a
        peer's address) — discover it via the partner's `reflect` or the directory
        (`list_peers`). Caveat: a persisted `guid` is a durable identity, so cloning a
        `.fantastic` store to another machine carries the same id and the relay will see a
        duplicate — re-mint by clearing `guid` on the clone.

        **Directory typing (kernelgroup)** — a connector advertises an opaque **attrs blob**
        to the relay (the relay STORES + reflects it into `list_peers` entries + a
        `peer_updated` event, and never interprets it). Well-known keys:
        - `role` — `"manager" | "kernel"` (default `kernel`). A **manager** owns kernels and
          exposes control; a **kernel** is driven (standalone, or owned by a manager).
        - `owner_guid` — the managing peer's **kernel-instance guid** (that peer's
          ROOT-record `guid`, surfaced as the top-level `guid` in its root `reflect` — the
          kernel's own persisted identity, NOT a relay LEG's `guid`; this connector's own
          `guid` below is the WS-path leg id, a separate identity). `null` = standalone.
        - `exposes` — an opaque control-surface list the peer advertises (e.g.
          `["stop","restart"]`).

        Set them at create (record meta) and they advertise on connect (re-announced on
        every reconnect — the relay drops per-connection state on a drop). `set_identity`
        (verb: optional `role`/`owner_guid`/`exposes`, merged) updates them live + persists,
        so a manager can publish/retract a kernel's control surface at runtime. **reach ≠
        control**: the directory carries who-owns-what; driving a kernel owned by another
        manager is a plain `forward` to that manager (gated by ingress, as always). A plain
        peer (no typed keys) advertises nothing — identical to an un-typed leg.

        Record fields: `relay_url` · `guid` (auto-minted if absent) · `partner_guid` (peer to
        reach) · `relay_token` (X-Fantastic-Auth) · `heartbeat` (s, default 30) ·
        `reconnect` (s, default 10) · `role` · `owner_guid` · `exposes`. Transport: `relay`.

        """
}
