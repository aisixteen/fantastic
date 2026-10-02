// FIND the line anywhere in memory (`grep`).
//
// A reply too large to inline is held in RAM under its pid for this run only and
// the model is handed its digest + origin line; it greps for the line it wants.
// `pull` is gone (python deleted it): after a restart there are no bodies — the
// model re-reads from the origin line. The rows themselves are searched by the
// memory store.
//
// Mirrors python `ai_core/core.py` `_grep` (canonical).

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

private func badArg(_ msg: String, _ arg: String) -> JSON {
    .object(["error": .string(msg), "reason": .string("bad_arg"), "arg": .string(arg)])
}

/// A message id the AI path reads — python `ai_core._as_id`, which is NOT the
/// kernel's `asMessageId`.
///
/// Three differences, all model-visible: a string is STRIPPED first (`' 12 '` is
/// 12), it must be all ASCII digits (so `'٣'` and `'+5'` are nothing), and a
/// digit string of `0` IS an id — `pull pid:'0'` therefore reaches the
/// `not_found` refusal rather than the `bad_arg` one. The kernel's own reader is
/// untouched; this is the AI core's.
func aiAsId(_ v: JSON) -> Int64? {
    switch v {
    case .bool: return nil
    case .integer(let i): return i > 0 ? i : nil
    case .string(let s):
        let t = pyStrip(s)
        guard AIContext.pyDigits(t), let i = try? pyInt(.string(t)) else { return nil }
        return Int64(i)
    default: return nil
    }
}

/// `needle` as a CODE-POINT subsequence of `hay`.
///
/// `String.contains` compares Characters under canonical equivalence: it cannot
/// find `e` inside `é` (one grapheme) and it DOES find text python considers
/// different (NFC vs NFD). A grep answers for what the model can read back, so
/// it scans scalars.
func scalarsContain(_ hay: [Unicode.Scalar], _ needle: [Unicode.Scalar]) -> Bool {
    if needle.isEmpty { return true }
    if needle.count > hay.count { return false }
    for start in 0...(hay.count - needle.count) {
        var ok = true
        for k in 0..<needle.count where hay[start + k] != needle[k] {
            ok = false
            break
        }
        if ok { return true }
    }
    return false
}

