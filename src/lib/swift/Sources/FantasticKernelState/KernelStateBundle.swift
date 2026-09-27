// `kernel_state.tools` — the durable-state ROOT agent (a weak-bound STREAM
// CONSUMER).
//
// Port of python `PB/loader/kernel_state/src/kernel_state/tools.py`, which is
// canonical; rust `crates/bundles/fantastic-core`. `kernel_state` IS the tree
// root (`id="kernel_state"`). It owns the record⇄bytes serialization, the
// tree→path mapping and the auto-persist lifecycle. The auto-flush persists ONLY
// through a stream PROVIDER it DISCOVERS (`Kernel.findStore`: a `file_bridge` in
// the loader's subtree whose root resolves to `.fantastic`). NO PROVIDER ⇒ NO
// AUTO-PERSIST — state lives in RAM until one is wired. There is no direct-write
// fallback.
//
// Contract (duck-typed; any agent answering these is a loader):
//
//   load_tree                 -> {records, version}   read the subtree (direct)
//   persist_record {record}   -> {ok}                 write one agent.yaml
//   forget_record {id}        -> {ok}                 remove one agent's dir
//   persist_blob / load_blob                          the sidecar byte channel
//
// The ROOT loader installs the record `persist_writer` at `boot`
// (`Kernel.bootRootLoader`) and mints the instance guid. It keeps NO message
// tape: the kernel holds no conversation, and its execution rows are flushed by
// the kernel itself into the root's memory store.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// The `kernel_state.tools` bundle.
public final class KernelStateBundle: AgentBundle, @unchecked Sendable {
    /// Per-loader process state — what `boot` installed, so a second `boot` is a
    /// no-op and `onShutdown` can tear it down. Python keeps it on the agent
    /// object (`_fs_flush_loop`).
    private struct LoaderState {
        var installed: RootLoaderState?
        var booted = false
    }

    private let lock = NSLock()
    private var state: [String: LoaderState] = [:]

    public init() {}

    /// The CATALOG name is the entry-point NAME (`kernel_state`), not the
    /// module path — python `_available_bundles` pages `ep.name`.
    public var name: String { "kernel_state" }
    public var verbs: [String] { Array((Self.verbsView.asObject ?? [:]).keys) }

    /// The SHORT orientation readme seeded into `.fantastic/readme.md`. The full
    /// manual is the keyed doc tree — `reflect readme=index` / `=<key>`.
    public var readme: String? { RootReadme.text }

