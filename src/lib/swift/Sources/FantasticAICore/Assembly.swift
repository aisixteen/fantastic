// Prompt assembly — the shared system-prompt builder behind every
// Swift LLM backend. Mirrors Rust's `fantastic-ai-core::assembly` and
// the Python `ai_core._assemble`: the model's DEFAULT system block is a
// lean primer (id-index of the tree + bundle catalog), the agent's own
// self-reflect, and the context-window how-to. NO agent menu, NO tool
// how-to — this backend does not run a tool loop; a caller that wants
// tool-calling supplies its own `system_prompt` teaching its convention.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Render a reflect reply into a one-line `sentence  k=v  k=v` blurb.
func renderReflect(_ v: JSON) -> String {
    guard var obj = v.asObject else { return "" }
    let sentence = obj["sentence"]?.asString ?? ""
    obj["sentence"] = nil
    var parts: [String] = []
    for (k, val) in obj {
        let rendered = val.asString ?? val.serialize()
        parts.append("\(k)=\(rendered)")
    }
    let fields = parts.joined(separator: "  ")
    return "\(sentence)  \(fields)".trimmingCharacters(in: .whitespaces)
}

/// The context-window how-to, appended to the DEFAULT system block. Context is
/// the backend's (it owns the conversation), so it teaches the recall /
/// context_status affordances — unlike tool-calling, which is the caller's.
/// Byte-for-byte the Python `_CONTEXT_HOWTO` (and Rust `CONTEXT_HOWTO`).
let CONTEXT_HOWTO = """
    ## Context window & compaction
    Your live context is finite. When the conversation outgrows it, the system COMPACTS your
    view: older turns are summarized or elided and a `[context-notice]` turn is inserted. The
    DURABLE transcript is ALWAYS whole — compaction only shapes what you see right now, and
    nothing is ever truly lost.
    - To page dropped turns back on demand: `send('<your_own_id>', {type:'recall', query?, limit?})`.
      `query` substring-filters your full history; `limit` caps the page. Use it whenever you
      need a detail from earlier that isn't in your current view.
    - When a compaction notice lands and the dropped span held durable facts (names, decisions,
      preferences), persist them to a memory agent (reflect the tree to find one) via the send
      tool — that is how they survive beyond this window.
    - To inspect your budget + the last compaction + your own last reaction:
      `send('<your_own_id>', {type:'context_status'})`.
    """
