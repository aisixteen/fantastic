// Per-agent agent.yaml records on disk: read one (`readRecord`, or
// `readRecordClamped` under python's `fs` clamp), render one
// (`recordBytes`, python's `messages_yaml.dump(record)`), walk a `.fantastic`
// tree (`readTree` / `walkRecords`). Ongoing writes ride the root loader's
// record writer through the discovered store; there is no direct record write
// here — the cold materialization a test needs (python `_testkit.persist`)
// lives in that test's target.

import FantasticJSON
import Foundation
import OrderedCollections

/// The record file every agent's dir carries (python `_RECORD_FILE`).
public let RECORD_FILE = "agent.yaml"

public enum Persistence {
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
    ///
    /// Every path is clamped under `rootDir` as python's `fs` clamps it
    /// (`clamp`): a `children_dir` that resolves outside (`../x`), or a record
    /// (or its dir) linked outside, throws python's `ValueError` — `fs: path
    /// '<rel>' escapes root` — instead of hydrating records from another
    /// directory.
    ///
    /// `live` is the root's LIVE record when the caller has one (a session
    /// loader serving its namespace): the namespace keeps no copy of it on disk.
    public static func readTree(
        rootDir: URL, fallbackRootId: String = "kernel_state", live: AgentRecord? = nil
    )
        throws(PyError) -> [AgentRecord]
    {
        var rootRec: AgentRecord? = live
        if rootRec == nil, let obj = try readRecordClamped(rootDir, RECORD_FILE),
            let rec = try? AgentRecord(json: .object(obj))
        {
            rootRec = rec
        }
        let declared = rootRec?.meta["children_dir"]?.asString
        let childrenDir = (declared?.isEmpty == false) ? declared! : "agents"

        var records: [AgentRecord] = rootRec.map { [$0] } ?? []
        try walk(
            rootDir, childrenDir, childrenDir: childrenDir,
            parentId: rootRec?.id ?? fallbackRootId, into: &records)
        return records
    }

    /// The records under `root`'s children dir, depth-first in sorted order,
    /// each parented by its DISK POSITION (`parentId` at this level, the
    /// enclosing record below it). A missing or unparseable record is skipped
    /// and its subtree with it. `readTree` walks from a root; a loader walks its
    /// own namespace with itself as the top-level parent (`_disk_parents`).
    /// Python `_walk`, clamped as `readTree` is.
    public static func walkRecords(_ root: URL, childrenDir: String, parentId: String?)
        throws(PyError) -> [AgentRecord]
    {
        var out: [AgentRecord] = []
        try walk(root, childrenDir, childrenDir: childrenDir, parentId: parentId, into: &out)
        return out
    }

    private static func walk(
        _ root: URL, _ rel: String, childrenDir: String, parentId: String?,
        into out: inout [AgentRecord]
    ) throws(PyError) {
        let dir = try clamp(root, rel)
        let fm = FileManager.default
        var isDir: ObjCBool = false
        guard fm.fileExists(atPath: dir, isDirectory: &isDir), isDir.boolValue,
            let names = try? fm.contentsOfDirectory(atPath: dir)
        else { return }
        for name in names.sorted() {
            let entry = rel + "/" + name
            guard let obj = try readRecordClamped(root, entry + "/" + RECORD_FILE),
                var rec = try? AgentRecord(json: .object(obj))
            else { continue }
            rec.parentId = parentId
            out.append(rec)
            try walk(
                root, entry + "/" + childrenDir, childrenDir: childrenDir,
                parentId: rec.id, into: &out)
        }
    }

    /// python `_read_record` under the clamp: the clamp is OUTSIDE python's
    /// `try`, so a record path that escapes throws; absent or corrupt is nil.
    /// The tree walks read every record through it, and the boot reads the root
    /// record through it BEFORE any seed write, so a root `agent.yaml` linked
    /// outside refuses the boot instead of being reseeded over.
    public static func readRecordClamped(_ root: URL, _ rel: String) throws(PyError)
        -> OrderedDictionary<String, JSON>?
    {
        readRecord(URL(fileURLWithPath: try clamp(root, rel)))
    }

    /// python `fs.resolve(root, rel)` for the tree walks: `rel` resolved under
    /// the resolved `root` (`pyRealpath` — `..` and every symlink, dangling or
    /// not, followed). Python's walk opens only through `file_bridge.fs`, so a
    /// path that lands outside throws `fs`'s own `ValueError` text.
    private static func clamp(_ root: URL, _ rel: String) throws(PyError) -> String {
        let escapes = PyError("ValueError", "fs: path \(pyRepr(rel)) escapes root")
        if rel.hasPrefix("/") { throw escapes }
        let rootPath = try pyRealpath(root.path)
        let target = try rel.isEmpty ? rootPath : pyRealpath(rootPath + "/" + rel)
        let under =
            target == rootPath || target.hasPrefix(rootPath == "/" ? "/" : rootPath + "/")
        guard under else { throw escapes }
        return target
    }
}
