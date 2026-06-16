// Cross-kernel bridge bundle (WS-only, asymmetric).
//
// Mirrors Python's `kernel_bridge.tools`. The bridge is an
// asymmetric client: it opens a WS to the remote kernel's `web_ws`
// surface and ships raw call frames. No B-side bridge agent is
// needed — the remote's `web_ws` handles inbound calls natively.
//
// Transports:
//   - InMemory: both kernels in one process (unit-test backbone)
//   - WebSocket: real WS via swift-nio (NIOWebSocketClient) against a
//     remote `web_ws` — cross-platform (macOS + Linux)
//
// (HTTP transport was removed — WS subsumes its request/reply
// semantic and adds streaming via the `watch`/`event` protocol.)
//
// Verbs:
//   - boot     : open the transport per the agent's meta record
//   - forward  : ship a `{type:"call", target, payload}` frame, await reply
//   - watch_remote / unwatch_remote : subscribe to remote agent emits;
//     `{type:"event"}` frames are re-emitted on this bridge's local inbox
//   - detach   : drop the attached transport
//   - shutdown : same as detach (substrate lifecycle hook)
//   - reflect  : identity + connectivity + verb docs

@_exported import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation

// The relay token-issuance leg is a plain HTTP POST via URLSession.data(for:)
// — that path IS implemented on Linux (swift-corelibs-foundation, libcurl), so
// it stays on URLSession. URLRequest/URLSession live in FoundationNetworking on
// Linux; in Foundation on Apple. (The WS dial uses swift-nio, not URLSession.)
#if canImport(FoundationNetworking)
    import FoundationNetworking
#endif

/// `handler_module` of the WS derivation (ws / memory transports).
public let WS_HANDLER_MODULE = "ws_bridge.tools"
/// `handler_module` of the RELAY derivation (the relay-kernel router transport).
public let RELAY_HANDLER_MODULE = "relay_connector.tools"

/// Which transport family a bundle admits — the only behavioural difference
/// between the two derivations (they share one engine, mirroring py's separate
/// `ws_bridge` + `relay_connector` bundles and rust's `Family`).
public enum BridgeFamily: Sendable {
    /// ws / memory (the `ws_bridge` derivation).
    case ws
    /// the relay-kernel router (`relay_connector` derivation).
    case relay

    var label: String { self == .ws ? "ws_bridge" : "relay_connector" }
    func admits(_ transportKind: String) -> Bool {
        switch self {
        case .ws: return transportKind == "ws" || transportKind == "memory"
        case .relay: return transportKind == "relay" || transportKind == "memory"
        }
    }
}

// ── Transport enum ─────────────────────────────────────────────────

/// Internal transport wrapper — discriminates the (currently two)
/// ways the bridge can reach a remote kernel.
enum BridgeTransport {
    case memory(InMemoryBridge)
    case ws(WebSocketTransport)
    case relay(RelayTransport)

    func forward(target: AgentId, payload: JSON) async -> JSON {
        switch self {
        case .memory(let bridge):
            return await bridge.forward(target: target, payload: payload)
        case .ws(let transport):
            return await transport.forward(target: target, payload: payload)
        case .relay(let transport):
            return await transport.forward(target: target, payload: payload)
        }
    }

    /// Binary forward — a `read_stream`/`write_stream` carried cross-kernel.
    func binaryForward(target: AgentId, header: JSON, blob: Data) async -> (JSON, Data) {
        switch self {
        case .memory(let bridge):
            return await bridge.binaryForward(target: target, header: header, blob: blob)
        case .ws(let transport):
            // Over-the-wire binary forward: a codec frame `[4B len|header|body]`
            // shipped as a binary WS message (mirrors rust's 4b). The remote
            // `web_ws.handleBinaryFrame` decodes it, gates, and replies with a
            // codec binary frame (read_stream) or a text reply (write_stream).
            return await transport.binaryForward(target: target, header: header, blob: blob)
        case .relay(let transport):
            // Tunneled over the relay as a binary WS message wrapped in a relay
            // `send` envelope; the partner connector decodes, gates, dispatches.
            return await transport.binaryForward(target: target, header: header, blob: blob)
        }
    }

