// The keyed manual — a doc TREE beside the short `root_readme.md` orientation.
//
// `Resources/docs/` is a copy of python's `kernel_state/docs/`: `<key>.md` is a
// topic, a sibling dir `<key>/` extends it with sub-docs (`<key>/<sub>`) —
// nested dirs are tiers. The tree feeds BOTH the on-disk `.fantastic/docs/`
// (seeded at boot, copy-if-missing) and `reflect readme=index` /
// `readme=<key>` on the root. Mirrors Python's `Agent._readme_index` walk and
// Rust's `fantastic_core::DOCS`.

import FantasticJSON
import Foundation

public enum RootDocs {
    /// One topic: its key (`frontend/routing` for a nested one), its body, and
    /// the sub-docs of its sibling dir.
    public struct Node: Sendable {
        public let key: String
        public let body: String
        public let children: [Node]
    }

    /// Python `Path.suffix`: the LAST dot in the name, but only counted when it
    /// is neither the first character (a dotfile has no suffix) nor the last (a
    /// trailing dot has no suffix either). A file named exactly `.md` has NO
    /// suffix by this rule — `hasSuffix(".md")` said otherwise and, paired with
    /// a stem of `dropLast(3)` giving `""`, walked back into the SAME directory
    /// forever (C62; measured: kills the kernel, drops the REST connection).
    static func pySuffix(_ name: String) -> String {
        guard let i = name.lastIndex(of: ".") else { return "" }
        let idx = name.distance(from: name.startIndex, to: i)
        guard idx > 0, idx < name.count - 1 else { return "" }
        return String(name[i...])
    }

    /// Python `Path.stem`: the name with [`pySuffix`] removed.
    static func pyStem(_ name: String) -> String {
        let suf = pySuffix(name)
        return suf.isEmpty ? name : String(name.dropLast(suf.count))
    }

    /// Walk a `docs/` directory the way python's `_readme_index` does: every
    /// file whose [`pySuffix`] is `.md`, in sorted name order, each extended by
    /// the children of a sibling dir of the same [`pyStem`]. Absent or empty dir
    /// → `[]`.
    ///
    /// Python text mode (C59): UTF-8 decoded, `\r\n`/lone `\r` → `\n` — `body`,
    /// and everything derived from it (`desc`, `tokens`), is the TRANSLATED
    /// text. An OSError (permission, a race with a delete) is skipped like
    /// python's `except OSError: continue`; a `UnicodeDecodeError` is NOT an
    /// OSError — python lets it propagate, so this throws and fails the whole
    /// reflect (C60).
    public static func walk(_ dir: URL, prefix: String = "") throws(PyError) -> [Node] {
        let fm = FileManager.default
        guard let names = try? fm.contentsOfDirectory(atPath: dir.path) else { return [] }
        var out: [Node] = []
        for name in names.sorted() {
            guard pySuffix(name) == ".md" else { continue }
            let file = dir.appendingPathComponent(name)
            var isDir: ObjCBool = false
            guard fm.fileExists(atPath: file.path, isDirectory: &isDir), !isDir.boolValue,
                let data = try? Data(contentsOf: file)
            else { continue }
            if let decodeError = pyUtf8DecodeError(data) {
                throw PyError("UnicodeDecodeError", decodeError)
            }
            let body = pyUniversalNewlines(String(decoding: data, as: UTF8.self))
            let stem = pyStem(name)
            let key = prefix + stem
            let sub = dir.appendingPathComponent(stem)
            let children: [Node] =
                fm.fileExists(atPath: sub.path, isDirectory: &isDir) && isDir.boolValue
                ? try walk(sub, prefix: key + "/") : []
            out.append(Node(key: key, body: body, children: children))
        }
        return out
    }

    /// The embedded tree, read once from the bundled resources. Always valid
    /// UTF-8 (our own resources) — a decode error here would mean a bundling
    /// bug, not a caller mistake, so it collapses to the empty tree rather than
    /// failing every in-memory reflect.
    public static let tree: [Node] = {
        guard let dir = Bundle.module.resourceURL?.appendingPathComponent("docs") else { return [] }
        return (try? walk(dir)) ?? []
    }()

    /// Every key, pre-order — python's `_readme_keys`.
    public static func keys(_ nodes: [Node]) -> [String] {
        nodes.flatMap { [$0.key] + keys($0.children) }
    }

    /// `(key, body)` for every topic, pre-order, nested keys included.
    public static let all: [(key: String, body: String)] = flatten(tree)

    private static func flatten(_ nodes: [Node]) -> [(key: String, body: String)] {
        nodes.flatMap { [(key: $0.key, body: $0.body)] + flatten($0.children) }
    }

    /// Seed `<workdir>/.fantastic/docs/<key>.md` from [`all`] — nested keys land
    /// in nested dirs — copy-if-missing (preserves operator edits). Called at
    /// boot beside `RootReadme.seed`.
    public static func seed(workdir: URL) {
        let dir =
            workdir
            .appendingPathComponent(".fantastic")
            .appendingPathComponent("docs")
        let fm = FileManager.default
        for (key, body) in all {
            let dest = dir.appendingPathComponent("\(key).md")
            if fm.fileExists(atPath: dest.path) { continue }
            try? fm.createDirectory(
                at: dest.deletingLastPathComponent(), withIntermediateDirectories: true)
            try? body.write(to: dest, atomically: true, encoding: .utf8)
        }
    }
}
