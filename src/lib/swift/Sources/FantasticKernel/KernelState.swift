// Kernel snapshot for save / load.
//
// Mirrors Rust's `fantastic_kernel::state::{KernelState, CURRENT_VERSION}`.

import FantasticJSON
import Foundation

/// python `Kernel.save()`'s shape: `{version, records}`.
public struct KernelState: Sendable, Codable, Equatable {
    public static let currentVersion: UInt32 = 1

    public var version: UInt32
    public var records: [AgentRecord]

    public init(version: UInt32 = KernelState.currentVersion, records: [AgentRecord]) {
        self.version = version
        self.records = records
    }

    /// python `snapshot.get("version", CURRENT_VERSION)` /
    /// `snapshot.get("records", [])` — both keys default.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        version = try c.decodeIfPresent(UInt32.self, forKey: .version) ?? KernelState.currentVersion
        records = try c.decodeIfPresent([AgentRecord].self, forKey: .records) ?? []
    }
}

extension Kernel {
    /// Snapshot the current kernel state as a serializable
    /// `KernelState`. Agents are sorted by id for deterministic
    /// output.
    public func snapshotState() -> KernelState {
        // Drop the ephemeral subtree TRANSITIVELY.
        //
        // Dropping only the ephemeral agent ITSELF leaves its children behind as
        // orphans — records whose `parent_id` names an agent that is not in the
        // document — and `load` rejects exactly that. So `save` would emit a
        // snapshot its own loader refused.
        let all = allAgents()
        var dropped = Set(all.filter { $0.ephemeral }.map { $0.id })
        // Walk down until nothing new is reached: a child of a dropped agent is
        // dropped, and so is ITS child.
        var grew = true
        while grew {
            grew = false
            for a in all where !dropped.contains(a.id) {
                if let p = a.parentId, dropped.contains(p) {
                    dropped.insert(a.id)
                    grew = true
                }
            }
        }
        let records = all
            .filter { !dropped.contains($0.id) }
            .map { $0.record() }
            .sorted { $0.id < $1.id }
        return KernelState(records: records)
    }

    /// Serialize the state snapshot to a UTF-8 JSON string. Used
    /// by `save` (writes to disk) + tested by callers in-memory.
    public func saveJSON() throws -> String {
        let state = snapshotState()
        let data = try JSONEncoder().encode(state)
        // Re-parse + re-serialize via our order-preserving path so
        // bytes are stable.
        let json = try JSON.parse(data)
        return json.serialize()
    }

    /// Hydrate the kernel from a snapshot JSON string. Validates:
    ///   - version ≤ currentVersion
    ///   - no duplicate ids
    ///   - every non-root agent has its parent in the snapshot
    ///   - exactly one root (parent_id == nil)
    /// Bundles with unknown handler_modules are skipped (weak-load).
    public func loadJSON(_ string: String) throws {
        var json = try JSON.parse(string)
        // python `Kernel.load` takes a bare record list as well as the
        // `{version, records}` envelope `save` writes.
        if case .array = json { json = .object(["records": json]) }
        let data = try JSONEncoder().encode(json)
        let state = try JSONDecoder().decode(KernelState.self, from: data)
        try load(state)
    }

