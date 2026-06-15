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
/// `handler_module` of the CLOUD derivation (the zero-trust relay transport).
public let CLOUD_HANDLER_MODULE = "cloud_bridge.tools"

/// Which transport family a bundle admits — the only behavioural difference
/// between the two derivations (they share one engine, mirroring py's separate
/// `ws_bridge` + `cloud_bridge` bundles and rust's `Family`).
public enum BridgeFamily: Sendable {
    /// ws / memory (the `ws_bridge` derivation).
    case ws
    /// the relay (`cloud_bridge` derivation).
    case cloud

    var label: String { self == .ws ? "ws_bridge" : "cloud_bridge" }
    func admits(_ transportKind: String) -> Bool {
        switch self {
        case .ws: return transportKind == "ws" || transportKind == "memory"
        case .cloud: return transportKind == "cloud_bridge"
        }
    }
}

// ── Transport enum ─────────────────────────────────────────────────

/// Internal transport wrapper — discriminates the (currently two)
/// ways the bridge can reach a remote kernel.
enum BridgeTransport {
    case memory(InMemoryBridge)
    case ws(WebSocketTransport)
    case cloud(CloudBridgeTransport)

    func forward(target: AgentId, payload: JSON) async -> JSON {
        switch self {
        case .memory(let bridge):
            return await bridge.forward(target: target, payload: payload)
        case .ws(let transport):
            return await transport.forward(target: target, payload: payload)
        case .cloud(let transport):
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
        case .cloud(let transport):
            // Cloud tunnels the codec frame as a tag-1 binary record `[4B len|
            // tag(1B)|wire]` over the mTLS relay (mirrors rust's cloud wire).
            return await transport.binaryForward(target: target, header: header, blob: blob)
        }
    }

    func watchRemote(target: AgentId) async -> JSON {
        switch self {
        case .memory(let bridge):
            return await bridge.watchRemote(target: target)
        case .ws(let transport):
            return await transport.watchRemote(target: target)
        case .cloud(let transport):
            return await transport.watchRemote(target: target)
        }
    }

    func unwatchRemote(target: AgentId) async -> JSON {
        switch self {
        case .memory(let bridge):
            return await bridge.unwatchRemote(target: target)
        case .ws(let transport):
            return await transport.unwatchRemote(target: target)
        case .cloud(let transport):
            return await transport.unwatchRemote(target: target)
        }
    }