    private func withLock<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }

    // ── verbs ──────────────────────────────────────────────────

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard let agent = kernel.agent(agentId) else {
            return Refusal.unknown(agentId.value, live: kernel.allAgents().map { $0.id.value })
        }
        switch payload["type"].asString ?? "" {
        case "reflect":
            return .object([
                "id": .string(agent.id.value),
                "sentence": .string(
                    "Disk persistence/hydration root — owns .fantastic; "
                        + "answers load_tree / persist_record / forget_record."),
                "root": .string(loaderRoot(agent).path),
                "verbs": Self.verbsView,
            ])
        case "boot":
            return await boot(agent, kernel)
        case "load_tree":
            return .object([
                "records": .array(
                    Persistence.readTree(rootDir: loaderRoot(agent)).map { $0.json }),
                "version": .integer(Int64(KernelState.currentVersion)),
            ])
        case "persist_record":
            return persistRecord(agent, payload, kernel)
        case "forget_record":
            return forgetRecord(agent, payload, kernel)
        case "persist_blob":
            return await kernel.persistBlob(payload)
        case "load_blob":
            return await kernel.loadBlob(payload)
        case "shutdown":
            return nil
        default:
            // CODED. A reason-less error on the ROOT — the most-addressed agent
            // in the kernel — is decorated by `applyReflectFlags` into something
            // that reads like a healthy reflect (python 2026-09-02).
            let t = payload["type"]
            return Refusal.make(
                Refusal.unhandled, "kernel_state: unknown type \(pyRepr(t))",
                extra: [("verb", t)])
        }
    }

    /// IDEMPOTENT, as python's `_boot` is: a second boot installs nothing.
    private func boot(_ agent: Agent, _ kernel: Kernel) async -> JSON? {
        let first: Bool = withLock {
            var e = state[agent.id.value] ?? LoaderState()
            if e.booted { return false }
            e.booted = true
            state[agent.id.value] = e
            return true
        }
        guard first else { return nil }
        // A SESSION loader (a `root` meta pointing at a sub-namespace) seeds its
        // namespace anchor so `read_tree` has a root to walk from.
        let rootDir = loaderRoot(agent)
        if rootDir.path != agent.rootPath.path,
            !FileManager.default.fileExists(
                atPath: rootDir.appendingPathComponent(RECORD_FILE).path)
        {
            Self.writeRecord(at: rootDir, agent.record().ordered)
        }
        let installed = await kernel.bootRootLoader(agent)
        withLock {
            var e = state[agent.id.value] ?? LoaderState()
            e.installed = installed
            state[agent.id.value] = e
        }
        return nil
    }

    /// args: record:dict (req). Merge-write one agent.yaml. Location is the live
    /// agent's own dir if present, else computed under this loader's root from
    /// the record's parent chain.
    private func persistRecord(_ agent: Agent, _ payload: JSON, _ kernel: Kernel) -> JSON {
        guard let record = payload["record"].asObject, pyTruthy(record["id"] ?? .null),
            let id = record["id"]?.asString
        else {
            return Refusal.missing("persist_record", "record (dict with id)")
        }
        // DEFENCE IN DEPTH. `create_agent` guards the id at the mint, but this
        // verb takes one straight from the PAYLOAD — and a foreign record
        // arrives here from a bridge/session loader, a lower-trust caller. An id
        // becomes a directory, so an unvalidated one writes wherever it points.
        if let bad = Refusal.guardAgentId("persist_record", id) { return bad }
        let parentId = record["parent_id"]?.asString
        if let denied = guardRecordOwner("persist_record", id, parentId, agent, kernel) {
            return denied
        }
        if let pid = parentId, let bad = Refusal.guardAgentId("persist_record", pid) {
            return bad
        }
        let dir = kernel.agent(AgentId(id))?.rootPath ?? resolvePath(agent, kernel, id, parentId)
        Self.writeRecord(at: dir, record)
        return .object(["ok": .bool(true)])
    }

    /// args: id:str (req), parent_id:str (opt). Remove the agent's dir. This
    /// verb ends in a recursive delete: measured 2026-08-28, an agent with NO
    /// explicit grant sent `id="../../important_project_dir"`.
    private func forgetRecord(_ agent: Agent, _ payload: JSON, _ kernel: Kernel) -> JSON {
        guard pyTruthy(payload["id"]), let target = payload["id"].asString else {
            return Refusal.missing("forget_record", "id")
        }
        if let bad = Refusal.guardAgentId("forget_record", target) { return bad }
        let parentId = payload["parent_id"].asString
        if let denied = guardRecordOwner("forget_record", target, parentId, agent, kernel) {
            return denied
        }
        if let pid = parentId, let bad = Refusal.guardAgentId("forget_record", pid) {
            return bad
        }
        let dir =
            kernel.agent(AgentId(target))?.rootPath
            ?? resolvePath(agent, kernel, target, parentId)
        try? FileManager.default.removeItem(at: dir)
        return .object(["ok": .bool(true)])
    }

    // ── the root reflect frame ─────────────────────────────────

    /// DUCK-TYPED root-reflect contribution — python `reflect_root_extra`.
    ///
    /// Surfaces WHICH file_bridge the loader auto-persists THROUGH (or `null`
    /// when none is wired: state stays in RAM), the kernel's stable instance
    /// `guid`, and — on `detail:"full"` only — the doc-tree FRAME (`readmes` +
    /// `docs`) plus the long `self_extension` prose. The DISTILLED frame carries
    /// the SHORT `self_extension` instead: the shape of a durable program cannot
    /// be left to a readme, because a model that lacks it writes something that
    /// cannot work — and the weak models are exactly the ones that do not follow
    /// the pointer to go read it.
    public func reflectRootExtra(agent: Agent, full: Bool, kernel: Kernel) throws(PyError)
        -> OrderedDictionary<String, JSON>?
    {
        var out: OrderedDictionary<String, JSON> = [
            "persistence": .object([
                "provider": kernel.findStore().map { JSON.string($0.value) } ?? .null
            ]),
            "guid": agent.metaValue(forKey: "guid") ?? .null,
        ]
        if !full {
            out["self_extension"] = .string(Self.selfExtensionShort)
            return out
        }
        out["readmes"] = try kernel.readmeIndex(agent)
        out["docs"] = .string(Self.docsLine)
        out["self_extension"] = .string(Self.selfExtensionFull)
        return out
    }

    // ── lifecycle ──────────────────────────────────────────────

    /// LAND THE LAST RECORDS while the provider child is still alive. The kernel
    /// awaits this BEFORE the teardown walk (python `flush_pending`).
    ///
    /// THE RECORD FLUSH LIVES HERE, not in `onShutdown`. The record loop
    /// debounces 0.15 s; a `create_agent` + `update_agent` followed straight away
    /// by `shutdown()` used to reach its final flush mid-teardown-walk, with the
    /// store already gone, and the update never landed.
    public func flushPending(agentId: AgentId, kernel: Kernel) async throws {
        await kernel.flushRecords()
    }

    /// Unsubscribe + stop the record loop. `flushPending` normally ran already
    /// (the kernel calls it before the walk, while the provider is alive) and is
    /// what WRITES.
    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
        let installed: RootLoaderState? = withLock {
            let e = state.removeValue(forKey: agentId.value)
            return e?.installed
        }
        if let installed { await kernel.shutdownRootLoader(installed) }
    }

    // ── helpers ────────────────────────────────────────────────

    /// The loader's own disk dir: a `root` meta overrides the agent's address, so
    /// a SESSION loader can serve a sub-namespace. Python `_root`.
    private func loaderRoot(_ agent: Agent) -> URL {
        if let r = agent.metaValue(forKey: "root")?.asString, !r.isEmpty {
            return URL(fileURLWithPath: r, isDirectory: true)
        }
        return agent.rootPath
    }

    /// The children-container dir name for this loader's namespace (declared
    /// config; default `agents`). Python `_children_name`.
    private func childrenName(_ agent: Agent) -> String {
        if let cd = agent.metaValue(forKey: "children_dir")?.asString, !cd.isEmpty { return cd }
        return "agents"
    }

    /// This loader's PERSISTED records → `[id: parent id]`, the parent taken from
    /// DISK POSITION under its root (authoritative, as `readTree` does): a record
    /// directly under the root's children dir is parented to the loader itself. A
    /// record the host never runs — a federated kernel's, sent through a bridge —
    /// exists only here, so a walk that stops at the live tree loses every record
    /// nested under it. Python `_disk_parents`.
    private func diskParents(_ agent: Agent) -> [String: String?] {
        let cd = childrenName(agent)
        var out: [String: String?] = [:]
        for rec in Persistence.walkRecords(
            loaderRoot(agent).appendingPathComponent(cd), childrenDir: cd,
            parentId: agent.id.value)
        {
            out[rec.id] = .some(rec.parentId)
        }
        return out
    }

    /// (known, parent) for one id: the LIVE tree first, then this loader's
    /// persisted records (read once per walk, cached in `disk`). Unknown in both
    /// ⇒ (false, nil) — the caller decides; nothing is guessed. Python `_where`.
    private func whereIs(
        _ agent: Agent, _ kernel: Kernel, _ node: String, _ disk: inout [String: String?]?
    ) -> (known: Bool, parent: String?) {
        if let live = kernel.agent(AgentId(node)) { return (true, live.parentId?.value) }
        if disk == nil { disk = diskParents(agent) }
        if let entry = disk?[node] { return (true, entry) }
        return (false, nil)
    }

    /// Compute a record's on-disk dir under this loader's root by walking its
    /// parent chain: the live tree, then this loader's persisted records (so a
    /// record nested 2+ levels under a federated record lands where it belongs).
    /// A record that already exists keeps ITS position; the payload's `parent_id`
    /// places only a NEW one. Used when the agent isn't (or is no longer) live.
    /// Python `_resolve_path`.
    private func resolvePath(_ agent: Agent, _ kernel: Kernel, _ id: String, _ parentId: String?)
        -> URL
    {
        var parts = [id]
        var disk: [String: String?]? = nil
        var (known, pid) = whereIs(agent, kernel, id, &disk)
        if !known { pid = parentId }
        var seen = Set<String>()
        while let p = pid, !p.isEmpty, p != agent.id.value, !seen.contains(p) {
            seen.insert(p)
            parts.append(p)
            pid = whereIs(agent, kernel, p, &disk).parent
        }
        var path = loaderRoot(agent)
        let children = childrenName(agent)
        for part in parts.reversed() {
            path = path.appendingPathComponent(children).appendingPathComponent(part)
        }
        return path
    }

    /// Merge-write one record's `agent.yaml` at `dir` (the agent's own dir).
    /// Reads any existing file and overlays only the kernel-managed keys,
    /// preserving unknown fields + sidecars. Python `write_record`.
    static func writeRecord(at dir: URL, _ record: OrderedDictionary<String, JSON>) {
        let file = dir.appendingPathComponent(RECORD_FILE)
        var onDisk = Persistence.readRecord(file) ?? [:]
        for (k, v) in record { onDisk[k] = v }
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try? Persistence.recordBytes(onDisk).write(to: file, options: .atomic)
    }

    /// Refuse a RECORD verb that names an agent outside the caller's own subtree.
    ///
    /// Same hole the blob owner guard closed for sidecars, one field over: the
    /// record id arrives in the PAYLOAD, so any agent granted `persist_record` on
    /// the loader could rewrite — or `forget_record` could DELETE — a sibling's
    /// record. An OPERATOR (no origin: the loader's own flush, the CLI, a bridge
    /// hydrating at boot) is the bookkeeping these verbs exist for and stays
    /// ungated. An AGENT may name ITSELF or something it owns.
    ///
    /// THE CHAIN IS WALKED WHERE IT REALLY IS: the live tree, then this loader's
    /// own persisted records (a federated kernel's records are not live here — a
    /// canvas's nested agents used to stop the walk at the canvas and be
    /// refused). A record that already exists is placed by the live tree or its
    /// disk position, never by the payload — else naming a sibling's id with
    /// `parent_id` = yourself passed. The payload's `parent_id` places only a
    /// record that exists in neither. Python `_guard_record_owner`.
    private func guardRecordOwner(
        _ verb: String, _ recordId: String, _ parentId: String?, _ agent: Agent,
        _ kernel: Kernel
    ) -> JSON? {
        guard let caller = KernelTaskLocals.currentOrigin else { return nil }
        if caller.value == recordId { return nil }
        var disk: [String: String?]? = nil
        var (known, node) = whereIs(agent, kernel, recordId, &disk)
        if !known { node = (parentId?.isEmpty == false) ? parentId : nil }
        var seen = Set<String>()
        while let n = node, !n.isEmpty, !seen.contains(n) {
            if n == caller.value { return nil }
            seen.insert(n)
            node = whereIs(agent, kernel, n, &disk).parent
        }
        return Refusal.make(
            Refusal.unauthorized,
            "\(pyRepr(caller.value)) may not \(verb) \(pyRepr(recordId)) — a record verb is "
                + "scoped to the caller's own subtree; only an operator may name one outside it")
    }

    /// `verbs` as the loader's own reflect renders it — one line per verb,
    /// python's first-docstring-line rule.
    static let verbsView: JSON = .object([
        "reflect": .string("Identity + the disk root it owns. No args."),
        "boot": .string(
            "Start the auto-flush loop unless `watch=false` (a loader serving a"),
        "load_tree": .string(
            "args: none. Read this loader's subtree from disk → {records, version}."),
        "persist_record": .string(
            "args: record:dict (req). Merge-write one agent.yaml. Location is the"),
        "forget_record": .string(
            "args: id:str (req), parent_id:str (opt). rmtree the agent's dir. For a"),
        "persist_blob": .string(
            "args: agent_id:str (req), name:str (req), content:str (req). Persist an"),
        "load_blob": .string(
            "args: agent_id:str (req), name:str (req). Read an agent's sidecar THROUGH the"),
    ])
}

