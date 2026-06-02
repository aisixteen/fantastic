// Cross-kernel bridge bundle (WS-only, asymmetric).
//
// Mirrors Python's `kernel_bridge.tools`. The bridge is an
// asymmetric client: it opens a WS to the remote kernel's `web_ws`
// surface and ships raw call frames. No B-side bridge agent is
// needed — the remote's `web_ws` handles inbound calls natively.
//
// Transports:
//   - InMemory: both kernels in one process (unit-test backbone)
//   - WebSocket: real WS via URLSession against a remote `web_ws`
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

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "kernel_bridge.tools"

// ── Transport enum ─────────────────────────────────────────────────

/// Internal transport wrapper — discriminates the (currently two)
/// ways the bridge can reach a remote kernel.
enum BridgeTransport {
    case memory(InMemoryBridge)
    case ws(WebSocketTransport)

    func forward(target: AgentId, payload: JSON) async -> JSON {
        switch self {
        case .memory(let bridge):
            return await bridge.forward(target: target, payload: payload)
        case .ws(let transport):
            return await transport.forward(target: target, payload: payload)
        }
    }

    func watchRemote(target: AgentId) async -> JSON {
        switch self {
        case .memory(let bridge):
            return await bridge.watchRemote(target: target)
        case .ws(let transport):
            return await transport.watchRemote(target: target)
        }
    }

    func unwatchRemote(target: AgentId) async -> JSON {
        switch self {
        case .memory(let bridge):
            return await bridge.unwatchRemote(target: target)
        case .ws(let transport):
            return await transport.unwatchRemote(target: target)
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
    public let name = "kernel_bridge"
    private let lock = NSLock()
    private var bridges: [AgentId: BridgeTransport] = [:]

    public init() {}

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
    /// was attached.
    @discardableResult
    public func detach(agentId: AgentId) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return bridges.removeValue(forKey: agentId) != nil
    }

    /// Sync lookup helper — keeps the NSLock outside any async scope.
    private func bridgeFor(_ agentId: AgentId) -> BridgeTransport? {
        lock.lock()
        defer { lock.unlock() }
        return bridges[agentId]
    }

    public var readme: String? {
        """
        kernel_bridge — cross-kernel comms (WS-only, asymmetric).
        Opens a WS to the remote's `web_ws` and ships raw call frames; \
        the remote dispatches `kernel.send` natively, no peer bridge. \
        Verbs: forward (await reply), watch_remote/unwatch_remote (stream \
        remote emits onto this bridge's inbox). Weak binding — remote is \
        addressed by URL + path only, no shared types.
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
            return [
                "id": .string(agentId.value),
                "kind": .string("kernel_bridge"),
                "sentence": .string(
                    "Cross-kernel transport (WS-only, asymmetric; in-memory test backbone)."
                ),
                "attached": .bool(attached),
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
            let kind = agent.metaValue(forKey: "transport")?.asString ?? "ws"
            switch kind {
            case "ws":
                guard let host = agent.metaValue(forKey: "host")?.asString,
                    let port = agent.metaValue(forKey: "local_port")?.asInt,
                    let peerId = agent.metaValue(forKey: "peer_id")?.asString
                else {
                    return .object([
                        "error": .string(
                            "kernel_bridge: ws transport requires host + local_port + peer_id meta"
                        )
                    ])
                }
                let url = URL(string: "ws://\(host):\(port)/\(peerId)/ws")
                guard let url else {
                    return .object([
                        "error": .string("kernel_bridge: malformed ws url")
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
                        "kernel_bridge: memory transport requires explicit attachInMemory")
                ])
            default:
                return .object([
                    "error": .string("kernel_bridge: unknown transport \(kind)")
                ])
            }
        case "shutdown":
            _ = detach(agentId: agentId)
            return .object(["ok": .bool(true)])
        case "detach":
            let removed = detach(agentId: agentId)
            return .object(["ok": .bool(true), "removed": .bool(removed)])
        case "forward":
            // Python is canonical: arg name is `target` (no
            // `target_id` legacy). Accept `target_id` too for
            // backward compat with older Swift callers.
            let targetStr =
                payload["target"].asString
                ?? payload["target_id"].asString
            guard let targetStr else {
                return .object([
                    "error": .string(
                        "kernel_bridge.forward: target (str) + payload (dict) required"
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
                    "error": .string("kernel_bridge.watch_remote: target (str) required")
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
                    "error": .string("kernel_bridge.unwatch_remote: target (str) required")
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
}
