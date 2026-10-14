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

    /// Optional README text — `reflect readme=true` serves it from here, never
    /// from a copy on disk. Bundles that don't ship a readme return `nil` (the
    /// default).
    var readme: String? { get }

    /// The bundle's shipped `docs/` topic tree, flattened pre-order as
    /// `(key, body)` (`<key>.md`, nested keys in nested dirs) — python's
    /// package `docs/` resource, served by `reflect readme=index`/`=<key>`.
    /// Never copied to disk. Default: none.
    var docs: [(key: String, body: String)] { get }

    /// WHICH VERBS THIS BUNDLE DECLARES — python's module-level `VERBS` table,
    /// read STATICALLY (`Agent.advertises(verb)`): no send, no call row, no task
    /// row, no ledger execution, no watcher echo. `shutdownAgents` picks its
    /// targets from this exactly as python does. Reflect's `verbs` map is
    /// rendered from the SAME per-bundle table, so the two cannot drift.
    ///
    /// Default `[]` — python's "a module with no VERBS table advertises
    /// nothing" (`kernel/_agent.py: advertises`).
    var verbs: [String] { get }

    /// HOW TO CALL this bundle's agents — `{verb: doc}`, python's module-level
    /// `VERBS` docstrings, served by the kernel's `docs` verb (oneliners: each
    /// doc's signature; detailed: the whole doc). Never on a reflect (owner,
    /// 2026-10-05). Default: no docs.
    var verbDocTable: JSON { get }

    /// THIS AGENT'S verb docs, when the bundle serves different verbs per agent
    /// (an MCP server's tools) — python's duck-typed `instance_verbs(agent)`.
    /// `docs of=<agent id>` reads it before `verbDocTable`; nil keeps the
    /// bundle's (owner, 2026-10-08). Default: nil.
    func instanceVerbDocs(_ agent: Agent) -> JSON?

    /// The rest of the DETAILED docs — python's duck-typed module-level `EMITS`,
    /// `CANCEL` and `NOTES`, served by `docs` (tier detailed, no `verb`).
    /// Default: none.
    var emitsDoc: JSON? { get }
    var cancelDoc: JSON? { get }
    var notesDoc: JSON? { get }

    /// Dispatch a verb. Receives the agent the verb is targeting,
    /// the JSON payload, and a kernel handle. Returns the reply
    /// (`nil` for fire-and-forget verbs).
    func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON?

    /// Dispatch a binary-framed verb. The binary channel is **symmetric**: a
    /// request carries `(header, body)` and the reply carries `(JSON?, Blob?)` —
    /// raw bytes flow in BOTH directions, never base64 (mirrors py/rust's
    /// `read_stream`/`write_stream`/`paste_image`, where the bytes are a native
    /// `bytes` value in the dict). `header` is the payload with NOTHING at the
    /// body's path (python holds the bytes there); `body` is the raw request body
    /// AT ITS PATH (`Blob`) — a handler reads ITS OWN key with `bodyAt`, exactly as
    /// python's reads `payload.get("data")` / `payload.get("bytes")`, so a body
    /// placed under another key is not its body. `nil` = the binary channel with
    /// no request body (a raw read: `read_stream`).
    ///
    /// The reply body is a `Blob?` — the chunk AND ITS SLOT in the reply, as
    /// python's reply dict holds a bytes value at a key: a `read_stream` chunk is
    /// `Blob("bytes", …)`, a bridge relaying a peer's reply keeps the slot the
    /// peer's frame named (`["x.y"]` is the reply key `"x.y"`, never `x` → `y`).
    /// It is OPTIONAL and the distinction is load-bearing (C37): `nil` = the verb
    /// returned NO chunk (a `write_stream` status, a refusal); `.some(b)` = it
    /// returned one, `b.bytes` possibly EMPTY (a 0-byte read of an empty file).
    /// Every wire leg picks its response shape from that, exactly as python's
    /// `isinstance(reply["bytes"], bytes)` does — testing `isEmpty` instead
    /// served an empty file as JSON. Rust `Bundle::handle_binary`.
    ///
    /// ANY VERB MAY ANSWER WITH A BODY. Every wire leg (web_ws, web_rest, a
    /// bridge serving an inbound call, the exec relay) dispatches its calls HERE —
    /// a text frame with `body: nil` (`Item.rawReply`) — and answers binary
    /// exactly when the reply carries a body. There is no list of binary verbs
    /// and nothing to declare: a bundle that answers bytes returns them here.
    ///
    /// Default: dispatch the HEADER through the text `handle` (C79) — python
    /// has no separate binary channel at all, so a bundle without a binary edge
    /// answers the header exactly as the text channel would. There is NO base64
    /// fallback: the body is simply not a chunk such a verb ever asked for.
    func handleBinary(
        agentId: AgentId,
        header: JSON,
        body: Blob?,
        kernel: Kernel
    ) async throws -> (JSON?, Blob?)

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

    /// WHAT THIS KIND'S RECORD TAKES, readable before any agent of it exists —
    /// python's duck-typed module-level `RECORD_SCHEMA` (lib request #3,
    /// 2026-09-30). `{fields:[{name, type, required, enum?, default?, hint}]}`
    /// (a key is a `credentials` field, `{store, ref}`), served on the kind's
    /// entry in `reflect bundles=all` (never
    /// `bundles=ids`). The same schema is what `validateMeta` enforces at the
    /// write, so the two cannot drift. Default `nil`: the bundle publishes none.
    var recordSchema: JSON? { get }

    /// DUCK-TYPED ROOT EXTENSION — python `reflect_root_extra(agent, full=…)`.
    ///
    /// The ROOT's bundle (the `kernel_state` loader) contributes root-only
    /// reflect fields the substrate cannot compute without coupling to bundle
    /// logic: `persistence: {provider}`, the instance `guid`, the doc-tree frame
    /// and the `self_extension` shape. Merged by `applyReflectFlags` on the ROOT
    /// ALONE; every other bundle returns the default `nil` and contributes
    /// nothing.
    ///
    /// THROWS like any other reflect step: the doc-tree frame walks the same
    /// `docs/` a bad-UTF-8 file can fail on `detail:"full"` (C60) — python's
    /// exception is not scoped to the `readme` flag alone.
    func reflectRootExtra(agent: Agent, full: Bool, kernel: Kernel) throws(PyError)
        -> OrderedDictionary<String, JSON>?

    /// LAND THE LAST WINDOW while the provider child is still alive. `Kernel
    /// .shutdown` awaits this for every loaded agent BEFORE the teardown walk,
    /// so the loader's message pump can append what the shutdown produced
    /// (python `kernel_state.flush_pending`). Default: nothing to land.
    func flushPending(agentId: AgentId, kernel: Kernel) async throws

    /// Fired during cascade delete BEFORE the agent unregisters.
    /// Use for cleanup (close connections, dispose sessions, etc.).
    func onDelete(agentId: AgentId, kernel: Kernel) async throws

    /// Fired by `Kernel.shutdown`'s depth-first walk for every loaded agent
    /// that uses this bundle — the process is stopping, the record stays.
    /// ONLY this hook runs at shutdown (audit 2026-09-28, c9): a bundle that
    /// frees OS resources at shutdown declares it, usually by calling its own
    /// `onDelete`. Default: nothing.
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
    /// on `taskId` when the resource is task-scoped (an LLM pass): the cancelled
    /// task may be some OTHER dispatch on the same agent. Do NOT tear down
    /// AGENT-scoped resources here (a PTY outlives the `write` that fed it) —
    /// that is what `onDelete` / `onShutdown` are for.
    ///
    /// Optional: the default is a no-op, so pure-async bundles need nothing
    /// (same discovery + optionality as `onDelete`). A hook that throws is
    /// ignored and the kill still happens: cancel is not refusable.
    func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws

    /// Can a FOLDER BUNDLE (`<root>/bundles/<name>/bundle.yaml`, `base: <this
    /// bundle's name>`) extend this bundle? Python: the module exposes
    /// `folder_bundle`. Default: no (owner, 2026-10-08).
    var folderBase: Bool { get }

    /// Build the bundle a folder runs as — `name` its folder (its instances'
    /// handler_module), `folder` the folder's absolute URL, `spec` its parsed
    /// `bundle.yaml` — or the refusal text naming the field.
    func folderBundle(name: String, folder: URL, spec: JSON) -> FolderBuild
}

