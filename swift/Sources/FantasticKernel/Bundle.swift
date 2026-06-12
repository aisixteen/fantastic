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

    /// Dispatch a binary-framed verb. The binary channel is **symmetric**: a
    /// request carries `(header, blob)` and the reply carries `(JSON?, Data)` —
    /// raw bytes flow in BOTH directions, never base64 (mirrors py/rust's
    /// `read_stream`/`write_stream`). The returned `Data` is the raw reply body
    /// (empty when the verb returns no bytes, e.g. `write_stream`'s status).
    /// Default: base64-decode the request blob into `header["data"]` and route
    /// through `handle` (the legacy text bridge), returning an empty reply body.
    func handleBinary(
        agentId: AgentId,
        header: JSON,
        blob: Data,
        kernel: Kernel
    ) async throws -> (JSON?, Data)

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
    ) async throws -> (JSON?, Data) {
        // Default: base64 the blob into header.data and call handle. No reply body.
        var payload = header
        payload["data"] = .string(blob.base64EncodedString())
        let reply = try await handle(agentId: agentId, payload: payload, kernel: kernel)
        return (reply, Data())
    }

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {}
    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {}
}
