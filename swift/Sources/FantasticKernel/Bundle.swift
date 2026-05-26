// Bundle protocol — agent behavior plug-in.
//
// Mirrors Rust's `fantastic_kernel::bundle::Bundle` trait. A bundle
// is a class registered under a `handler_module` string; the kernel
// routes any verb dispatched to an agent whose `handlerModule` matches
// to the bundle's `handle` method.
//
// All methods are async since bundle work can be I/O-bound (file
// reads, HTTP calls, LLM streaming). Defaults are provided for
// everything except `name` and `handle`.

import FantasticJSON
import Foundation

public protocol AgentBundle: Sendable {
    /// Short name for telemetry / reflect (e.g. `"file"`, `"web"`).
    /// Convention: matches the handler_module without the `.tools`
    /// suffix.
    var name: String { get }

    /// Optional README text seeded into `<agent_root>/readme.md` on
    /// agent creation in Disk mode. Bundles that don't ship a readme
    /// return `nil` (the default).
    var readme: String? { get }

    /// Dispatch a verb. Receives the agent the verb is targeting,
    /// the JSON payload, and a kernel handle. Returns the reply
    /// (`nil` for fire-and-forget verbs).
    func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON?

    /// Dispatch a binary-framed verb (raw bytes alongside JSON
    /// header). Default delegates to `handle` with the bytes
    /// base64-encoded into `header["data"]`.
    func handleBinary(
        agentId: AgentId,
        header: JSON,
        blob: Data,
        kernel: Kernel
    ) async throws -> JSON?

    /// Fired during cascade delete BEFORE the agent unregisters.
    /// Use for cleanup (close connections, dispose sessions, etc.).
    func onDelete(agentId: AgentId, kernel: Kernel) async throws

    /// Fired on kernel shutdown for every loaded agent that uses
    /// this bundle. Use for graceful drain of in-flight work.
    func onShutdown(agentId: AgentId, kernel: Kernel) async throws
}

extension AgentBundle {
    public var readme: String? { nil }

    public func handleBinary(
        agentId: AgentId,
        header: JSON,
        blob: Data,
        kernel: Kernel
    ) async throws -> JSON? {
        // Default: base64 the blob into header.data and call handle.
        var payload = header
        payload["data"] = .string(blob.base64EncodedString())
        return try await handle(agentId: agentId, payload: payload, kernel: kernel)
    }

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {}
    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {}
}