extension KernelStateBundle {
    /// The SHORT `self_extension` a DISTILLED root reflect carries — python
    /// `reflect_root_extra`'s non-full branch, verbatim.
    static let selfExtensionShort =
        "A DURABLE PROGRAM is a FILE plus a command — no `code` field. (1) WRITE it to a file: <type>write_stream</type> <path>…</path> <truncate>true</truncate> <bytes>…</bytes> — `bytes` is VERBATIM (the program between `<bytes>` and `</bytes>` exactly as written, real newlines, nothing escaped). (2) RUN it on the COMMAND runner: <type>start</type> <command>…</command> <args><item>…</item></args>, gated by its `allowed` regexes (reflect it). (3) Re-run the same command to keep it current. A COMPUTE worker is an `exec_bridge.tools` agent + a program file — NOT a new handler_module. Create it as YOUR child; to wire it to a SIBLING (`totals`) grant `can_call:{totals}` AND `update_agent id='totals'` to add it to `totals.callable_by`. Read <type>reflect</type> <readme>self-extension</readme> — send it to 'kernel' — for the fd wire."

    /// The one-liner teaching keyed doc fetch — python's `docs`.
    static let docsLine =
        "This kernel's manual is a keyed doc tree. To read it, send 'kernel' a reflect with a string `readme` field: <type>reflect</type> <readme>index</readme> lists every topic with its ~token cost; <readme>key</readme> opens one (e.g. self-extension, streams); <readme>true</readme> is the short orientation. The field is named exactly `readme` (a string) - not 'readme_key'. Read only what a task needs."