extension AIBackend {
    /// args: query:str (req — case-insensitive substring), ids:list[int]? (row
    /// ids / pids / fold ids to search; a fold id searches its members),
    /// limit:int?, width:int? (default 200). Finds the LINE that mentions
    /// something ANYWHERE in your memory — every row you ever wrote or received
    /// (behind every clear, inside every fold), the rows other agents SHARED with
    /// you, and the reply bodies still held in memory this run. Returns
    /// {matches:[{id, line, text}], total, truncated}. Python `_grep`.
    func grepVerb(agent: Agent, payload: JSON, kernel: Kernel) async throws -> JSON {
        let id = agent.id.value
        let rawQ = payload["query"]
        let qText = pyStrip(pyTruthy(rawQ) ? pyStr(rawQ) : "").lowercased()
        if qText.isEmpty { return badArg("grep: `query` is required", "query") }
        let q = Array(qText.unicodeScalars)
        // A value that is not a number is the default.
        let limit = max(1, min((try? pyIntOr(payload["limit"], 100)) ?? 100, 100))
        // The per-line cut. A bare default (200) that the model can raise: a rule
        // written as one long line lost its tail at 200 and the model concluded
        // grep could not reach it (measured 2026-09-08, python fold_long).
        let width = max(20, min((try? pyIntOr(payload["width"], 200)) ?? 200, 20_000))
        var ids: [Int64]? = nil
        switch payload["ids"] {
        case .null:
            break
        case .array(let raw):
            let parsed = raw.map { aiAsId($0) }
            if parsed.contains(where: { $0 == nil }) {
                return badArg("grep: `ids` must be a list of message ids / pids (ints)", "ids")
            }
            ids = parsed.compactMap { $0 }
        default:
            return badArg("grep: `ids` must be a list of message ids / pids (ints)", "ids")
        }

        var matches: [JSON] = []
        var total = 0
        var seenLines: Set<String> = []
        // IDENTICAL LINES ONCE. A task the human repeats per delivery ("find the
        // audit marker…") matched 28 times in one run and pushed the one body hit
        // past the inline cap (measured 2026-09-06, python recall_after_fold). The
        // first occurrence keeps its address.
        func take(_ rid: JSON, _ n: JSON, _ line: String) {
            let key = pySplitWhitespace(line).joined(separator: " ")
            if seenLines.contains(key) { return }
            seenLines.insert(key)
            total += 1
            if matches.count < limit {
                let cut = String(
                    String.UnicodeScalarView(pyStrip(line).unicodeScalars.prefix(width)))
                matches.append(.object(["id": rid, "line": n, "text": .string(cut)]))
            }
        }

        // BODIES FIRST: what a grep is for is the line inside a body never read
        // whole — rows that merely mention the query come after, so the content
        // hit is never the one the inline cap cuts. Bodies live in RAM for this
        // run only.
        let bodies = heldBodies(id, kernel)
        var inReach: Set<Int64>? = nil
        if let ids {
            // A fold id is a DESCENT: its members are reply rows, and their bodies
            // hang off the pids on their tags (measured 2026-09-08: a descent that
            // ignored them found nothing, 22 greps, 22 zeros). An id that is itself
            // a pid counts.
            let rows = try await loadHistory(id, kernel, behind: true)
            let scope = Set(AIFolds.expandIds(rows, ids))
            var reach = Set(ids).union(scope)
            for m in rows {
                guard let mid = rowId(m), scope.contains(mid), rowRole(m) == "user" else { continue }
                for block in replyBlocks(m["content"].asString ?? "") {
                    if let p = pidOf(block).flatMap({ aiAsId(.string($0)) }) { reach.insert(p) }
                }
            }
            inReach = reach
        }
        for (pid, text) in bodies {
            if let inReach, !inReach.contains(pid) { continue }
            // PYTHON'S LINE SET: `splitlines()` breaks on VT, FF, the information
            // separators, NEL and the Unicode line/paragraph separators too.
            for (n, line) in pySplitlines(text).enumerated()
            where scalarsContain(Array(line.lowercased().unicodeScalars), q) {
                take(.integer(pid), .integer(Int64(n + 1)), line)
            }
        }
        // YOUR ROWS — the WHOLE conversation, searched by the store itself.
        var req: OrderedDictionary<String, JSON> = [
            "type": .string("search"), "query": .string(qText), "limit": .integer(1000),
            "width": .integer(Int64(width)),
        ]
        if let ids { req["ids"] = .array(ids.map { .integer($0) }) }
        let res = try await storeCall(id, kernel, req)
        for m in res["matches"].asArray ?? [] {
            take(m["id"], m["line"] == .null ? .integer(0) : m["line"], m["text"].asString ?? "")
        }
        // ROWS SHARED WITH YOU — every fork row's grant, searched under that grant.
        if ids == nil {
            for r in try await loadHead(id, kernel)
            where r["kind"].asString == "fork" && r["grant"] != .null {
                let shared = try await storeCall(
                    id, kernel,
                    [
                        "type": .string("search"), "query": .string(qText), "grant": r["grant"],
                        "limit": .integer(1000), "width": .integer(Int64(width)),
                    ])
                for m in shared["matches"].asArray ?? [] {
                    take(
                        .string("\(pyStr(r["owner"])):\(pyStr(m["id"]))"),
                        m["line"] == .null ? .integer(0) : m["line"], m["text"].asString ?? "")
                }
            }
        }
        let (kept, cut) = AIContext.fitReply(
            matches, capTokens: AIContext.inlineCap(agent, modelWindow: config.modelWindow), limit: limit)
        let more = cut || total > kept.count
        var out: OrderedDictionary<String, JSON> = [
            "matches": .array(kept),
            "total": .integer(Int64(total)),
            "truncated": .bool(more),
        ]
        if more, let last = kept.last {
            out["next"] = .object(["id": last["id"], "line": last["line"]])
        }
        return .object(out)
    }
}
