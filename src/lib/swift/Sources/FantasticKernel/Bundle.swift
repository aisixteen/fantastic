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
import OrderedCollections
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
    /// Default: REFUSE — a bundle with no binary edge does not accept binary
    /// frames. There is NO base64 fallback (raw bytes ride the binary carrier,
    /// never base64); a bundle that speaks binary overrides this.
    func handleBinary(
        agentId: AgentId,
        header: JSON,
        blob: Data,
        kernel: Kernel
    ) async throws -> (JSON?, Data)

    /// Refuse an `update_agent` patch this bundle cannot honour. Called by the
    /// substrate BEFORE the record changes; a returned string REFUSES the patch.
    ///
    /// A record carries meta only its bundle understands, so the substrate cannot
    /// judge it — but the bundle can. Default: accept everything. Mirrors py's
    /// duck-typed `validate_meta` / rust's `Bundle::validate_meta`.
    ///
    /// Exists because `update_agent` used to accept anything: an agent set
    /// `ingress_rule="password:kernel-internal-transcripts"`, got `{"updated": true}`
    /// back, and believed a security control was in place that no resolver
    /// implements. Accepting a value you cannot honour AND reporting success is the
    /// worst of both.
    /// `record` is the agent's CURRENT meta (empty at create) — a bundle
    /// validates the state the write WILL produce, not the patch alone
    /// (py-canonical; called at BOTH create and update).
    func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String?

    /// Fired during cascade delete BEFORE the agent unregisters.
    /// Use for cleanup (close connections, dispose sessions, etc.).
    func onDelete(agentId: AgentId, kernel: Kernel) async throws

    /// Fired on kernel shutdown for every loaded agent that uses
    /// this bundle. Use for graceful drain of in-flight work.
    func onShutdown(agentId: AgentId, kernel: Kernel) async throws

    /// The SIGTERM half of `cancel`: the kernel is killing `taskId` (one row of
    /// the task table, owned by this agent) and will cancel its Swift `Task` the
    /// moment this returns. Awaited for the WHOLE subtree while every task is
    /// still alive, so a bundle can release what it owns before anything is
    /// aborted.
    ///
    /// Cancelling a Swift `Task` frees NO real resource — it flips a flag. An OS
    /// subprocess, a PTY, an HTTP stream held FOR THAT TASK must be released
    /// here; this is where the orphaned-subprocess class of bug is fixed. Match
    /// on `taskId` when the resource is task-scoped (an LLM turn): the cancelled
    /// task may be some OTHER dispatch on the same agent. Do NOT tear down
    /// AGENT-scoped resources here (a PTY outlives the `write` that fed it) —
    /// that is what `onDelete` / `onShutdown` are for.
    ///
    /// Optional: the default is a no-op, so pure-async bundles need nothing
    /// (same discovery + optionality as `onDelete`). A hook that throws is
    /// ignored and the kill still happens: cancel is not refusable.
    func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws
}

extension AgentBundle {
    /// Default: accept every patch (most bundles have nothing to check).
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? { nil }

    public var readme: String? { nil }

    public func handleBinary(
        agentId: AgentId,
        header: JSON,
        blob: Data,
        kernel: Kernel
    ) async throws -> (JSON?, Data) {
        // NO base64 fallback (pure streams): a bundle with no binary edge refuses
        // the frame loudly rather than smuggling the blob through the text `handle`.
        let verb = header["type"].asString ?? "(none)"
        return (
            .object([
                "error": .string(
                    "\(name): no binary edge — verb \"\(verb)\" does not accept binary frames")
            ]), Data()
        )
    }

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {}
    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {}
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {}
}
