// Per-agent agent.json persistence (merge-only).
//
// Mirrors Rust's `persistence.rs`. Critical invariants:
//
//   - InMemory mode → no-op
//   - Disk mode → read existing agent.json (if any), overlay
//     kernel-managed fields, write back. NEVER wholesale-overwrite
//     because bundles may have written sidecar fields the kernel
//     doesn't know about.

import FantasticJSON
import Foundation
import OrderedCollections

public enum Persistence {
    public enum PersistenceError: Error {
        case ioFailure(String)
    }

    /// Write `agent`'s record to disk, merging into any existing
    /// agent.json. Ephemeral agents and InMemory mode skip silently.
    public static func persist(agent: Agent, storage: StorageMode) throws {
        guard case .disk = storage else { return }
        if agent.ephemeral { return }
        let fm = FileManager.default
        try? fm.createDirectory(
            at: agent.rootPath, withIntermediateDirectories: true)
        let file = agent.agentFile

        var existing: OrderedDictionary<String, JSON> = [:]
        if let data = try? Data(contentsOf: file),
            let parsed = try? JSON.parse(data),
            case let .object(d) = parsed
        {
            existing = d
        }

        // Overlay kernel-managed fields. `id` + `handler_module` +
        // `parent_id` get refreshed; meta keys flatten on top.
        existing["id"] = .string(agent.id.value)
        if let hm = agent.handlerModule {
            existing["handler_module"] = .string(hm)
        } else {
            existing["handler_module"] = nil
        }
        if let pid = agent.parentId {
            existing["parent_id"] = .string(pid.value)
        } else {
            existing["parent_id"] = nil
        }
        for (k, v) in agent.meta {
            existing[k] = v
        }

        let merged: JSON = .object(existing)
        // On-disk format matches Python's
        //   `self._agent_file().write_text(json.dumps(self.record, indent=2))`
        // so cross-runtime workdir handoff produces byte-identical
        // agent.json files. Compact form is reserved for the wire
        // protocol (`.serialize()`).
        let serialized = merged.serializePretty(indent: 2)
        guard let bytes = serialized.data(using: .utf8) else {
            throw PersistenceError.ioFailure("non-UTF-8 serialization")
        }
        try bytes.write(to: file, options: .atomic)
    }

    /// Seed `<agent.root>/readme.md` with `content` if it doesn't
    /// already exist. Used at create_agent time when the bundle ships
    /// a non-nil readme.
    public static func seedReadme(agent: Agent, content: String, storage: StorageMode)
        throws
    {
        guard case .disk = storage else { return }
        let fm = FileManager.default
        try? fm.createDirectory(
            at: agent.rootPath, withIntermediateDirectories: true)
        let file = agent.readmeFile
        if fm.fileExists(atPath: file.path) { return }
        try content.write(to: file, atomically: true, encoding: .utf8)
    }

    /// Remove `agent`'s root directory recursively. Best-effort —
    /// non-existent paths are not an error.
    public static func remove(agent: Agent, storage: StorageMode) throws {
        guard case .disk = storage else { return }
        let fm = FileManager.default
        if fm.fileExists(atPath: agent.rootPath.path) {
            try? fm.removeItem(at: agent.rootPath)
        }
    }

    /// Recursively walk the nested on-disk agent tree and decode every
    /// `agent.json` into an `AgentRecord`. The canonical layout nests
    /// children under their parent:
    ///
    ///   <agentsDir>/<id>/agent.json
    ///   <agentsDir>/<id>/agents/<child>/agent.json
    ///   <agentsDir>/<id>/agents/<child>/agents/<grandchild>/agent.json
    ///
    /// so a shallow read (direct children only) would drop every
    /// nested agent — canvas members, a `web_ws` under `web`, etc. —
    /// and the one-shot CLI / daemon would boot a partial fleet.
    /// Mirrors Python's `_load_children` recursion.
    ///
    /// Failures (missing file, unparseable JSON) are LOGGED to stderr
    /// and skipped — mirrors the weak-load policy the substrate
    /// applies for unknown handler_modules. The kernel deals with a
    /// partial fleet gracefully.
    ///
    /// Returns records depth-first. The caller wraps them in a
    /// `KernelState` and calls `kernel.load(_:)`, which wires the tree
    /// by `parent_id` (order-independent) and validates root + parent
    /// integrity.
    public static func readAllAgentRecords(from agentsDir: URL) -> [AgentRecord] {
        var records: [AgentRecord] = []
        let decoder = JSONDecoder()
        let fm = FileManager.default

        func walk(_ dir: URL) {
            guard
                let entries = try? fm.contentsOfDirectory(
                    at: dir, includingPropertiesForKeys: nil,
                    options: [.skipsHiddenFiles])
            else {
                return
            }
            for entry in entries {
                // Each direct child of `dir` is `<id>/`, containing an
                // `agent.json` plus (optionally) a nested `agents/` dir.
                var isDir: ObjCBool = false
                guard
                    fm.fileExists(atPath: entry.path, isDirectory: &isDir),
                    isDir.boolValue
                else { continue }
                let agentFile = entry.appendingPathComponent("agent.json")
                if let data = try? Data(contentsOf: agentFile) {
                    do {
                        let record = try decoder.decode(AgentRecord.self, from: data)
                        records.append(record)
                    } catch {
                        FileHandle.standardError.write(
                            "[kernel] skipping malformed agent.json at \(agentFile.path): \(error)\n"
                                .data(using: .utf8) ?? Data())
                    }
                }
                // Recurse into this agent's children, if any.
                let childrenDir = entry.appendingPathComponent("agents")
                var childIsDir: ObjCBool = false
                if fm.fileExists(atPath: childrenDir.path, isDirectory: &childIsDir),
                    childIsDir.boolValue
                {
                    walk(childrenDir)
                }
            }
        }

        walk(agentsDir)
        return records
    }
}
