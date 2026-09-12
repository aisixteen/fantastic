// THE VIEW ARRAY — the agent's STATE: the ordered ids it sees, each at a
// resolution. Kernel-held, written by every projection and by `context_recall`,
// so the next turn (and the next PROCESS) rebuilds the same bytes rather than a
// freshly-guessed prefix.
//
// Python `ai_core/core.py` `_save_view` / `_load_view` / `_res_of` /
// `_apply_res`; rust `view.rs`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// APPEND rows to the kernel row store on behalf of `agent`. Bookkeeping from
/// above — the same standing as the ledger's own put; no grant, no gate, no
/// disk. A pre-minted `id` is accepted, which is what lets a fold, a recall row
/// and a turn name themselves before their row exists. Python `_save_history`.
func appendRows(_ agent: AgentId, kernel: Kernel, _ rows: [JSON]) {
    for r in rows {
        let row: Row = r.asObject ?? [:]
        do { try kernel.rows.put(agent, row) } catch {
            // A refused row is the hydrator's mistake, not the model's — say it and
            // keep the rest, rather than dropping a conversation.
            FileHandle.standardError.write(Data("  [history] \(agent.value): \(error)\n".utf8))
        }
    }
}

/// The agent's STATE: the ordered ids it sees, each at a resolution.
/// Kernel-held. Python `_save_view` — the doc is `{v, ts, budget, why, ids}`.
func saveView(_ agent: AgentId, kernel: Kernel, ids: [JSON], budget: Int64, why: String) {
    let doc: Row = [
        "v": .integer(1),
        "ts": .double(Date().timeIntervalSince1970),
        "budget": .integer(budget),
        "why": .string(why),
        "ids": .array(ids),
    ]
    do { try kernel.rows.viewPut(agent, doc) } catch {
        FileHandle.standardError.write(Data("  [view] \(agent.value): \(error)\n".utf8))
    }
}

/// The current view doc, when one has been written and carries an `ids` list.
/// Python `_load_view`.
func loadView(_ agent: AgentId, kernel: Kernel) -> Row? {
    guard let doc = kernel.rows.viewGet(agent), doc["ids"]?.asArray != nil else { return nil }
    return doc
}

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
