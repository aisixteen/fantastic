// Public API shim layer.
//
// Surface-compatible with the Rust UniFFI export (rust/crates/
// fantastic-uniffi/src/lib.rs). The Apple app holds an opaque
// `Kernel` handle returned by `startKernelInMemory` /
// `startKernel`; every method below is a thin JSON-string shim
// over the native Swift methods on `Kernel`.
//
// Why string boundaries: keeps the surface identical to UniFFI so
// app-side call sites (`kernel.sendJson(...)`,
// `kernel.registerProxyAgent(...)`) don't need to change.
// Internally the Swift implementation works against typed `JSON`
// values; the shim just parses on the way in and serializes on the
// way out.

import FantasticJSON
import Foundation

// MARK: - State listener callback

/// Callback the embedding app implements to receive kernel state
/// events. Mirrors Rust's `StateListener` UniFFI callback. The
/// `eventJson` parameter is a JSON-serialized state event:
///   {"type":"send"|"emit"|"created"|"removed"|"updated", ...}
public protocol StateListener: AnyObject, Sendable {
    func onEvent(eventJson: String)
}

extension Kernel {
    /// Bridge a Swift `StateListener` to the native subscriber API.
    /// Returns the opaque token the caller uses to detach.
    @discardableResult
    public func subscribe(listener: StateListener) -> SubscriberToken {
        return subscribe { [weak listener] event in
            listener?.onEvent(eventJson: event.serialize())
        }
    }
}

// MARK: - Kernel error (matches UniFFI's KernelError variants)

public enum KernelStartupError: Error, Sendable {
    case workdirInvalid(String)
    case portBindFailed(String)
    case bootFailed(String)
    case alreadyRunning
    case invalidSnapshot(String)
    case internalError(String)
}

// MARK: - HTTP port accessor (real binding lands in 8B)

extension Kernel {
    /// The port the HTTP listener is bound to, or 0 if no listener
    /// is running. Real implementation lands with the Hummingbird
    /// HTTP listener in phase 8B; pre-listener kernels report 0.
    public func httpPort() -> UInt16 {
        return httpPortValue
    }

    /// Phase 8B sets this when the listener binds. Public so the
    /// `FantasticWeb` target can write it.
    public func setHttpPort(_ port: UInt16) {
        httpPortLock.lock()
        defer { httpPortLock.unlock() }
        _httpPort = port
    }

    fileprivate var httpPortValue: UInt16 {
        httpPortLock.lock()
        defer { httpPortLock.unlock() }
        return _httpPort
    }
}

// MARK: - JSON-string shims (sendJson, sendJsonAs, proxyEmit)

extension Kernel {
    /// JSON-in, JSON-out RPC. Parses the payload, dispatches via
    /// `send`, serializes the reply. Matches Rust's
    /// `sendJson` UniFFI signature.
    public func sendJson(targetId: String, payloadJson: String) async -> String {
        let payload: JSON
        do {
            payload = try JSON.parse(payloadJson)
        } catch {
            return JSON.object([
                "error": .string("sendJson: payload not valid JSON: \(error)")
            ]).serialize()
        }
        let reply = await send(AgentId(targetId), payload)
        return reply.serialize()
    }

    /// JSON-in, JSON-out RPC with explicit sender attribution.
    /// State events tag the dispatch as originating from `senderId`.
    public func sendJsonAs(
        senderId: String,
        targetId: String,
        payloadJson: String
    ) async -> String {
        let payload: JSON
        do {
            payload = try JSON.parse(payloadJson)
        } catch {
            return JSON.object([
                "error": .string("sendJsonAs: payload not valid JSON: \(error)")
            ]).serialize()
        }
        let reply = await sendAs(
            sender: AgentId(senderId),
            target: AgentId(targetId),
            payload: payload
        )
        return reply.serialize()
    }

    /// Fire an event into `agentId`'s inbox without dispatching.
    /// JSON-string boundary mirroring UniFFI's `proxyEmit`.
    public func proxyEmit(agentId: String, eventJson: String) async {
        let event: JSON
        do {
            event = try JSON.parse(eventJson)
        } catch {
            return  // silently drop malformed JSON, same as Rust UniFFI
        }
        await emit(AgentId(agentId), event)
    }
}

// MARK: - Tool registry shims (registerTool / unregisterTool / ...)

extension Kernel {
    /// Register a tool in the LLM tool registry. Internally routes
    /// through `kernel.send("tools", {register, ...})`. Returns a
    /// JSON string `{ok: true, name}` or `{error, reason}`.
    public func registerTool(
        senderId: String,
        name: String,
        agentId: String,
        verb: String?,
        description: String,
        parametersSchemaJson: String
    ) async -> String {
        let schema: JSON
        do {
            schema = try JSON.parse(parametersSchemaJson)
        } catch {
            return JSON.object([
                "error": .string("registerTool: parameters_schema not valid JSON"),
                "reason": .string("invalid_args"),
            ]).serialize()
        }
        var payload: JSON = [
            "type": .string("register"),
            "name": .string(name),
            "agent_id": .string(agentId),
            "description": .string(description),
            "parameters_schema": schema,
            "sender": .string(senderId),
        ]
        if let v = verb {
            payload["verb"] = .string(v)
        }
        let reply = await send(AgentId("tools"), payload)
        return reply.serialize()
    }