/// What a base made of a folder: the bundle it runs as, or why not.
public enum FolderBuild: Sendable {
    case built(any AgentBundle)
    case refused(String)
}

extension AgentBundle {
    public var folderBase: Bool { false }

    public func folderBundle(name: String, folder: URL, spec: JSON) -> FolderBuild {
        .refused("\(self.name) is not a base a folder can extend")
    }

    /// Default: accept every patch (most bundles have nothing to check).
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? { nil }

    public var readme: String? { nil }

    public var docs: [(key: String, body: String)] { [] }

    public var verbs: [String] { [] }

    public var verbDocTable: JSON { .object([:]) }
    public func instanceVerbDocs(_ agent: Agent) -> JSON? { nil }
    public var emitsDoc: JSON? { nil }
    public var cancelDoc: JSON? { nil }
    public var notesDoc: JSON? { nil }

    public var recordSchema: JSON? { nil }

    public func handleBinary(
        agentId: AgentId,
        header: JSON,
        body: Blob?,
        kernel: Kernel
    ) async throws -> (JSON?, Blob?) {
        // C79: THE HEADER IS THE PAYLOAD. Python has one dispatch — a REST octet
        // POST of `{type:"say"}` to the cli agent reaches `handle` and answers
        // 204; swift refused it with "no binary edge", which is a channel this
        // substrate invented and python never had. A bundle with no chunk verb
        // simply never looks at the body.
        (try await handle(agentId: agentId, payload: header, kernel: kernel), nil)
    }

    public func reflectRootExtra(agent: Agent, full: Bool, kernel: Kernel) throws(PyError)
        -> OrderedDictionary<String, JSON>?
    { nil }

    public func flushPending(agentId: AgentId, kernel: Kernel) async throws {}

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {}
    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {}
    public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {}
}
