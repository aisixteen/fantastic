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

import FantasticFile
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
    public var verbDocTable: JSON { Self.verbsView }

    /// The SHORT orientation readme — `reflect readme=true` serves it from here.
    /// The full manual is the keyed doc tree — `reflect readme=index` / `=<key>`.
    public var readme: String? { RootReadme.text }

    /// The keyed manual — python's `kernel_state/docs/` package tree — served
    /// from the bundle, never copied to disk.
    public var docs: [(key: String, body: String)] { RootDocs.all }

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
                "root": .string(rootDisplay(agent, kernel)),
            ])
        case "boot":
            return try await boot(agent, kernel)
        case "load_tree":
            let rootDir = try clampedRoot(agent, kernel)
            // A session loader's namespace keeps no copy of its record: the
            // walk is rooted at the LIVE one (owner 2026-10-14).
            let live =
                Kernel.canonPath(rootDir.path) == Kernel.canonPath(agent.rootPath.path)
                ? nil : try? AgentRecord(json: .object(agent.record().ordered))
            return .object([
                "records": .array(
                    try Persistence.readTree(rootDir: rootDir, live: live).map { $0.json }
                ),
                "version": .integer(Int64(KernelState.currentVersion)),
            ])
        case "persist_record":
            return try persistRecord(agent, payload, kernel)
        case "forget_record":
            return try forgetRecord(agent, payload, kernel)
        case "persist_blob":
            return await kernel.persistBlob(payload)
        case "load_blob":
            return await kernel.loadBlob(payload)
        case "reload_bundle":
            return try await reloadBundle(agent, payload, kernel)
        case "read_bundles":
            return try readBundles(agent, payload, kernel)
        default:
            // CODED. A reason-less error on the ROOT — the most-addressed agent
            // in the kernel — is decorated by `applyReflectFlags` into something
            // that reads like a healthy reflect (python 2026-09-02). `shutdown`
            // lands here too: python's VERBS has none.
            let t = payload["type"]
            return Refusal.make(
                Refusal.unhandled, "kernel_state: unknown type \(pyRepr(t))",
                extra: [("verb", t)])
        }
    }

    /// `reload_bundle {name}` — python `kernel_state._reload_bundle`: re-read
    /// `bundles/<name>/bundle.yaml` (a new folder loads) and restart every live
    /// instance, each AS ITSELF (kernel bookkeeping): `shutdown` with the reason,
    /// then `boot` under the fresh load. A folder that no longer loads is refused
    /// naming the folder and field; its instances keep the previous load.
    private func reloadBundle(
        _ agent: Agent, _ payload: JSON, _ kernel: Kernel
    ) async throws(PyError) -> JSON {
        if agent.parentId != nil {
            return Refusal.make(
                Refusal.notHere, "reload_bundle: the kernel's bundles are the ROOT loader's",
                hint: "send it to kernel")
        }
        guard let name = payload["name"].asString, !name.isEmpty else {
            return Refusal.missing(
                "reload_bundle", "name", hint: "a folder under .fantastic/bundles/")
        }
        let root = try clampedRoot(agent, kernel)
        let rel = "\(Folders.bundlesDir)/\(name)"
        guard Folders.isFolderName(name),
            FileManager.default.fileExists(atPath: root.appendingPathComponent(rel).path)
        else {
            return Refusal.make(
                Refusal.notFound, "reload_bundle: no folder \(root.lastPathComponent)/\(rel)",
                hint: "a bundle is a folder under .fantastic/bundles/ with a bundle.yaml")
        }
        if let err = Folders.loadBundle(kernel: kernel, root: root, name: name) {
            return Refusal.make(Refusal.invalidMeta, err)
        }
        let live = kernel.allAgents()
            .filter { $0.handlerModule == name }
            .sorted { $0.id.value < $1.id.value }
        for a in live {
            _ = await kernel.bookkeeping(
                a, .object(["type": .string("shutdown"), "reason": .string("reloaded")]))
            _ = await kernel.bookkeeping(a, .object(["type": .string("boot")]))
        }
        return .object([
            "reloaded": .string(name),
            "instances": .array(live.map { .string($0.id.value) }),
        ])
    }

    /// `read_bundles {name?}` — python `kernel_state._read_bundles`: the folder
    /// bundles under THIS loader's root as TEXT, each `bundles/<name>/bundle.yaml`
    /// raw (`error` when it has none), for a kernel that loads its own folders
    /// through this loader (a page kernel's proxy_loader). Parses nothing.
    private func readBundles(
        _ agent: Agent, _ payload: JSON, _ kernel: Kernel
    ) throws(PyError) -> JSON {
        let root = try clampedRoot(agent, kernel)
        let r = root.path
        let shown = root.lastPathComponent
        let dir = Folders.bundlesDir
        var names: [String] = []
        do {
            switch payload["name"] {
            case .null:
                if try fsExists(r, dir, base: r), try fsIsDir(r, dir, base: r) {
                    for p in try fsListDir(r, dir, base: r) {
                        let n = URL(fileURLWithPath: p).lastPathComponent
                        if try fsIsDir(r, "\(dir)/\(n)", base: r) { names.append(n) }
                    }
                }
            case .string(let n) where !n.isEmpty:
                let rel = "\(dir)/\(n)"
                // a name the folder rule refuses is never joined into a path
                guard Folders.isFolderName(n), try fsExists(r, rel, base: r),
                    try fsIsDir(r, rel, base: r)
                else {
                    return Refusal.make(
                        Refusal.notFound, "read_bundles: no folder \(shown)/\(rel)",
                        hint: "a bundle is a folder under \(shown)/\(dir)/ with a bundle.yaml")
                }
                names = [n]
            default:
                return Refusal.bad("read_bundles", "name", "a folder name")
            }
            var out: [JSON] = []
            for n in names {
                let rel = "\(dir)/\(n)"
                let file = "\(rel)/\(Folders.bundleFile)"
                var row: OrderedDictionary<String, JSON> = [
                    "name": .string(n), "folder": .string("\(shown)/\(rel)"),
                ]
                if try fsExists(r, file, base: r) {
                    row["text"] = .string(try fsReadText(r, file, base: r))
                } else {
                    row["error"] = .string("bundles/\(n): no bundle.yaml")
                }
                out.append(.object(row))
            }
            return .object(["bundles": .array(out)])
        } catch {
            throw Self.raised(error)
        }
    }

    /// IDEMPOTENT, as python's `_boot` is: a second boot installs nothing.
    private func boot(_ agent: Agent, _ kernel: Kernel) async throws(PyError) -> JSON? {
        let first: Bool = withLock {
            var e = state[agent.id.value] ?? LoaderState()
            if e.booted { return false }
            e.booted = true
            state[agent.id.value] = e
            return true
        }
        guard first else { return nil }
        // A SESSION loader (a `root` meta pointing at a sub-namespace) writes no
        // record of its own there: its one record lives in its parent's tree and
        // `load_tree` walks the namespace from it (owner 2026-10-14 — a copy at
        // `<root>/agent.yaml` froze and diverged).
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
    ///
    /// The ids are TYPED as python's `guard_agent_id` types them: a truthy id
    /// that is not a string (`5`) is `id must be a non-empty string`, never
    /// `record required`; a `parent_id` that is present and not a string is the
    /// same refusal, never silently dropped.
    private func persistRecord(_ agent: Agent, _ payload: JSON, _ kernel: Kernel) throws(PyError)
        -> JSON
    {
        if let denied = guardOwnTree("persist_record", agent) { return denied }
        guard let record = payload["record"].asObject, pyTruthy(record["id"] ?? .null) else {
            return Refusal.missing("persist_record", "record (dict with id)")
        }
        // DEFENCE IN DEPTH. `create_agent` guards the id at the mint, but this
        // verb takes one straight from the PAYLOAD — and a foreign record
        // arrives here from a bridge/session loader, a lower-trust caller. An id
        // becomes a directory, so an unvalidated one writes wherever it points.
        guard let id = record["id"]?.asString else {
            return Refusal.bad("persist_record", "id", "a non-empty string")
        }
        if let bad = Refusal.guardAgentId("persist_record", id) { return bad }
        let rawParent = record["parent_id"] ?? .null
        if let bad = Self.guardParentChain("persist_record", rawParent) { return bad }
        let dir: URL
        if let live = kernel.agent(AgentId(id)) {
            dir = live.rootPath
        } else {
            dir = try resolvePath(agent, kernel, id, rawParent.asString)
        }
        try Self.writeRecord(at: try clamped(dir.path, kernel), record)
        return .object(["ok": .bool(true)])
    }

    /// args: id:str (req), parent_id:str (opt). Remove the agent's dir. This
    /// verb ends in a recursive delete: measured 2026-08-28, an agent with NO
    /// explicit grant sent `id="../../important_project_dir"`.
    private func forgetRecord(_ agent: Agent, _ payload: JSON, _ kernel: Kernel) throws(PyError)
        -> JSON
    {
        if let denied = guardOwnTree("forget_record", agent) { return denied }
        guard pyTruthy(payload["id"]) else {
            return Refusal.missing("forget_record", "id")
        }
        guard let target = payload["id"].asString else {
            return Refusal.bad("forget_record", "id", "a non-empty string")
        }
        if let bad = Refusal.guardAgentId("forget_record", target) { return bad }
        let rawParent = payload["parent_id"]
        if let bad = Self.guardParentChain("forget_record", rawParent) { return bad }
        let dir: URL
        if let live = kernel.agent(AgentId(target)) {
            dir = live.rootPath
        } else {
            dir = try resolvePath(agent, kernel, target, rawParent.asString)
        }
        // python `rmtree(fs.Root(workdir, path))`: clamped, absent is a no-op,
        // and a removal the OS refuses is swallowed there too.
        let rp = try clamped(dir.path, kernel)
        if FileManager.default.fileExists(atPath: rp.path) {
            try? fsRemove(rp.path, "", recursive: true, base: try runningDir(kernel))
        }
        return .object(["ok": .bool(true)])
    }

    /// python `_guard_parent_chain`: a `parent_id` that is present must be a
    /// valid agent id — the path walk joins every link of the chain.
    static func guardParentChain(_ verb: String, _ parentId: JSON) -> JSON? {
        if parentId.isNull { return nil }
        guard let pid = parentId.asString else {
            return Refusal.bad(verb, "id", "a non-empty string")
        }
        return Refusal.guardAgentId(verb, pid)
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
        // `provider` only while writes land; a refused/failed write reports
        // `error` and no provider until one lands again (d2).
        var persistence: OrderedDictionary<String, JSON> = [:]
        if let err = kernel.persistError {
            persistence["provider"] = .null
            persistence["error"] = .object(err)
        } else {
            persistence["provider"] = kernel.findStore().map { JSON.string($0.value) } ?? .null
        }
        var out: OrderedDictionary<String, JSON> = [
            "persistence": .object(persistence),
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
        // DEBUG FILE FLUSH — a tree with no record store lives in RAM; with
        // `FANTASTIC_DEBUG_FLUSH=1` (set by the e2e harness, never by default)
        // every non-ephemeral record lands in its `agent.yaml` at stop, so a test
        // restart boots the tree it stopped. The wire twin of `_testkit.persist`.
        if ProcessInfo.processInfo.environment["FANTASTIC_DEBUG_FLUSH"] == "1" {
            for a in kernel.allAgents() where !a.ephemeral {
                try Self.writeRecord(at: try clamped(a.rootPath.path, kernel), a.record().ordered)
            }
        }
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

    /// The loader's own disk dir AS CONFIGURED: a `root` meta overrides the
    /// agent's address, so a SESSION loader can serve a sub-namespace. Python
    /// `_root` — not yet resolved; every use goes through `clamped`.
    private func loaderRoot(_ agent: Agent) -> String {
        if let r = agent.metaValue(forKey: "root")?.asString, !r.isEmpty { return r }
        return agent.rootPath.path
    }

    /// The running dir every loader path is clamped to (python `Kernel.workdir`).
    private func runningDir(_ kernel: Kernel) throws(PyError) -> String {
        guard let w = kernel.workdir?.path else {
            throw PyError("ValueError", "kernel has no root — no running dir to clamp disk to")
        }
        return w
    }

    /// python `fs.resolve_root(fs.Root(workdir, path))`: a relative path
    /// resolves under the RUNNING DIR (never the process cwd), and one that
    /// leaves it RAISES — `ValueError: fs: root '…' escapes the running dir`.
    private func clamped(_ path: String, _ kernel: Kernel) throws(PyError) -> URL {
        let base = try runningDir(kernel)
        do {
            return URL(fileURLWithPath: try fsResolveRoot(path, base: base), isDirectory: true)
        } catch {
            throw Self.raised(error)
        }
    }

    /// The loader's own root, clamped.
    private func clampedRoot(_ agent: Agent, _ kernel: Kernel) throws(PyError) -> URL {
        try clamped(loaderRoot(agent), kernel)
    }

    /// python `str(_root(agent))` — the configured root as pathlib prints it,
    /// or the loader's own address relative to the running dir (python's
    /// addresses are relative: `main.py` roots the tree at `.fantastic`).
    private func rootDisplay(_ agent: Agent, _ kernel: Kernel) -> String {
        if let r = agent.metaValue(forKey: "root")?.asString, !r.isEmpty {
            return Self.pyPurePath(r)
        }
        let p = agent.rootPath.path
        if let w = kernel.workdir?.path, p.hasPrefix(w + "/") {
            return String(p.dropFirst(w.count + 1))
        }
        return p
    }

    /// `str(PurePosixPath(s))`: empty and `.` segments dropped, `..` kept.
    static func pyPurePath(_ s: String) -> String {
        let parts = s.split(separator: "/", omittingEmptySubsequences: true).filter { $0 != "." }
        let joined = parts.joined(separator: "/")
        if s.hasPrefix("/") { return "/" + joined }
        return joined.isEmpty ? "." : joined
    }

    /// python's raise for an `fs` failure, as the kernel settles it: the clamp's
    /// refusals are `ValueError`s, an OS failure the matching `OSError`
    /// subclass (`[Errno N] …`).
    static func raised(_ e: FsError) -> PyError {
        let m = e.message
        if m.hasPrefix("[Errno "), let end = m.firstIndex(of: "]"),
            let code = Int32(m[m.index(m.startIndex, offsetBy: 7)..<end])
        {
            return PyError(pyOSErrorName(code), m)
        }
        return PyError("ValueError", m)
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
    private func diskParents(_ agent: Agent, _ kernel: Kernel) throws(PyError)
        -> [String: String?]
    {
        let cd = childrenName(agent)
        var out: [String: String?] = [:]
        for rec in try Persistence.walkRecords(
            clampedRoot(agent, kernel), childrenDir: cd, parentId: agent.id.value)
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
    ) throws(PyError) -> (known: Bool, parent: String?) {
        if let live = kernel.agent(AgentId(node)) { return (true, live.parentId?.value) }
        if disk == nil { disk = try diskParents(agent, kernel) }
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
        throws(PyError) -> URL
    {
        var parts = [id]
        var disk: [String: String?]? = nil
        var (known, pid) = try whereIs(agent, kernel, id, &disk)
        if !known { pid = parentId }
        var seen = Set<String>()
        while let p = pid, !p.isEmpty, p != agent.id.value, !seen.contains(p) {
            seen.insert(p)
            parts.append(p)
            pid = try whereIs(agent, kernel, p, &disk).parent
        }
        var path = try clampedRoot(agent, kernel)
        let children = childrenName(agent)
        for part in parts.reversed() {
            path = path.appendingPathComponent(children).appendingPathComponent(part)
        }
        return path
    }

    /// Merge-write one record's `agent.yaml` at `dir` (the agent's own dir,
    /// already clamped). Reads any existing file and overlays only the
    /// kernel-managed keys, preserving unknown fields + sidecars. A write the OS
    /// refuses RAISES, as `fs.write_text` does. A bundle's readme and `docs/`
    /// are never copied — reflect serves them from the bundle. Python
    /// `write_record`.
    static func writeRecord(
        at dir: URL, _ record: OrderedDictionary<String, JSON>
    ) throws(PyError) {
        let file = dir.appendingPathComponent(RECORD_FILE)
        var onDisk = Persistence.readRecord(file) ?? [:]
        for (k, v) in record { onDisk[k] = v }
        do {
            try fsWriteBytes(dir.path, RECORD_FILE, Persistence.recordBytes(onDisk), base: dir.path)
        } catch {
            throw raised(error)
        }
    }

    /// The ROOT loader's tree is the one this kernel HYDRATES at boot, so only
    /// the operator (its own flush, the CLI, boot) writes it. An agent's record
    /// changes through `update_agent`, gated like any verb — a record
    /// written here by an agent would be hydrated with whatever grants it named
    /// (audit 2026-09-28, a15). A session loader (a remote's sub-namespace,
    /// never hydrated here) keeps serving its remote. Python `_guard_own_tree`.
    private func guardOwnTree(_ verb: String, _ agent: Agent) -> JSON? {
        guard agent.parentId == nil, KernelTaskLocals.currentOrigin != nil else { return nil }
        return Refusal.make(
            Refusal.unauthorized,
            "\(verb): the kernel's own tree is written only by the operator — "
                + "an agent's record changes through update_agent",
            hint: "update_agent id=<id> …  (needs update_agent granted on the root)",
            extra: [("side", .string("store"))])
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
        "reload_bundle": .string(
            "args: name:str (req). Re-read `bundles/<name>/bundle.yaml` (a new folder loads) and restart every live instance of it, abruptly — an in-flight call is answered with an error naming the reload. Returns {reloaded, instances}. The root answers it."
        ),
        "read_bundles": .string(
            "args: name:str?. The folder bundles under this loader's root AS TEXT — {bundles:[{name, folder, text?, error?}]}, each `bundles/<name>/bundle.yaml` raw (`error` when it has none); `name` reads one. For a kernel that loads its own folders through this loader (a page kernel's proxy_loader); this loader parses nothing."
        ),
    ])
}

extension KernelStateBundle {
    /// The SHORT `self_extension` a DISTILLED root reflect carries — python
    /// `reflect_root_extra`'s non-full branch, verbatim.
    static let selfExtensionShort =
        "A DURABLE PROGRAM is a FILE plus a command (no `code` field): write it with a file agent's <type>write_stream</type> (`bytes` verbatim), run it on an `exec_bridge.tools` agent — <type>start</type> <command>python3</command> <args><item>prog.py</item></args>, matched against its `allowed` — and re-run to update. Wiring an agent to another takes BOTH halves: its `can_call` and the other's `callable_by`. More: reflect kernel with <readme>self-extension</readme>."

    /// The one-liner teaching keyed doc fetch — python's `docs`.
    static let docsLine =
        "How to CALL an agent: send 'kernel' <type>docs</type> <of>agent id, bundle or kernel</of> — one signature line per verb; add <verb>name</verb> <tier>detailed</tier> for one verb's full text. The manual is a keyed doc tree: send 'kernel' <type>reflect</type> with <readme>index</readme> (every topic, ~token cost), <readme>key</readme> (one topic) or <readme>true</readme> (the short orientation). Read only what a task needs."

    /// The LONG `self_extension` a `detail:"full"` root reflect carries — python's
    /// full branch, verbatim (it has no fd-wire tail sentence).
    static let selfExtensionFull =
        "A DURABLE PROGRAM is a FILE you write plus a command that runs it — not a `code`-carrying agent (there is no `code` field). A program carried as a JSON string is exactly how a call gets lost to escaping. Assemble these THREE pieces and you have built something that persists and re-runs:\n1. WRITE THE PROGRAM TO A FILE: never carry file contents in a JSON string. Use the file leg's STREAM verbs — <type>write_stream</type> <path>…</path> <truncate>true</truncate> <bytes>…</bytes> to write, <type>read_stream</type> <path>…</path> <offset>0</offset> to read, one raw chunk at a time. `bytes` is VERBATIM: the program between `<bytes>` and `</bytes>` exactly as written, real newlines, nothing escaped. The text verbs (`write`, `edit`, `append`) take `bytes` the same way.\n2. RUN IT: the executor is a general-purpose COMMAND runner. Reflect it, read its `allowed` regexes, then send <type>start</type> <command>path</command> <args><item>…</item></args>; poll <type>status</type>. It runs commands, not inline programs.\n3. RE-RUN: the file is durable on disk, so the SAME `start` command run again later re-executes the SAME program against whatever the inputs hold THEN — that is how a thing 'keeps itself up to date'.\nA WORKER THAT COMPUTES is an `exec_bridge.tools` agent (the EXISTING command runner) that owns a program file — you never author a new handler_module (you cannot add modules to the catalog and do not need to). 'A specialist with child workers' = a parent agent whose children are exec_bridge runners, each with its own program file.\nWIRING TO A SIBLING: a record is edited with `update_agent` on the root — holding that grant (both ends) you edit ANY record, `totals`/`readings` included; without it you cannot raise your OWN authority. To let your worker call `totals`: grant the worker `can_call:{totals}` AND `update_agent id='totals'` to add the worker to `totals.callable_by` (send the WHOLE map — update replaces, not merges).\nA SPAWNED PROGRAM reaches agents over its control fd — a TWO-WAY socket (FANTASTIC_CTRL_FD): write {op:'send', rid, target, payload}; the reply comes back on the SAME fd as {op:'reply', rid, data}. It is not inline — read frames until one carries your `rid`, skipping event/inbox frames.\nCOMPOSE the agents you were given for everything else (`list_agents`, reflect each, send). CREATE/DELETE plain agents (create_agent with a handler_module and grants, delete_agent) work as before."
}