    /// The LONG `self_extension` a `detail:"full"` root reflect carries — python's
    /// full branch, verbatim (it has no fd-wire tail sentence).
    static let selfExtensionFull =
        "A DURABLE PROGRAM is a FILE you write plus a command that runs it — not a `code`-carrying agent. There is no `code` field: a program used to be embedded as a JSON string, which is exactly how a call gets lost to escaping. Assemble these THREE pieces and you have built something that persists and re-runs, with no `code` field at all:\n1. WRITE THE PROGRAM TO A FILE: never carry file contents in a JSON string. Use the file leg's STREAM verbs — <type>write_stream</type> <path>…</path> <truncate>true</truncate> <bytes>…</bytes> to write, <type>read_stream</type> <path>…</path> <offset>0</offset> to read, one raw chunk at a time. `bytes` is VERBATIM: the program between `<bytes>` and `</bytes>` exactly as written, real newlines, nothing escaped. That is the only byte path; the string read/write verbs are gone.\n2. RUN IT: the executor is a general-purpose COMMAND runner. Reflect it, read its `allowed` regexes, then send <type>start</type> <command>path</command> <args><item>…</item></args>; poll <type>status</type>. It runs commands, not inline programs.\n3. RE-RUN: the file is durable on disk, so the SAME `start` command run again later re-executes the SAME program against whatever the inputs hold THEN — that is how a thing 'keeps itself up to date'.\nA WORKER THAT COMPUTES is an `exec_bridge.tools` agent (the EXISTING command runner) that owns a program file — you never author a new handler_module (you cannot add modules to the catalog and do not need to). 'A specialist with child workers' = a parent agent whose children are exec_bridge runners, each with its own program file.\nWIRING TO A SIBLING: you cannot raise your OWN authority, but if you hold `update_agent` (e.g. on the root) you may open ANY agent's ingress — including a sibling like `totals`/`readings` that is not your child. To let your worker call `totals`: grant the worker `can_call:{totals}` AND `update_agent id='totals'` to add the worker to `totals.callable_by` (send the WHOLE map — update replaces, not merges).\nA SPAWNED PROGRAM reaches agents over its control fd — a TWO-WAY socket (FANTASTIC_CTRL_FD): write {op:'send', rid, target, payload}; the reply comes back on the SAME fd as {op:'reply', rid, data}. It is not inline — read frames until one carries your `rid`, skipping event/inbox frames.\nCOMPOSE the agents you were given for everything else (`list_agents`, reflect each, send). CREATE/DELETE plain agents (create_agent with a handler_module and grants, delete_agent) work as before."
}