    func watchRemote(target: AgentId) async -> JSON {
        switch self {
        case .memory(let bridge):
            return await bridge.watchRemote(target: target)
        case .ws(let transport):
            return await transport.watchRemote(target: target)
        case .relay(let transport):
            return await transport.watchRemote(target: target)
        }
    }

    func unwatchRemote(target: AgentId) async -> JSON {
        switch self {
        case .memory(let bridge):
            return await bridge.unwatchRemote(target: target)
        case .ws(let transport):
            return await transport.unwatchRemote(target: target)
        case .relay(let transport):
            return await transport.unwatchRemote(target: target)
        }
    }

    // Directory surface — relay_connector only (addresses the relay's `relay` agent).

    func listPeers(timeout: Double) async -> JSON {
        guard case .relay(let transport) = self else {
            return .object(["error": .string("transport has no relay directory")])
        }
        return await transport.listPeers(timeout: timeout)
    }

    func watchDirectory(timeout: Double) async -> JSON {
        guard case .relay(let transport) = self else {
            return .object(["error": .string("transport has no relay directory")])
        }
        return await transport.watchDirectory(timeout: timeout)
    }

    func unwatchDirectory() async -> JSON {
        guard case .relay(let transport) = self else {
            return .object(["ok": .bool(true)])
        }
        return await transport.unwatchDirectory()
    }

    func setIdentity(_ attrs: JSON) async -> JSON {
        guard case .relay(let transport) = self else {
            return .object(["error": .string("transport has no relay directory")])
        }
        return await transport.setIdentity(attrs)
    }

    /// Release transport resources (receive loop, heartbeat, relay socket).
    /// memory/ws legs are torn down by ARC; the relay leg holds long-lived
    /// Tasks + a WS, so it needs an explicit close.
    func close() async {
        if case .relay(let transport) = self {
            await transport.close()
        }
    }
}

// ── In-memory bridge ───────────────────────────────────────────────

