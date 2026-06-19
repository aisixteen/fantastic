// The canonical root readme — the single substrate doc.
//
// `Resources/root_readme.md` is a byte-identical copy of the canonical
// `python/bundled_agents/loader/kernel_state/src/kernel_state/readme.md`.
// It is the one source that
// feeds BOTH the on-disk `.fantastic/readme.md` (seeded at boot) and
// `reflect readme=true` on the root — they must be identical. The
// cross-runtime parity test byte-diffs all three.

import Foundation

public enum RootReadme {
    /// The canonical readme text, loaded once from the bundled resource.
    public static let text: String = {
        guard
            let url = Bundle.module.url(
                forResource: "root_readme", withExtension: "md"),
            let s = try? String(contentsOf: url, encoding: .utf8)
        else {
            return ""
        }
        return s
    }()

    /// Seed `<workdir>/.fantastic/readme.md` from [`text`] if missing.
    /// Idempotent — preserves any operator edits. Mirrors Rust's
    /// `seed_root_readme` and Python's `Core._seed_root_readme`.
    public static func seed(workdir: URL) {
        let dest = workdir
            .appendingPathComponent(".fantastic")
            .appendingPathComponent("readme.md")
        let fm = FileManager.default
        if fm.fileExists(atPath: dest.path) { return }
        try? fm.createDirectory(
            at: dest.deletingLastPathComponent(),
            withIntermediateDirectories: true)
        try? text.write(to: dest, atomically: true, encoding: .utf8)
    }
}
