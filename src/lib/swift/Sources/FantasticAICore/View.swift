// THE VIEW ARRAY — the agent's STATE: the ordered ids it sees, each at a
// resolution. A `view` row in the agent's memory store (`saveView` / `loadView`
// in `Memory.swift`), written by every projection and by `context_recall`, so the
// next turn (and the next PROCESS) rebuilds the same bytes rather than a
// freshly-guessed prefix.
//
// Python `ai_core/core.py` `_res_of` / `_apply_res`; rust `view.rs`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// The resolution a rendered row is at: a fold, a stripped stub, or the row
/// itself. Python `_res_of`.
func resOf(_ row: JSON) -> String {
    if row["kind"].asString == "fold" { return "fold" }
    if let c = row["content"].asString, c.contains("(stripped:"), c.hasPrefix("[reply to ") {
        return "stub"
    }
    return "full"
}

/// Render a stored row at the resolution the view array asks for. Python
/// `_apply_res` — the stub renderer is `stub`, ONE owner shared with `strip`.
func applyRes(_ row: JSON, _ res: String) -> JSON {
    if res == "stub", let c = row["content"].asString, c.hasPrefix("[reply to ") {
        return withField(row, "content", .string(replyBlocks(c).map(stub).joined(separator: "\n\n")))
    }
    return row
}