public actor InMemoryBridge {
    private weak var remoteKernel: Kernel?
    /// The local bridge agent we re-emit `event` payloads on after a
    /// `watch` subscription. Set by `KernelBridgeBundle.attachInMemory`.
    private var localBridgeId: AgentId?
    private weak var localKernel: Kernel?
    /// One drain task per active watch_remote subscription. Cancelled
    /// on unwatch_remote so the synthetic relay inbox stops draining.
    private var watchTasks: [AgentId: Task<Void, Never>] = [:]
    /// Synthetic watcher ids per target — needed for `unwatch` so we
    /// remove the same id we registered in `watch`.
    private var watcherIds: [AgentId: AgentId] = [:]

    public init(remote: Kernel) {
        self.remoteKernel = remote
    }

    public func setLocalSink(agentId: AgentId, kernel: Kernel) {
        self.localBridgeId = agentId
        self.localKernel = kernel
    }

    public func forward(target: AgentId, payload: JSON) async -> JSON {
        guard let remote = remoteKernel else {
            return .object(["error": .string("remote kernel gone")])
        }
        return await remote.send(target, payload)
    }

    /// Binary forward — ships a raw chunk to the remote on the symmetric binary
    /// channel + returns `(reply, body)`. In-process, a direct `sendWithBinary`.
    public func binaryForward(target: AgentId, header: JSON, blob: Data) async -> (JSON, Data) {
        guard let remote = remoteKernel else {
            return (.object(["error": .string("remote kernel gone")]), Data())
        }
        return await remote.sendWithBinary(target, header, blob)
    }

    /// Wire up a watch on the remote kernel: every emit on
    /// `target`'s inbox is mirrored onto this bridge agent's local
    /// inbox via `localKernel.emit`. Uses the substrate's standard
    /// `watch(src:watcher:)` mechanism with a synthetic watcher id;
    /// a background Task drains the watcher's inbox AsyncStream and
    /// re-emits on the local sink.
    ///
    /// In-memory bridges are paired in tests; production uses
    /// `WebSocketTransport.watchRemote` which talks to the remote's
    /// `web_ws` server. The in-memory shape mirrors the WS path so
    /// tests don't drift from production semantics.
    public func watchRemote(target: AgentId) async -> JSON {
        guard let remote = remoteKernel else {
            return .object(["error": .string("remote kernel gone")])
        }
        guard let sink = localBridgeId, let local = localKernel else {
            return .object([
                "error": .string("in-memory bridge has no local sink (call setLocalSink)")
            ])
        }
        // Idempotent — re-watching same target is a no-op.
        if watchTasks[target] != nil {
            return .object([
                "ok": .bool(true),
                "watching": .string(target.value),
                "already": .bool(true),
            ])
        }
        let watcherId = AgentId(
            "inmem_relay_\(UUID().uuidString.prefix(8))_\(target.value)")
        watcherIds[target] = watcherId
        remote.watch(src: target, watcher: watcherId)
        let stream = remote.ensureInbox(watcherId)

        let task = Task {
            for await event in stream {
                if Task.isCancelled { break }
                await local.emit(sink, event)
            }
        }
        watchTasks[target] = task
        return .object([
            "ok": .bool(true),
            "watching": .string(target.value),
        ])
    }

    public func unwatchRemote(target: AgentId) async -> JSON {
        guard let remote = remoteKernel else {
            return .object(["error": .string("remote kernel gone")])
        }
        if let watcherId = watcherIds.removeValue(forKey: target) {
            remote.unwatch(src: target, watcher: watcherId)
        }
        watchTasks.removeValue(forKey: target)?.cancel()
        return .object([
            "ok": .bool(true),
            "unwatched": .string(target.value),
        ])
    }
}

// ── Bundle ─────────────────────────────────────────────────────────

public final class KernelBridgeBundle: AgentBundle, @unchecked Sendable {
    /// Which io_bridge derivation this instance is — the only difference between
    /// `ws_bridge.tools` and `relay_connector.tools` (one engine, two registrations).
    public let family: BridgeFamily
    public var name: String { family.label }
    private let lock = NSLock()
    private var bridges: [AgentId: BridgeTransport] = [:]

    /// Default `.ws` keeps the in-memory test seam (`attachInMemory`) ergonomic.
    public init(family: BridgeFamily = .ws) {
        self.family = family
    }

    /// Attach an in-memory bridge for `agentId` pointing at `remote`.
    /// The app calls this after creating a `kernel_bridge.tools` agent.
    /// `localKernel` is the kernel that owns `agentId` — needed so
    /// `watch_remote` can re-emit events on the local bridge's inbox.
    public func attachInMemory(agentId: AgentId, remote: Kernel, localKernel: Kernel) {
        let bridge = InMemoryBridge(remote: remote)
        // setLocalSink is an async-actor call but attach is sync; use
        // a Task to wire the sink eagerly. Forwards landing before
        // the sink is set still work (forward doesn't need sink).
        Task {
            await bridge.setLocalSink(agentId: agentId, kernel: localKernel)
        }
        attachTransport(agentId: agentId, transport: .memory(bridge))
    }

    /// Used by `Transports.swift`'s public attach helpers. Internal
    /// to the module — callers go through `attachInMemory` or
    /// `attachWebSocket`.
    func attachTransport(agentId: AgentId, transport: BridgeTransport) {
        lock.lock()
        defer { lock.unlock() }
        bridges[agentId] = transport
    }

