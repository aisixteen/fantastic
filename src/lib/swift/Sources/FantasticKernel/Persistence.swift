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

    /// Write `agent`'s record to disk, merging into any existing agent.json.
    /// Ephemeral agents and InMemory mode skip silently.
    ///
    /// THE COLD PRIMITIVE, python's `_testkit.persist`: the ONE direct write,
    /// for materializing a live record without a provider. Ongoing persistence
    /// rides the root loader's debounced `RecordWriter` through the discovered
    /// store — persisting at the verb is what C14 removed.
    ///
    /// The record persists AS-IS — a single `agent.json`. There is no `code`/
    /// `main.py` split: exec_bridge is a general-purpose COMMAND runner now, not a
    /// code runner, so no bundle persists program text as a record field. Mirrors
    /// python + rust.
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

    /// Walk a `.fantastic` tree → a flat list of records (root first, then
    /// descendants depth-first). Port of python `kernel_state.read_tree`.
    ///
    ///   <rootDir>/agent.json                      ← the root record
    ///   <rootDir>/<children_dir>/<id>/agent.json
    ///   <rootDir>/<children_dir>/<id>/<children_dir>/<child>/agent.json
    ///
    /// THE DISK POSITION IS AUTHORITATIVE: each record's `parent_id` is
    /// overwritten with the id of the agent whose children dir it sits in (the
    /// root's id at the top level), so a stale or hand-edited `parent_id` can
    /// never contradict where the record actually lives.
    ///
    /// The container's NAME is declared by the root record (`children_dir`,
    /// default `agents`) — a self-describing tree may call it `host_agents`.
    ///
    /// Pure read; the bootstrap calls it BEFORE any agent exists. An entry with
    /// no `agent.json`, or one that does not parse, is skipped SILENTLY and is
    /// NOT descended into — a corrupt record takes its subtree with it, exactly
    /// as python's `_walk` does, rather than orphaning children onto a parent
    /// that failed to load. Entries are visited in sorted order (`fs.list_dir`).
    /// A MISSING or unparseable root record yields the children alone, parented
    /// to `fallbackRootId` (python `read_tree`, 2026-09-20 — it used to hide the
    /// whole tree behind a reseed).
    public static func readTree(rootDir: URL, fallbackRootId: String = "kernel_state") -> [AgentRecord]
    {
        let fm = FileManager.default
        let rootFile = rootDir.appendingPathComponent("agent.json")
        var rootRec: AgentRecord? = nil
        if let data = try? Data(contentsOf: rootFile),
            let parsed = try? JSON.parse(data),
            let rec = try? AgentRecord(json: parsed)
        {
            rootRec = rec
        }
        let declared = rootRec?.meta["children_dir"]?.asString
        let childrenDir = (declared?.isEmpty == false) ? declared! : "agents"

        var records: [AgentRecord] = rootRec.map { [$0] } ?? []

        func walk(_ dir: URL, parentId: String?) {
            var isDir: ObjCBool = false
            guard fm.fileExists(atPath: dir.path, isDirectory: &isDir), isDir.boolValue,
                let entries = try? fm.contentsOfDirectory(
                    at: dir, includingPropertiesForKeys: nil, options: [])
            else { return }
            for entry in entries.sorted(by: { $0.lastPathComponent < $1.lastPathComponent }) {
                let agentFile = entry.appendingPathComponent("agent.json")
                guard let data = try? Data(contentsOf: agentFile),
                    let parsed = try? JSON.parse(data),
                    var rec = try? AgentRecord(json: parsed)
                else { continue }
                rec.parentId = parentId
                records.append(rec)
                walk(entry.appendingPathComponent(childrenDir), parentId: rec.id)
            }
        }

        walk(rootDir.appendingPathComponent(childrenDir), parentId: rootRec?.id ?? fallbackRootId)
        return records
    }
}
