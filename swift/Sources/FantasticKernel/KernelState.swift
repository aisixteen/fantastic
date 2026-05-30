// Kernel snapshot for save / load.
//
// Mirrors Rust's `fantastic_kernel::state::{KernelState, CURRENT_VERSION}`.

import FantasticJSON
import Foundation

public struct KernelState: Sendable, Codable, Equatable {
    public static let currentVersion: UInt32 = 1

    public var version: UInt32
    public var agents: [AgentRecord]

    public init(version: UInt32 = KernelState.currentVersion, agents: [AgentRecord]) {
        self.version = version
        self.agents = agents
    }
}

extension Kernel {
    /// Snapshot the current kernel state as a serializable
    /// `KernelState`. Agents are sorted by id for deterministic
    /// output.
    public func snapshotState() -> KernelState {
        let records = allAgents()
            .map { $0.record() }
            .sorted { $0.id < $1.id }
        return KernelState(agents: records)
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

    /// Write the snapshot to `<workdir>/.fantastic/state.json` in
    /// Disk mode. In InMemory mode this is a no-op.
    ///
    /// Distinct from the shim `save() -> String` in PublicAPI.swift
    /// (which mirrors Rust UniFFI's `save() -> String` and returns
    /// the JSON in-memory). Use this when you have a Disk-mode
    /// kernel and want it to flush.
    public func saveToDisk() throws {
        guard let stateFile = storage.stateFile else { return }
        let json = try saveJSON()
        guard let data = json.data(using: .utf8) else { return }
        try? FileManager.default.createDirectory(
            at: stateFile.deletingLastPathComponent(),
            withIntermediateDirectories: true)
        try data.write(to: stateFile, options: .atomic)
    }

    /// Hydrate the kernel from a snapshot JSON string. Validates:
    ///   - version ≤ currentVersion
    ///   - no duplicate ids
    ///   - every non-root agent has its parent in the snapshot
    ///   - exactly one root (parent_id == nil)
    /// Bundles with unknown handler_modules are skipped (weak-load).
    public func loadJSON(_ string: String) throws {
        let json = try JSON.parse(string)
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
        for rec in state.agents {
            if seen.contains(rec.id) {
                throw KernelError.invalidSnapshot("duplicate id \(rec.id)")
            }
            seen.insert(rec.id)
        }
        // Root + parent integrity check.
        var roots = 0
        for rec in state.agents {
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
            // Weak-load: skip if handler_module is set but unknown.
            // The agent shell still registers so children can wire,
            // but verbs targeting it return "no bundle" until the
            // bundle lands. Mirrors Python's weak-load policy.
            if let hm = rec.handlerModule, bundles.get(hm) == nil {
                _ = hm
            }
            let rootPath: URL
            if let pr = parentRootPath {
                rootPath = pr
                    .appendingPathComponent("agents")
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
            for child in state.agents where child.parentId == rec.id {
                construct(child, parentRootPath: rootPath)
            }
        }

        // Start from the root (parent_id == nil). Required to exist
        // by the validation pass above.
        if let rootRec = state.agents.first(where: { $0.parentId == nil }) {
            construct(rootRec, parentRootPath: nil)
            if let rootAgent = registry[AgentId(rootRec.id)] {
                setRoot(rootAgent)
            }
        }

        // Wire parent-child relationships now that every agent has a
        // proper rootPath.
        for rec in state.agents {
            if let pid = rec.parentId, let parent = registry[AgentId(pid)],
                let child = registry[AgentId(rec.id)]
            {
                parent.insertChild(child)
            }
        }
    }
}
