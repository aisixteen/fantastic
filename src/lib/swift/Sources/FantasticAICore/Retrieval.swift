// FULL ASYNC retrieval: page a stored reply body by pid (`pull`), or FIND the
// line first (`grep`).
//
// These two are what make an unbounded reply non-fatal. A reply too large to
// inline is stored under its pid and the model is handed a digest; it then
// greps for the line it wants and pulls only that. Without them a 240KB ledger
// dump is a turn that cannot fit.
//
// Mirrors python `ai_core/core.py` `_pull` / `_grep` (canonical) and rust
// `retrieval.rs`.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Python's `int(payload.get(k) or dflt)`: a falsy value is the default; an int
/// is itself; a float truncates; a numeric string parses; anything else is the
/// `TypeError`/`ValueError` python raises — `nil` here, for the caller to answer.
func pyIntOr(_ v: JSON, _ dflt: Int) -> Int? {
    if !pythonTruthy(v) { return dflt }
    switch v {
    case .bool: return 1  // the truthy bool is `True`, and `int(True)` is 1
    case .integer(let n): return Int(n)
    case .double(let d): return d.isFinite ? Int(d) : nil
    case .string(let s): return Int(s.trimmingCharacters(in: .whitespacesAndNewlines))
    default: return nil
    }
}

private func badArg(_ msg: String, _ arg: String) -> JSON {
    .object(["error": .string(msg), "reason": .string("bad_arg"), "arg": .string(arg)])
}

/// A row's content as the text a scan reads: the string itself, or the
/// serialized form of anything else. Python `content if isinstance(content, str)
/// else json.dumps(content, default=str)`.
private func contentText(_ m: JSON) -> String {
    m["content"].asString ?? m["content"].serialize()
}

extension AIBackend {
    /// args: pid:str (req — the pid on a settled reply's tag), offset:int? (0),
    /// length:int? (chars; default and max = your inline cap). Reads ONE PAGE of
    /// a reply body that was too large to inline. Returns {pid, offset, text,
    /// next_offset, eof} — page with `offset: next_offset` until `eof`. Python
    /// `_pull`.
    func pullVerb(agent: Agent, payload: JSON, kernel: Kernel) -> JSON {
        guard let pid = asMessageId(payload["pid"]) else {
            return badArg("pull: `pid` (an int) is required — it is on the reply's tag line", "pid")
        }
        let cap = AIContext.pullCapChars(agent)
        guard let rawOffset = pyIntOr(payload["offset"], 0),
            let length = pyIntOr(payload["length"], cap)
        else {
            return .object([
                "error": .string("pull: offset/length must be integers"),
                "reason": .string("bad_arg"),
            ])
        }
        let offset = max(0, rawOffset)
        if length > cap {
            return badArg(
                "pull: length \(length) exceeds the \(cap)-char page cap — a page must "
                    + "fit INLINE where it is going; ask for less and page",
                "length")
        }
        guard
            let (page, nxt, eof, total) = kernel.rows.bodyGet(
                pid, offset: offset, length: max(1, length))
        else {
            // A fold id: its body is on the row itself.
            for r in loadHistory(agent: agent, kernel: kernel) where isFold(r) && rowId(r) == pid {
                let text = Array(pythonTruthy(r["body"]) ? pyStr(r["body"]) : "")
                let n = max(0, length)
                return .object([
                    "pid": .integer(pid),
                    "offset": .integer(0),
                    "text": .string(String(text.prefix(n))),
                    "next_offset": .integer(Int64(min(n, text.count))),
                    "eof": .bool(n >= text.count),
                    "refs": pythonTruthy(r["refs"]) ? r["refs"] : .array([]),
                    "title": r["title"],
                ])
            }
            return .object([
                "error": .string(
                    "pull: no stored body for pid \(pid) — only replies too large "
                        + "to inline are stored; small ones were already in your turn"),
                "reason": .string("not_found"),
            ])
        }
        let capTok = AIContext.inlineCap(agent)
        func reply(_ t: String, _ n: Int, _ e: Bool) -> JSON {
            .object([
                "pid": .integer(pid),
                "offset": .integer(Int64(offset)),
                "text": .string(t),
                "next_offset": .integer(Int64(n)),
                "eof": .bool(e),
                "total": .integer(Int64(total)),
            ])
        }
        // A PAGE FITS INLINE, MEASURED SERIALIZED. Measured 2026-09-06 (python):
        // raw pages sized to the cap in chars came back over it once JSON-escaped
        // (newlines, quotes) and were dehydrated — the model pulled its own pull.
        // Shrink until the reply itself fits.
        var text = page
        var out = reply(text, nxt, eof)
        while text.count > 64
            && AIContext.estimateOne(
                .object(["role": .string("user"), "content": .string(out.serialize())]))
                > capTok
        {
            text = String(text.prefix(Int(Double(text.count) * 0.8)))
            out = reply(text, offset + text.count, false)
        }
        return out
    }