    /// Hydrate from a `KernelState` directly.
    public func load(_ state: KernelState) throws {
        guard state.version <= KernelState.currentVersion else {
            throw KernelError.invalidSnapshot(
                "version \(state.version) > supported \(KernelState.currentVersion)")
        }

        // Duplicate check.
        var seen: Set<String> = []
        for rec in state.records {
            // RESERVED KEYS ARE REFUSED HERE — before anything is torn down.
            // These name substrate wiring, not agent meta, and a hand-authored
            // `root_path:` once cleared the whole tree and THEN raised: the
            // kernel was bricked by a field that looked like configuration.
            // Validation before teardown is what makes a bad document a no-op.
            for k in reservedRecordKeys where rec.meta[k] != nil {
                throw KernelError.invalidSnapshot(
                    "record \(rec.id) carries reserved key \(k) — that names substrate "
                        + "wiring, not agent meta")
            }
            // THE SAME ID RULE AS A CREATE: a record edited on disk to carry the
            // reserved `kernel`, a foreign `guid|agent`, or a path-walking id is
            // refused here, before the live tree is torn down.
            if let bad = Refusal.guardAgentId("load", rec.id) {
                throw KernelError.invalidSnapshot(
                    "record \(rec.id): \(bad["error"].asString ?? "")")
            }
            if seen.contains(rec.id) {
                throw KernelError.invalidSnapshot("duplicate id \(rec.id)")
            }
            seen.insert(rec.id)
        }
        // Root + parent integrity check.
        var roots = 0
        for rec in state.records {
            if rec.parentId == nil {
                roots += 1
            } else if let p = rec.parentId, !seen.contains(p) {
                throw KernelError.invalidSnapshot(
                    "dangling parent_id \(p) on agent \(rec.id)")
            }
        }
        if roots == 0 {
            throw KernelError.invalidSnapshot("no root agent (parent_id=null)")
        }
        if roots > 1 {
            throw KernelError.invalidSnapshot(
                "snapshot has \(roots) roots; expected exactly one")
        }
        // REACHABILITY. Every parent resolving is not enough: a CYCLE
        // (a -> b -> a) resolves on both ends and is unreachable from the root,
        // so those records were silently dropped — a document naming five agents
        // booted a kernel holding three, with no word about the other two.
        // Caught HERE, with the other pre-teardown checks, so a bad snapshot is
        // a no-op rather than a half-destroyed tree.
        var kids: [String: [String]] = [:]
        var rootId = ""
        for rec in state.records {
            if let p = rec.parentId {
                kids[p, default: []].append(rec.id)
            } else {
                rootId = rec.id
            }
        }
        var reached: Set<String> = []
        var stack = [rootId]
        while let node = stack.popLast() {
            if !reached.insert(node).inserted { continue }
            stack.append(contentsOf: kids[node] ?? [])
        }
        let orphans = state.records.map(\.id).filter { !reached.contains($0) }.sorted()
        if !orphans.isEmpty {
            throw KernelError.invalidSnapshot(
                "snapshot has \(orphans.count) record(s) unreachable from the root "
                    + "\"\(rootId)\" (a cycle or an orphaned parent chain): \(orphans)")
        }

        // THE ROOT RECORD DECLARES THIS TREE'S CHILDREN-DIR NAME (declared
        // config, default `agents`). Set BEFORE the build, since every agent
        // derives its address from it. Python `_kernel.py` `load`.
        let declared = state.records.first { $0.parentId == nil }?
            .meta["children_dir"]?.asString
        childrenDir = (declared?.isEmpty == false) ? declared! : "agents"

        // Wipe existing state.
        for a in allAgents() {
            unregister(a.id)
        }

        // Compute the base rootPath for the substrate root. In disk
        // mode, this is `<workdir>/.fantastic`. In in-memory mode,
        // it's an empty URL — no persistence ever fires for in-memory
        // agents, so the value doesn't matter beyond being consistent.
        let baseRoot: URL
        if let workdir = storage.workdir {
            baseRoot = workdir.appendingPathComponent(".fantastic")
        } else {
            baseRoot = URL(fileURLWithPath: "")
        }

        // Build a top-down construction: each agent's rootPath is
        // parent.childrenDir/<id>. Without this, subsequent
        // `create_agent` writes after hydration would land in the
        // wrong directory (e.g. `<cwd>/agents/<id>/` instead of
        // `<workdir>/.fantastic/agents/<id>/`).
        var registry: [AgentId: Agent] = [:]

        func construct(_ rec: AgentRecord, parentRootPath: URL?) {
            // WEAK LOAD. A record whose bundle is not installed in THIS runtime
            // (the browser's `*.ts` agents persist into the same tree) is
            // skipped along with its whole subtree, and says so on stderr in
            // python's exact words — CI and the selftests grep this line.
            // Registering the shell instead put an agent in the tree that could
            // answer nothing and still handed its children a live parent.
            if let hm = rec.handlerModule, bundles.get(hm) == nil {
                RuntimeLog.warn(
                    "[kernel] skipping agent \(rec.id): bundle \(hm) not installed in "
                        + "this runtime")
                return
            }
            let rootPath: URL
            if let pr = parentRootPath {
                rootPath = pr
                    .appendingPathComponent(childrenDir)
                    .appendingPathComponent(rec.id)
            } else {
                rootPath = baseRoot
            }
            let agent = Agent(
                id: AgentId(rec.id),
                handlerModule: rec.handlerModule,
                parentId: rec.parentId.map { AgentId($0) },
                meta: rec.meta,
                rootPath: rootPath
            )
            registry[agent.id] = agent
            register(agent)
            // Recurse into this agent's children (records that name
            // it as their parent_id).
            for child in state.records where child.parentId == rec.id {
                construct(child, parentRootPath: rootPath)
            }
        }

        // Start from the root (parent_id == nil). Required to exist
        // by the validation pass above.
        if let rootRec = state.records.first(where: { $0.parentId == nil }) {
            construct(rootRec, parentRootPath: nil)
            if let rootAgent = registry[AgentId(rootRec.id)] {
                setRoot(rootAgent)
            }
            // THE COUNTER COMES BACK WITH THE GUID: the root record's reserved
            // bound is where this run starts, and this run's first block is
            // reserved in memory. `load` never edits a record (save → load → save
            // is identical); `start` writes the bound, and a started kernel
            // persists every later block. Python `Kernel.load`.
            restoreCounter(from: rootRec.meta)
        }

        // Wire parent-child relationships now that every agent has a
        // proper rootPath.
        for rec in state.records {
            if let pid = rec.parentId, let parent = registry[AgentId(pid)],
                let child = registry[AgentId(rec.id)]
            {
                parent.insertChild(child)
            }
        }
    }
}