    /// Drop the transport for `agentId`. Returns whether a transport
    /// was attached. A cloud leg is closed asynchronously (it owns a
    /// receive loop, heartbeat, and relay socket that must be released).
    @discardableResult
    public func detach(agentId: AgentId) -> Bool {
        lock.lock()
        let removed = bridges.removeValue(forKey: agentId)
        lock.unlock()
        if let removed {
            Task { await removed.close() }
        }
        return removed != nil
    }

    /// Sync lookup helper — keeps the NSLock outside any async scope.
    private func bridgeFor(_ agentId: AgentId) -> BridgeTransport? {
        lock.lock()
        defer { lock.unlock() }
        return bridges[agentId]
    }

    /// Assemble the directory-attributes blob to advertise from the record's
    /// well-known keys (`role`/`owner_guid`/`exposes`). Defaults are OMITTED
    /// (role=kernel / owner_guid=null / exposes=[]) so a plain peer advertises
    /// nothing — the relay's own defaults stand. Opaque to the relay; mirrors py
    /// `_identity_from_record` / rust `identity_from_meta`.
    private func relayIdentity(_ agent: Agent) -> JSON {
        var attrs: JSON = [:]
        if let role = agent.metaValue(forKey: "role")?.asString, role != "kernel" {
            attrs["role"] = .string(role)
        }
        if let owner = agent.metaValue(forKey: "owner_guid"), owner.asString != nil {
            attrs["owner_guid"] = owner
        }
        if let exposes = agent.metaValue(forKey: "exposes"), case .array(let a) = exposes,
            !a.isEmpty
        {
            attrs["exposes"] = exposes
        }
        return attrs
    }

    /// Boot a `relay_connector` leg: dial the relay-kernel router and attach.
    /// Mirrors the Rust `"relay"` boot arm + Python's `build_transport`. Required
    /// meta: `relay_url` (ws://host:port), `partner_guid` (the peer kernel to reach).
    /// `guid` (our id = the WS path) is auto-minted + persisted on first boot if
    /// absent (explicit wins, never regenerated). Optional: `relay_token`
    /// (the group password for X-Fantastic-Auth, default ""). No certs/mTLS — the
    /// relay auths the connection at the WS upgrade and routes by `target`.
    private func bootRelay(agentId: AgentId, agent: Agent, kernel: Kernel) async -> JSON {
        guard let relayURLStr = agent.metaValue(forKey: "relay_url")?.asString,
            let relayURL = URL(string: relayURLStr)
        else {
            return .object(["error": .string("relay_connector: relay_url required")])
        }
        // `guid` (our WS path) is auto-minted ONCE on first boot if absent and
        // persisted into the record, so every later hydration re-dials the SAME path;
        // an explicit guid always wins, and a minted one is never regenerated (read
        // verbatim next boot). Mirrors py/rust.
        let guid: String
        if let existing = agent.metaValue(forKey: "guid")?.asString, !existing.isEmpty {
            guid = existing
        } else {
            guid = UUID().uuidString.replacingOccurrences(of: "-", with: "").lowercased()
            _ = agent.updateMeta(["guid": .string(guid)])
            await kernel.persistRecord(agent)
        }
        guard let partnerGuid = agent.metaValue(forKey: "partner_guid")?.asString,
            !partnerGuid.isEmpty
        else {
            return .object(["error": .string("relay_connector: partner_guid required")])
        }
        let token = agent.metaValue(forKey: "relay_token")?.asString ?? ""
        // `reconnect` (s) backoff before each re-dial; default 10, 0 = one-shot.
        let reconnect = agent.metaValue(forKey: "reconnect")?.asDouble ?? 10
        // Directory typing: validate `role` and advertise the attrs blob on connect.
        if let role = agent.metaValue(forKey: "role")?.asString, role != "manager",
            role != "kernel"
        {
            return .object([
                "error": .string("relay_connector: role must be manager|kernel, got \(role)")
            ])
        }
        let identity = relayIdentity(agent)
        // Resolve the per-leg ingress + egress rules from the record (`ingress_rule`
        // / `egress_rule`, else the legacy `auth` shorthand). A bad rule fails loudly.
        let ingress: IngressRule
        let egress: EgressRule
        do {
            let auth = agent.metaValue(forKey: "auth")
            ingress = try resolveIngress(
                ingressRule: agent.metaValue(forKey: "ingress_rule"), auth: auth)
            egress = try resolveEgress(
                egressRule: agent.metaValue(forKey: "egress_rule"), auth: auth)
        } catch {
            return .object(["error": .string("relay_connector: bad auth rule: \(error)")])
        }
        do {
            let transport = try await RelayTransport.connect(
                relayURL: relayURL, guid: guid, token: token, partnerGuid: partnerGuid,
                reconnect: reconnect, identity: identity,
                localAgentId: agentId, localKernel: kernel, ingress: ingress, egress: egress)
            attachTransport(agentId: agentId, transport: .relay(transport))
            return .object([
                "booted": .bool(true),
                "transport": .string("relay"),
            ])
        } catch {
            return .object(["error": .string("relay_connector: connect failed: \(error)")])
        }
    }

