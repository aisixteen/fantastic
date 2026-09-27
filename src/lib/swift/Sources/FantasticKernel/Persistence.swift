// Per-agent agent.yaml persistence (merge-only).
//
// Mirrors Rust's `persistence.rs`. Critical invariants:
//
//   - InMemory mode → no-op
//   - Disk mode → read existing agent.yaml (if any), overlay
//     kernel-managed fields, write back. NEVER wholesale-overwrite
//     because bundles may have written sidecar fields the kernel
//     doesn't know about.

import FantasticJSON
import Foundation
import OrderedCollections

/// The record file every agent's dir carries (python `_RECORD_FILE`).
public let RECORD_FILE = "agent.yaml"

public enum Persistence {
    public enum PersistenceError: Error {
        case ioFailure(String)
    }

    /// One record file → its mapping, or nil when absent / corrupt / not a
    /// mapping. Python `_read_record`.
    public static func readRecord(_ file: URL) -> OrderedDictionary<String, JSON>? {
        guard let text = try? String(contentsOf: file, encoding: .utf8),
            case .object(let d)? = try? YamlIO.load(text)
        else { return nil }
        return d
    }

    /// A record's bytes — python `messages_yaml.dump(record)`.
    public static func recordBytes(_ record: OrderedDictionary<String, JSON>) -> Data {
        Data(YamlIO.dump(.object(record)).utf8)
    }

    /// Write `agent`'s record to disk, merging into any existing agent.yaml.
    /// Ephemeral agents and InMemory mode skip silently.
    ///
    /// THE COLD PRIMITIVE, python's `_testkit.persist`: the ONE direct write,
    /// for materializing a live record without a provider. Ongoing persistence
    /// rides the root loader's debounced `RecordWriter` through the discovered
    /// store — persisting at the verb is what C14 removed.
    ///
    /// The record persists AS-IS — a single `agent.yaml`. There is no `code`/
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

        var existing = readRecord(file) ?? [:]

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

        // On-disk format is python's `messages_yaml.dump(record)`, byte for
        // byte, so a cross-runtime workdir handoff reads the same file.
        try recordBytes(existing).write(to: file, options: .atomic)
    }

    /// Walk a `.fantastic` tree → a flat list of records (root first, then
    /// descendants depth-first). Port of python `kernel_state.read_tree`.
    ///
    ///   <rootDir>/agent.yaml                      ← the root record
    ///   <rootDir>/<children_dir>/<id>/agent.yaml
    ///   <rootDir>/<children_dir>/<id>/<children_dir>/<child>/agent.yaml
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
    /// no `agent.yaml`, or one that does not parse, is skipped SILENTLY and is
    /// NOT descended into — a corrupt record takes its subtree with it, exactly
    /// as python's `_walk` does, rather than orphaning children onto a parent
    /// that failed to load. Entries are visited in sorted order (`fs.list_dir`).
    /// A MISSING or unparseable root record yields the children alone, parented
    /// to `fallbackRootId` (python `read_tree`, 2026-09-20 — it used to hide the
    /// whole tree behind a reseed).
    public static func readTree(rootDir: URL, fallbackRootId: String = "kernel_state") -> [AgentRecord]
    {
        let rootFile = rootDir.appendingPathComponent(RECORD_FILE)
        var rootRec: AgentRecord? = nil
        if let obj = readRecord(rootFile), let rec = try? AgentRecord(json: .object(obj)) {
            rootRec = rec
        }
        let declared = rootRec?.meta["children_dir"]?.asString
        let childrenDir = (declared?.isEmpty == false) ? declared! : "agents"

        var records: [AgentRecord] = rootRec.map { [$0] } ?? []
        walk(
            rootDir.appendingPathComponent(childrenDir), childrenDir: childrenDir,
            parentId: rootRec?.id ?? fallbackRootId, into: &records)
        return records
    }

    /// The records under one children dir, depth-first in sorted order, each
    /// parented by its DISK POSITION (`parentId` at this level, the enclosing
    /// record below it). A missing or unparseable record is skipped and its
    /// subtree with it. `readTree` walks from a root; a loader walks its own
    /// namespace with itself as the top-level parent (`_disk_parents`). Python
    /// `_walk`.
    public static func walkRecords(_ dir: URL, childrenDir: String, parentId: String?)
        -> [AgentRecord]
    {
        var out: [AgentRecord] = []
        walk(dir, childrenDir: childrenDir, parentId: parentId, into: &out)
        return out
    }

    private static func walk(
        _ dir: URL, childrenDir: String, parentId: String?, into out: inout [AgentRecord]
    ) {
        let fm = FileManager.default
        var isDir: ObjCBool = false
        guard fm.fileExists(atPath: dir.path, isDirectory: &isDir), isDir.boolValue,
            let entries = try? fm.contentsOfDirectory(
                at: dir, includingPropertiesForKeys: nil, options: [])
        else { return }
        for entry in entries.sorted(by: { $0.lastPathComponent < $1.lastPathComponent }) {
            guard let obj = readRecord(entry.appendingPathComponent(RECORD_FILE)),
                var rec = try? AgentRecord(json: .object(obj))
            else { continue }
            rec.parentId = parentId
            out.append(rec)
            walk(
                entry.appendingPathComponent(childrenDir), childrenDir: childrenDir,
                parentId: rec.id, into: &out)
        }
    }
}
