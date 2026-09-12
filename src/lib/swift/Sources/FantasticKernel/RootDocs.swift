// The keyed manual — a doc TREE beside the short `root_readme.md` orientation.
//
// `Resources/docs/` is a copy of python's `kernel_state/docs/`: `<key>.md` is a
// topic, a sibling dir `<key>/` extends it with sub-docs (`<key>/<sub>`) —
// nested dirs are tiers. The tree feeds BOTH the on-disk `.fantastic/docs/`
// (seeded at boot, copy-if-missing) and `reflect readme=index` /
// `readme=<key>` on the root. Mirrors Python's `Agent._readme_index` walk and
// Rust's `fantastic_core::DOCS`.

import Foundation

public enum RootDocs {
    /// One topic: its key (`frontend/routing` for a nested one), its body, and
    /// the sub-docs of its sibling dir.
    public struct Node: Sendable {
        public let key: String
        public let body: String
        public let children: [Node]
    }

    /// Walk a `docs/` directory the way python's `_readme_index` does: every
    /// `*.md` in sorted name order, each extended by the children of a sibling
    /// dir of the same stem. Absent or empty dir → `[]`.
    public static func walk(_ dir: URL, prefix: String = "") -> [Node] {
        let fm = FileManager.default
        guard let names = try? fm.contentsOfDirectory(atPath: dir.path) else { return [] }
        var out: [Node] = []
        for name in names.sorted() where name.hasSuffix(".md") {
            let file = dir.appendingPathComponent(name)
            var isDir: ObjCBool = false
            guard fm.fileExists(atPath: file.path, isDirectory: &isDir), !isDir.boolValue,
                let body = try? String(contentsOf: file, encoding: .utf8)
            else { continue }
            let stem = String(name.dropLast(3))
            let key = prefix + stem
            let sub = dir.appendingPathComponent(stem)
            let children =
                fm.fileExists(atPath: sub.path, isDirectory: &isDir) && isDir.boolValue
                ? walk(sub, prefix: key + "/") : []
            out.append(Node(key: key, body: body, children: children))
        }
        return out
    }

    /// The embedded tree, read once from the bundled resources.
    public static let tree: [Node] = {
        guard let dir = Bundle.module.resourceURL?.appendingPathComponent("docs") else { return [] }
        return walk(dir)
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