    /// Release transport resources (receive loop, heartbeat, relay socket).
    /// memory/ws legs are torn down by ARC; the cloud leg holds long-lived
    /// Tasks + a WS, so it needs an explicit close.
    func close() async {
        if case .cloud(let transport) = self {
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
    /// `ws_bridge.tools` and `cloud_bridge.tools` (one engine, two registrations).
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

    /// The TokenSource seam (cloud_bridge does NOT authenticate or mint): a literal
    /// `token`, else POST the relay's `/issue` control-plane endpoint with
    /// `provider`/`password`. Provider-agnostic — `provider` selects the auth method
    /// (password today; Apple/Google later = the same call, a different provider).
    private func resolveCloudToken(agent: Agent) async -> (token: String?, error: String?) {
        if let t = agent.metaValue(forKey: "token")?.asString {
            return (t, nil)
        }
        guard let urlStr = agent.metaValue(forKey: "issue_url")?.asString,
            let url = URL(string: urlStr)
        else {
            return (nil, "cloud_bridge: token or issue_url required")
        }
        let body: [String: String] = [
            "provider": agent.metaValue(forKey: "provider")?.asString ?? "password",
            "credential": agent.metaValue(forKey: "password")?.asString ?? "",
            "peer_id": agent.metaValue(forKey: "peer_id")?.asString ?? "",
            "partner_peer_id": agent.metaValue(forKey: "partner_peer_id")?.asString ?? "",
            "rendezvous": agent.metaValue(forKey: "rendezvous")?.asString ?? "",
        ]
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try? JSONSerialization.data(withJSONObject: body)
        req.timeoutInterval = 10
        do {
            let (data, resp) = try await URLSession.shared.data(for: req)
            if let http = resp as? HTTPURLResponse, !(200...299).contains(http.statusCode) {
                return (nil, "cloud_bridge: issue endpoint denied (HTTP \(http.statusCode))")
            }
            let token = String(decoding: data, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            return token.isEmpty
                ? (nil, "cloud_bridge: issue endpoint returned no token")
                : (token, nil)
        } catch {
            return (nil, "cloud_bridge: issue endpoint request: \(error)")
        }
    }

    /// Boot a `cloud_bridge` leg: dial the relay, run peer↔peer mTLS, attach.
    /// Mirrors the Rust `"cloud_bridge"` boot arm + Python's `build_transport`.
    /// Required meta: `relay_url`, `id_key` (b64url Ed25519 seed),
    /// `approved_peer_certs` (PEM list), `token`. TLS role: `tls_role`
    /// ("server"/"client") | `initiator` (bool) | derived from
    /// `peer_id`/`partner_peer_id` (server = peer_id >= partner_peer_id, so the
    /// lexicographically-smaller peer initiates).
    private func bootCloudBridge(agentId: AgentId, agent: Agent, kernel: Kernel) async -> JSON {
        guard let relayURLStr = agent.metaValue(forKey: "relay_url")?.asString,
            let relayURL = URL(string: relayURLStr)
        else {
            return .object(["error": .string("cloud_bridge: relay_url required")])
        }
        guard let idKeyB64 = agent.metaValue(forKey: "id_key")?.asString,
            let idKey = CloudCert.b64urlDecode(idKeyB64)
        else {
            return .object(["error": .string("cloud_bridge: id_key (b64url) required")])
        }
        let approved = (agent.metaValue(forKey: "approved_peer_certs")?.asArray ?? [])
            .compactMap { $0.asString }
        guard !approved.isEmpty else {
            return .object(["error": .string("cloud_bridge: approved_peer_certs required")])
        }
        let (resolvedToken, tokenError) = await resolveCloudToken(agent: agent)
        guard let token = resolvedToken else {
            return .object(["error": .string(tokenError ?? "cloud_bridge: token unavailable")])
        }
        let server: Bool
        switch agent.metaValue(forKey: "tls_role")?.asString {
        case "server": server = true
        case "client": server = false
        default:
            if let initiator = agent.metaValue(forKey: "initiator")?.asBool {
                server = !initiator
            } else {
                let peer = agent.metaValue(forKey: "peer_id")?.asString ?? ""
                let partner = agent.metaValue(forKey: "partner_peer_id")?.asString ?? ""
                guard !partner.isEmpty else {
                    return .object([
                        "error": .string(
                            "cloud_bridge: need tls_role, initiator, or partner_peer_id")
                    ])
                }
                server = peer >= partner
            }
        }
        let certDER: [UInt8]
        let keyPKCS8: [UInt8]
        do {
            (certDER, keyPKCS8) = try CloudCert.selfSigned(idKey: idKey)
        } catch {
            return .object(["error": .string("cloud_bridge: cert: \(error)")])
        }
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
            return .object(["error": .string("cloud_bridge: bad auth rule: \(error)")])
        }
        do {
            let wsChannel = try await WSByteChannel.connect(relayURL: relayURL, token: token)
            let transport = try await CloudBridgeTransport.connect(
                channel: wsChannel, server: server,
                certDER: certDER, keyPKCS8: keyPKCS8, approvedPeerPEMs: approved,
                localAgentId: agentId, localKernel: kernel, ingress: ingress, egress: egress)
            attachTransport(agentId: agentId, transport: .cloud(transport))
            return .object([
                "booted": .bool(true),
                "transport": .string("cloud_bridge"),
                "role": .string(server ? "server" : "client"),
            ])
        } catch {
            return .object(["error": .string("cloud_bridge: connect failed: \(error)")])
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
        Transports: ws (default) · cloud_bridge. cloud_bridge reaches a peer \
        through a zero-trust relay: both peers dial OUT (WSS), the relay pairs \
        + forwards opaque frames, and the peers run peer↔peer TLS 1.3 mutual \
        auth (self-signed Ed25519 device certs, pinned by PUBLIC KEY) — so the \
        relay sees only ciphertext. Meta: transport=cloud_bridge, relay_url, \
        id_key (b64url Ed25519), approved_peer_certs (PEM list), a token source \
        (token | issue_url+password+provider — POST the relay's /issue), \
        tls_role|initiator|partner_peer_id.
        Authorization: two symmetric, typed per-leg rules — `ingress_rule` (the \
        inbound FILTER) and `egress_rule` (the outbound DECORATOR), each \
        {type, env}; a legacy `auth` shorthand sets both. Types: allow_all \
        (default) | deny_inbound (refuse inbound calls, reply \
        {reason:"unauthorized"}) | password (kernel-GROUP shared secret: ingress \
        checks the envelope auth_token against the group token from an env var, \
        default FANTASTIC_GROUP_TOKEN; egress presents it). Resolved by name from \
        the IngressRules/EgressRules registries. Swift enforces ingress on the \
        cloud_bridge leg only — its sole inbound-call path (ws is an asymmetric \
        client; in-memory forward is a direct kernel call).
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
            // never reflected. Swift gates only the cloud_bridge inbound-call path.
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
                ?? (family == .cloud ? "cloud_bridge" : "ws")
            // The derivation only opens transports in its own family — a
            // cloud_bridge can't open a ws socket and vice-versa (the split).
            guard family.admits(kind) else {
                return .object([
                    "error": .string(
                        "\(family.label): transport \"\(kind)\" not in this derivation")
                ])
            }
            // NOTE: the `auth` policy (deny_inbound) is enforced only on the
            // cloud_bridge leg below — Swift's sole inbound-`call` dispatcher. The
            // ws transport is an asymmetric client (no inbound calls) and the
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
            case "cloud_bridge":
                return await bootCloudBridge(agentId: agentId, agent: agent, kernel: kernel)
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