    /// args: query:str (req — case-insensitive substring), ids:list[int]? (row
    /// ids / pids / fold ids to search; default: every row of this conversation
    /// and every stored reply body it received), limit:int? (matches, default:
    /// as many as fit inline), width:int? (chars of each matched line returned,
    /// from its start; default 200). Finds the LINE that mentions something,
    /// wherever it is — a stored body you never pulled, a digest line, an old
    /// turn. Returns {matches:[{id, line, text}], total, truncated}. Python
    /// `_grep`.
    ///
    /// THE SUPER SIMPLE ONE, deliberately. Measured 2026-09-06 (python): the
    /// model spent four ~10k-token thinking turns byte-scanning one 6.8KB body in
    /// 300-char pages to find a single line. "Find the line that mentions X" is
    /// the operation it wanted, and this is it — substring, case-insensitive,
    /// over rows and stored bodies, capped. The advanced machinery (regex,
    /// context lines, ranking) lands on top of this; the shape of the reply does
    /// not change.
    func grepVerb(agent: Agent, payload: JSON, kernel: Kernel) -> JSON {
        let rawQ = payload["query"]
        let q = (pythonTruthy(rawQ) ? pyStr(rawQ) : "")
            .trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        if q.isEmpty { return badArg("grep: `query` is required", "query") }
        // A value that is not a number is the default (python answers the same
        // clamp for `0`/absent; on a non-numeric it falls to `limit = 100`).
        let limit = max(1, min(pyIntOr(payload["limit"], 100) ?? 100, 100))
        // The per-line cut. A bare default (200) that the model can raise: a rule
        // written as one long line lost its tail at 200 and the model concluded
        // grep could not reach it (measured 2026-09-08, python fold_long).
        let width = max(20, min(pyIntOr(payload["width"], 200) ?? 200, 20_000))
        let rows = loadHistory(agent: agent, kernel: kernel)
        // A fold id in `ids` is a DESCENT: it expands to the rows it collapsed.
        var scope: Set<Int64>? = nil
        switch payload["ids"] {
        case .null:
            break
        case .array(let raw):
            let parsed = raw.map { asMessageId($0) }
            if parsed.contains(where: { $0 == nil }) {
                return badArg("grep: `ids` must be a list of message ids / pids (ints)", "ids")
            }
            let ids = parsed.compactMap { $0 }
            // `if ids:` — an EMPTY list is no scope at all, python's truthiness kept.
            if !ids.isEmpty { scope = Set(AIFolds.expandIds(rows, ids)) }
        default:
            return badArg("grep: `ids` must be a list of message ids / pids (ints)", "ids")
        }

        var matches: [JSON] = []
        var total = 0
        var seenLines: Set<String> = []
        // IDENTICAL LINES ONCE. A task the human repeats per delivery ("find the
        // audit marker…") matched 28 times in one run and pushed the one body hit
        // past the inline cap (measured 2026-09-06, python recall_after_fold: grep
        // "audit marker" → total 28, the marker line cut). The first occurrence
        // keeps its address.
        func scan(_ rid: Int64, _ text: String) {
            var n = 0
            for line in text.split(
                omittingEmptySubsequences: false, whereSeparator: { $0.isNewline })
            {
                n += 1
                guard line.lowercased().contains(q) else { continue }
                let key = line.split(whereSeparator: { $0.isWhitespace }).joined(separator: " ")
                if seenLines.contains(key) { continue }
                seenLines.insert(key)
                total += 1
                if matches.count < limit {
                    let cut = String(
                        line.trimmingCharacters(in: .whitespacesAndNewlines).prefix(width))
                    matches.append(
                        .object([
                            "id": .integer(rid), "line": .integer(Int64(n)), "text": .string(cut),
                        ]))
                }
            }
        }

        // The bodies in reach: the pids on the reply tags of the rows considered —
        // every row of the conversation, or, under a scope, the rows the scope
        // names (a fold's members ARE reply rows, and their bodies hang off the
        // pids in their tags — a descent that ignored them found nothing, measured
        // 2026-09-08, python: 22 greps, 22 zeros). An id that is itself a pid (a
        // call id) counts too.
        var pids: [Int64] = []
        for m in rows {
            if let s = scope, !(rowId(m).map { s.contains($0) } ?? false) { continue }
            guard rowRole(m) == "user" else { continue }
            for block in replyBlocks(contentText(m)) {
                if let p = pidOf(block).flatMap({ asMessageId(.string($0)) }) { pids.append(p) }
            }
        }
        if let s = scope {
            var seen: Set<Int64> = []
            pids = (s.sorted() + pids).filter { seen.insert($0).inserted }
        }
        // STORED BODIES FIRST: what a grep is for is the line inside a body never
        // pulled — rows that merely mention the query (the task, the model's own
        // plan) come after, so the content hit is never the one the inline cap
        // cuts. A pid with a stored body is greppable without pulling it: those
        // this conversation received (pids on its reply tags), or the ids the
        // caller named.
        for pid in pids {
            if let (text, _, _, _) = kernel.rows.bodyGet(pid, offset: 0, length: 4 * 1024 * 1024) {
                scan(pid, text)
            }
        }
        for m in rows {
            guard let rid = rowId(m) else { continue }
            if let s = scope, !s.contains(rid) { continue }
            scan(rid, contentText(m))
        }
        let (kept, cut) = AIContext.fitReply(
            matches, capTokens: AIContext.inlineCap(agent), limit: limit)
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
