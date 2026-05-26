// Cross-kernel bridge bundle.
//
// Mirrors Rust's `fantastic-kernel-bridge::KernelBridgeBundle`.
// Lets one kernel address agents living in another kernel via four
// transports:
//   - InMemory: both kernels in one process (shipped Phase 3)
//   - WebSocket: tokio-tungstenite client → remote WS endpoint (8E)
//   - HTTP: URLSession POST → remote HTTP endpoint (8E)
//   - SSH+WS: subprocess ssh -L tunnel + WS over tunnel (future)
//
// Bundle's `forward` verb routes to whichever transport is currently
// attached for the bridge agent's id.

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "kernel_bridge.tools"

// ── Transport enum ─────────────────────────────────────────────────

/// Internal transport wrapper — discriminates the three (eventually
/// four) ways the bridge can reach a remote kernel.
enum BridgeTransport {
    case memory(InMemoryBridge)
    case http(HttpTransport)
    case ws(WebSocketTransport)

    func forward(target: AgentId, payload: JSON) async -> JSON {
        switch self {
        case .memory(let bridge):
            return await bridge.forward(target: target, payload: payload)
        case .http(let transport):
            return await transport.forward(target: target, payload: payload)
        case .ws(let transport):
            return await transport.forward(target: target, payload: payload)
        }
    }
}

// ── In-memory bridge ───────────────────────────────────────────────

public actor InMemoryBridge {
    private weak var remoteKernel: Kernel?

    public init(remote: Kernel) {
        self.remoteKernel = remote
    }

    public func forward(target: AgentId, payload: JSON) async -> JSON {
        guard let remote = remoteKernel else {
            return .object(["error": .string("remote kernel gone")])
        }
        return await remote.send(target, payload)
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
    public func attachInMemory(agentId: AgentId, remote: Kernel) {
        attachTransport(agentId: agentId, transport: .memory(InMemoryBridge(remote: remote)))
    }

    /// Used by `Transports.swift`'s public attach helpers. Internal
    /// to the module — callers go through `attachInMemory`,
    /// `attachHttp`, or `attachWebSocket`.
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
                    "Cross-kernel transport (in-memory + HTTP + WebSocket)."),
                "attached": .bool(attached),
                "verbs": [
                    "forward":
                        "args: target_id, payload. Forwards to the attached remote kernel.",
                    "detach": "Drops the attached transport.",
                ] as JSON,
            ] as JSON
        case "boot", "shutdown":
            return .object(["ok": .bool(true)])
        case "detach":
            let removed = detach(agentId: agentId)
            return .object(["ok": .bool(true), "removed": .bool(removed)])
        case "forward":
            guard let targetStr = payload["target_id"].asString else {
                return .object(["error": .string("forward requires target_id")])
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
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }
}