    /// Drop a tool by name. Returns `{ok: true, name}` on hit,
    /// `{error, reason: "not_found"}` otherwise.
    public func unregisterTool(senderId: String, name: String) async -> String {
        let payload: JSON = [
            "type": .string("unregister"),
            "name": .string(name),
        ]
        let reply = await send(AgentId("tools"), payload)
        return reply.serialize()
    }

    /// Drop every tool registered with `senderId`.
    public func unregisterToolsBySender(senderId: String) async -> String {
        let payload: JSON = [
            "type": .string("unregister_by_sender"),
            "sender": .string(senderId),
        ]
        let reply = await send(AgentId("tools"), payload)
        return reply.serialize()
    }

    /// Fetch the current tool list in LLM-facing shape.
    public func listToolsForLlm() async -> String {
        let reply = await send(
            AgentId("tools"),
            .object(["type": .string("list_for_llm")]))
        return reply.serialize()
    }

    /// Dispatch a tool by name with JSON-encoded arguments. Returns
    /// the reply from the dispatch target.
    public func dispatchTool(name: String, argumentsJson: String) async -> String {
        let args: JSON
        do {
            args = try JSON.parse(argumentsJson)
        } catch {
            args = .object([:])  // empty args on parse fail
        }
        let payload: JSON = [
            "type": .string("dispatch"),
            "name": .string(name),
            "arguments": args,
        ]
        let reply = await send(AgentId("tools"), payload)
        return reply.serialize()
    }
}

// MARK: - save/load JSON-string shims

extension Kernel {
    /// Snapshot kernel state as a JSON string. Matches Rust UniFFI's
    /// `save() -> String` which returns the same shape.
    public func save() -> String {
        do {
            return try saveJSON()
        } catch {
            return JSON.object([
                "error": .string("save failed: \(error)")
            ]).serialize()
        }
    }

    /// Restore agent tree from JSON. Throws on malformed snapshot
    /// (version, duplicates, dangling parents, no root).
    public func load(json: String) throws {
        try loadJSON(json)
    }

    /// Idempotent shutdown. Future: stop the HTTP listener, release
    /// the workdir lock, fire bundle.onShutdown for every loaded
    /// agent. Today: no-op for in-memory kernels.
    public func shutdown() {
        // The real teardown wires up alongside 8B (HTTP listener
        // close) + 8H (lock release). For now this exists so app
        // call sites compile and any caller that does
        // `defer { kernel.shutdown() }` works.
    }
}

// ── proxy_agent host registration as Kernel methods ───────────────
//
// The Rust UniFFI surface exposed these as instance methods on
// `Kernel`; today the `FantasticProxyAgent` module ships them as
// module-level free functions (`registerHost(_:_:)`). The methods
// below restore the UniFFI shape so callers writing
// `kernel.registerProxyAgent(agentId:host:)` work unchanged.
//
// These methods live in FantasticKernel (not FantasticProxyAgent)
// only so the call-site surface matches Rust UniFFI; the actual
// host registry stays in FantasticProxyAgent.

// Forward-declare the protocol so this file doesn't need to import
// FantasticProxyAgent (which would create a dependency cycle —
// FantasticProxyAgent already imports FantasticKernel). Apps see
// the symbol through `FantasticKernelEmbedded`'s re-exports.
public protocol _ProxyAgentRegistrable: AnyObject, Sendable {
    func handle(payloadJson: String) -> String
    func onBoot()
    func onDelete()
}

extension Kernel {
    /// Install a Swift host for the proxy_agent with `agentId`.
    /// Mirrors UniFFI's `Kernel.registerProxyAgent`. The host must
    /// conform to `ProxyAgentHost` (aliased as `ProxyAgent`).
    ///
    /// Implementation forwards to the module-global host registry
    /// in `FantasticProxyAgent` via the small bridge below.
    public func registerProxyAgent(
        agentId: String,
        host: any _ProxyAgentRegistrable
    ) {
        Kernel._proxyAgentRegisterHook?(AgentId(agentId), host)
    }

    /// Drop the host for the proxy_agent with `agentId`. No-op if
    /// nothing was registered. Mirrors UniFFI's
    /// `Kernel.unregisterProxyAgent`.
    @discardableResult
    public func unregisterProxyAgent(agentId: String) -> Bool {
        return Kernel._proxyAgentUnregisterHook?(AgentId(agentId)) ?? false
    }

    /// Hook the `FantasticProxyAgent` module wires up at import-time
    /// so this extension doesn't need a direct dep on it. Set
    /// exactly once via `installProxyAgentRegistrationHook(_:_:)`.
    nonisolated(unsafe) public static var _proxyAgentRegisterHook:
        ((AgentId, any _ProxyAgentRegistrable) -> Void)?
    nonisolated(unsafe) public static var _proxyAgentUnregisterHook:
        ((AgentId) -> Bool)?
}

/// Called once by `FantasticProxyAgent` to plug the host registry
/// into `Kernel.registerProxyAgent` / `unregisterProxyAgent`.
public func installProxyAgentRegistrationHook(
    register: @escaping (AgentId, any _ProxyAgentRegistrable) -> Void,
    unregister: @escaping (AgentId) -> Bool
) {
    Kernel._proxyAgentRegisterHook = register
    Kernel._proxyAgentUnregisterHook = unregister
}