    public var readme: String? {
        """
        kernel_bridge — cross-kernel comms (WS-only, asymmetric).
        Opens a WS to the remote's `web_ws` and ships raw call frames; \
        the remote dispatches `kernel.send` natively, no peer bridge. \
        Verbs: forward (await reply), watch_remote/unwatch_remote (stream \
        remote emits onto this bridge's inbox). Weak binding — remote is \
        addressed by URL + path only, no shared types.
        Transports: ws (default) · relay. relay (relay_connector) reaches a peer \
        through a relay-KERNEL router: dial ws://<host>/<guid> (group password in \
        X-Fantastic-Auth, subprotocol fantastic.relay.v1), the relay routes by \
        `target` and tunnels the bridge frames — symmetric RPC + binary streams \
        (raw bytes, no base64). Meta: transport=relay, relay_url, guid (our id = \
        WS path), partner_guid (peer to reach), relay_token (X-Fantastic-Auth).
        Authorization: two symmetric, typed per-leg rules — `ingress_rule` (the \
        inbound FILTER) and `egress_rule` (the outbound DECORATOR), each \
        {type, env}; a legacy `auth` shorthand sets both. Types: allow_all \
        (default) | deny_inbound (refuse inbound calls, reply \
        {reason:"unauthorized"}) | password (kernel-GROUP shared secret: ingress \
        checks the envelope auth_token against the group token from an env var, \
        default FANTASTIC_GROUP_TOKEN; egress presents it). These per-leg rules \
        gate the tunneled bridge calls, INDEPENDENT of the relay's own connection \
        auth. Resolved by name from the IngressRules/EgressRules registries. Swift \
        enforces ingress on the relay leg only — its sole inbound-call path (ws is \
        an asymmetric client; in-memory forward is a direct kernel call).
        """
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        switch verb {
        case "reflect":
            let attached = bridgeFor(agentId) != nil
            // The per-leg rule TYPE names (config like a `token_env` is never
            // surfaced). Read-key == write-key (py parity): `ingress_rule` (inbound
            // FILTER, default deny_inbound — the SEAL) / `egress_rule` (outbound
            // DECORATOR, default silent); `auth` is a legacy WRITE shorthand only,
            // never reflected. Swift gates only the relay_connector inbound-call path.
            let a = kernel.agent(agentId)
            let authMeta = a?.metaValue(forKey: "auth")
            // SEALED BY DEFAULT — an io leg with no rule reflects as deny_inbound.
            let ingressName = ruleName(
                a?.metaValue(forKey: "ingress_rule") ?? authMeta, default: "deny_inbound")
            let egressName = ruleName(
                a?.metaValue(forKey: "egress_rule") ?? authMeta, default: "silent")
            return [
                "id": .string(agentId.value),
                "kind": .string(family.label),
                "sentence": .string(
                    "Cross-kernel transport (WS-only, asymmetric; in-memory test backbone)."
                ),
                // `connected` is the canonical field (python/rust); the transport
                // attaches only after a successful boot/handshake, so it tracks
                // attachment. `attached` kept as a swift-side alias.
                "connected": .bool(attached),
                "attached": .bool(attached),
                "ingress_rule": .string(ingressName),
                "egress_rule": .string(egressName),
                "sealed": .bool(ingressName != "allow_all"),
                "verbs": [
                    "forward":
                        "args: target, payload. Ships a raw `{type:'call', target, payload}` frame over the transport and returns the reply.",
                    "watch_remote":
                        "args: target. Subscribes to a remote agent's emits. Events are re-emitted on this bridge's local inbox.",
                    "unwatch_remote":
                        "args: target. Symmetric teardown of watch_remote.",
                    "detach": "Drops the attached transport.",
                ] as JSON,
            ] as JSON
        case "boot":
            // Idempotent: if already attached, no-op.
            if bridgeFor(agentId) != nil {
                return .object(["already": .bool(true)])
            }
            // Read transport config from the agent's meta record
            // (matches Python's `_boot` in
            // kernel_bridge/tools.py). Required field: `transport`.
            // For "ws": `host` + `local_port` + `peer_id` (the WS
            //   path segment on the remote — typically the id of a
            //   web_ws-served agent like `core`).
            // For "memory": injection-only (use `attachInMemory`).
            guard let agent = kernel.agent(agentId) else {
                return .object(["error": .string("no agent")])
            }
            let kind =
                agent.metaValue(forKey: "transport")?.asString
                ?? (family == .relay ? "relay" : "ws")
            // The derivation only opens transports in its own family — a
            // relay_connector can't open a ws socket and vice-versa (the split).
            guard family.admits(kind) else {
                return .object([
                    "error": .string(
                        "\(family.label): transport \"\(kind)\" not in this derivation")
                ])
            }
            // NOTE: the `auth` policy (deny_inbound) is enforced only on the
            // relay_connector leg below — Swift's sole inbound-`call` dispatcher.
            // The ws transport is an asymmetric client (no inbound calls) and the
            // in-memory forward is a direct kernel call, so there is no inbound
            // frame to gate on those paths (matches the relay e2e coverage).
            switch kind {
            case "ws":
                guard let host = agent.metaValue(forKey: "host")?.asString,
                    let port = agent.metaValue(forKey: "local_port")?.asInt,
                    let peerId = agent.metaValue(forKey: "peer_id")?.asString
                else {
                    return .object([
                        "error": .string(
                            "bridge: ws transport requires host + local_port + peer_id meta"
                        )
                    ])
                }
                let url = URL(string: "ws://\(host):\(port)/\(peerId)/ws")
                guard let url else {
                    return .object([
                        "error": .string("bridge: malformed ws url")
                    ])
                }
                await attachWebSocket(agentId: agentId, endpoint: url, kernel: kernel)
                return .object([
                    "booted": .bool(true),
                    "transport": .string("ws"),
                ])
            case "memory":
                return .object([
                    "error": .string(
                        "bridge: memory transport requires explicit attachInMemory")
                ])
            case "relay":
                return await bootRelay(agentId: agentId, agent: agent, kernel: kernel)
            default:
                return .object([
                    "error": .string("bridge: unknown transport \(kind)")
                ])
            }
        case "shutdown":
            _ = detach(agentId: agentId)
            return .object(["ok": .bool(true)])
        case "detach":
            let removed = detach(agentId: agentId)
            return .object(["ok": .bool(true), "removed": .bool(removed)])
        case "forward":
            // Python is canonical: the arg name is `target`.
            let targetStr = payload["target"].asString
            guard let targetStr else {
                return .object([
                    "error": .string(
                        "bridge.forward: target (str) + payload (dict) required"
                    )
                ])
            }
            guard let transport = bridgeFor(agentId) else {
                return .object([
                    "error": .string("no bridge attached"),
                    "reason": .string("not_attached"),
                ])
            }
            return await transport.forward(
                target: AgentId(targetStr),
                payload: payload["payload"])
        case "watch_remote":
            let targetStr = payload["target"].asString
            guard let targetStr else {
                return .object([
                    "error": .string("bridge.watch_remote: target (str) required")
                ])
            }
            guard let transport = bridgeFor(agentId) else {
                return .object([
                    "error": .string("no bridge attached"),
                    "reason": .string("not_attached"),
                ])
            }
            return await transport.watchRemote(target: AgentId(targetStr))
        case "unwatch_remote":
            let targetStr = payload["target"].asString
            guard let targetStr else {
                return .object([
                    "error": .string("bridge.unwatch_remote: target (str) required")
                ])
            }
            guard let transport = bridgeFor(agentId) else {
                return .object([
                    "error": .string("no bridge attached"),
                    "reason": .string("not_attached"),
                ])
            }
            return await transport.unwatchRemote(target: AgentId(targetStr))
        case "list_peers", "watch_directory", "unwatch_directory":
            // Directory surface (relay_connector): addresses the relay's own `relay`
            // agent (target:"relay"), not the partner.
            guard let transport = bridgeFor(agentId) else {
                return .object([
                    "error": .string("relay_connector.\(verb): not connected (call boot first)"),
                    "reason": .string("not_attached"),
                ])
            }
            let timeout = payload["timeout"].asDouble ?? 30
            switch verb {
            case "list_peers": return await transport.listPeers(timeout: timeout)
            case "watch_directory": return await transport.watchDirectory(timeout: timeout)
            default: return await transport.unwatchDirectory()
            }
        case "set_identity":
            // Advertise/update this peer's directory typing (role/owner_guid/exposes):
            // persisted (so it re-announces next boot) + pushed to the relay live.
            guard let transport = bridgeFor(agentId) else {
                return .object([
                    "error": .string(
                        "relay_connector.set_identity: not connected (call boot first)"),
                    "reason": .string("not_attached"),
                ])
            }
            if let role = payload["role"].asString, role != "manager", role != "kernel" {
                return .object([
                    "error": .string("relay_connector.set_identity: role must be manager|kernel")
                ])
            }
            guard let agent = kernel.agent(agentId) else {
                return .object(["error": .string("relay_connector.set_identity: no agent")])
            }
            // Merge only the provided well-known keys into the record + persist (an
            // explicit null retracts a field, e.g. owner_guid:null = become standalone).
            var changed = false
            if case .object(let m) = payload {
                for k in ["role", "owner_guid", "exposes"] where m[k] != nil {
                    _ = agent.updateMeta([k: m[k]!])
                    changed = true
                }
            }
            if changed { await kernel.persistRecord(agent) }
            return await transport.setIdentity(relayIdentity(agent))
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    /// Binary forward — `forward` carrying a raw request chunk (write_stream over
    /// the wire) / returning a raw reply chunk (read_stream). The local caller
    /// does `sendWithBinary(<bridge>, {type:forward, target, payload}, blob)`;
    /// in-process this streams cross-kernel directly. Non-`forward` binary verbs
    /// route through the text `handle` (the blob is unused).
    public func handleBinary(
        agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
    ) async throws -> (JSON?, Data) {
        let verb = header["type"].asString ?? ""
        guard verb == "forward" else {
            let reply = try await handle(agentId: agentId, payload: header, kernel: kernel)
            return (reply, Data())
        }
        guard let target = header["target"].asString, case .object = header["payload"] else {
            return (
                .object(["error": .string("bridge.forward: target + payload (object) required")]),
                Data()
            )
        }
        guard let transport = bridgeFor(agentId) else {
            return (
                .object(["error": .string("bridge.forward: not connected (call boot first)")]),
                Data()
            )
        }
        return await transport.binaryForward(
            target: AgentId(target), header: header["payload"], blob: blob)
    }
}
