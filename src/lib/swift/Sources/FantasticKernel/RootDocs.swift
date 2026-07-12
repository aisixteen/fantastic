// The keyed manual — a doc TREE beside the short `root_readme.md` orientation.
//
// `Resources/docs/<key>.md` are the topic files (FLAT for this runtime). They
// feed BOTH the on-disk `.fantastic/docs/` tree (seeded at boot) and
// `reflect readme=index` / `reflect readme=<key>` on the root. Mirrors Python's
// `kernel_state/docs/` + Rust's `fantastic_core::DOCS`.

import Foundation

public enum RootDocs {
    /// Ordered topic keys (mirror the on-disk `docs/` tree). `self-extension`
    /// first so a self-authoring model reaches it before the system manual.
    public static let keys: [String] = [
        "self-extension", "reflect", "agents", "memory",
        "persistence", "transports", "streams", "daemon",
    ]

    /// `(key, body)` for each topic, loaded once from the bundled resources.
    public static let all: [(key: String, body: String)] = {
        keys.compactMap { key in
            guard
                let url = Bundle.module.url(
                    forResource: key, withExtension: "md", subdirectory: "docs"),
                let s = try? String(contentsOf: url, encoding: .utf8)
            else { return nil }
            return (key, s)
        }
    }()

    /// Seed `<workdir>/.fantastic/docs/<key>.md` from [`all`], copy-if-missing
    /// (preserves operator edits). Called at boot beside `RootReadme.seed`.
    public static func seed(workdir: URL) {
        let dir =
            workdir
            .appendingPathComponent(".fantastic")
            .appendingPathComponent("docs")
        let fm = FileManager.default
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        for (key, body) in all {
            let dest = dir.appendingPathComponent("\(key).md")
            if fm.fileExists(atPath: dest.path) { continue }
            try? body.write(to: dest, atomically: true, encoding: .utf8)
        }
    }
}
