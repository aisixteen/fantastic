// The canonical root readme — the single substrate doc.
//
// `Resources/root_readme.md` is a byte-identical copy of the canonical
// `python/bundled_agents/loader/kernel_state/src/kernel_state/readme.md`.
// It is what `reflect readme=true` on the root serves — never copied to disk.

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
}
