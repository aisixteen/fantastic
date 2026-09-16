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

private func badArg(_ msg: String, _ arg: String) -> JSON {
    .object(["error": .string(msg), "reason": .string("bad_arg"), "arg": .string(arg)])
}

/// A message id the AI path reads — python `ai_core._as_id`, which is NOT the
/// kernel's `asMessageId`.
///
/// Three differences, all model-visible: a string is STRIPPED first (`' 12 '` is
/// 12), it must be all python DIGITS (so `'٣'` is 3 and `'+5'` is nothing), and a
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

/// Python's `text[:length]` for a scalar array — a NEGATIVE length counts from
/// the end (`abcdefghij`[: -3] is `abcdefg`), where a clamp to 0 returned
/// nothing.
private func pySlicePrefix(_ a: [Unicode.Scalar], _ length: Int) -> [Unicode.Scalar] {
    if length >= 0 { return Array(a.prefix(length)) }
    let end = a.count + length
    return end <= 0 ? [] : Array(a.prefix(end))
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

/// `list(set(ids))` for CPython 3.11 ints — the SLOT order of the hash table,
/// which is what a grep scope is scanned in.
///
/// Not sorted, and not insertion order: `list(set([1116,1065,1066,1080]))` is
/// `[1080, 1065, 1066, 1116]`, and that order decides which row owns a line two
/// bodies share (the first scan of a duplicate line keeps its address). Table of
/// 8 slots, `i = hash & mask`, ten linear probes then the perturbed jump
/// `i = i*5 + 1 + perturb` with `perturb >>= 5`, resized to the smallest power of
/// two above `used*4` once `fill*5 >= mask*3`.
func pySetOrder(_ ids: [Int64]) -> [Int64] {
    var mask = 7
    var table = [Int64?](repeating: nil, count: mask + 1)
    var fill = 0

    func insertClean(_ t: inout [Int64?], _ m: Int, _ key: Int64) {
        var i = Int(UInt64(bitPattern: key) & UInt64(m))
        var perturb = UInt64(bitPattern: key)
        while true {
            if t[i] == nil {
                t[i] = key
                return
            }
            if i + 9 <= m {
                var j = i
                for _ in 0..<9 {
                    j += 1
                    if t[j] == nil {
                        t[j] = key
                        return
                    }
                }
            }
            perturb >>= 5
            i = Int((UInt64(i) &* 5 &+ 1 &+ perturb) & UInt64(m))
        }
    }

    for key in ids {
        var i = Int(UInt64(bitPattern: key) & UInt64(mask))
        var perturb = UInt64(bitPattern: key)
        var added = false
        probe: while true {
            var idx = i
            var remaining = (i + 9 <= mask) ? 9 : 0
            while true {
                if table[idx] == nil {
                    table[idx] = key
                    fill += 1
                    added = true
                    break probe
                }
                if table[idx] == key { break probe }
                idx += 1
                if remaining == 0 { break }
                remaining -= 1
            }
            perturb >>= 5
            i = Int((UInt64(i) &* 5 &+ 1 &+ perturb) & UInt64(mask))
        }
        guard added, fill * 5 >= mask * 3 else { continue }
        var newSize = 8
        let minUsed = fill > 50000 ? fill * 2 : fill * 4
        while newSize <= minUsed { newSize <<= 1 }
        var grown = [Int64?](repeating: nil, count: newSize)
        for slot in table where slot != nil { insertClean(&grown, newSize - 1, slot!) }
        table = grown
        mask = newSize - 1
    }
    return table.compactMap { $0 }
}

/// A row's content as the text a scan reads: the string itself, or the
/// serialized form of anything else. Python `content if isinstance(content, str)
/// else json.dumps(content, default=str)`.
private func contentText(_ m: JSON) -> String {
    // PYTHON'S DUMP, not compact raw JSON: `json.dumps(content, default=str)`
    // escapes every non-ASCII scalar, so a grep for `é` over a dict row matches
    // NOTHING and a grep for `u00e9` matches. The model greps what it was shown.
    m["content"].asString ?? pyJsonDumps(m["content"])
}

extension AIBackend {
    /// args: pid:str (req — the pid on a settled reply's tag), offset:int? (0),
    /// length:int? (chars; default and max = your inline cap). Reads ONE PAGE of
    /// a reply body that was too large to inline. Returns {pid, offset, text,
    /// next_offset, eof} — page with `offset: next_offset` until `eof`. Python
    /// `_pull`.
    func pullVerb(agent: Agent, payload: JSON, kernel: Kernel) -> JSON {
        guard let pid = aiAsId(payload["pid"]) else {
            return badArg("pull: `pid` (an int) is required — it is on the reply's tag line", "pid")
        }
        let cap = AIContext.pullCapChars(agent)
        guard let rawOffset = try? pyIntOr(payload["offset"], 0),
            let length = try? pyIntOr(payload["length"], cap)
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
                // UNCLAMPED AND ON CODE POINTS, as python's slice is: a negative
                // `length` counts from the END of the body and `next_offset`
                // carries that negative number back, where a `max(0, …)` answered
                // an empty page. `str(r.get("body") or "")` is the text.
                let text = Array((pyTruthy(r["body"]) ? pyStr(r["body"]) : "").unicodeScalars)
                let page = pySlicePrefix(text, length)
                return .object([
                    "pid": .integer(pid),
                    "offset": .integer(0),
                    "text": .string(String(String.UnicodeScalarView(page))),
                    "next_offset": .integer(Int64(min(length, text.count))),
                    "eof": .bool(length >= text.count),
                    "refs": pyTruthy(r["refs"]) ? r["refs"] : .array([]),
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
        // CODE POINTS, not graphemes: `len(text)` and `text[:n]` are python's, so
        // a CRLF body shrinks and pages by the same arithmetic on both runtimes.
        var scalars = Array(page.unicodeScalars)
        var out = reply(page, nxt, eof)
        while scalars.count > 64
            && AIContext.estimateOne(
                .object(["role": .string("user"), "content": .string(pyJsonDumps(out))]))
                > capTok
        {
            scalars = Array(scalars.prefix(Int(Double(scalars.count) * 0.8)))
            out = reply(
                String(String.UnicodeScalarView(scalars)), offset + scalars.count, false)
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
        let qText = pyStrip(pyTruthy(rawQ) ? pyStr(rawQ) : "").lowercased()
        if qText.isEmpty { return badArg("grep: `query` is required", "query") }
        let q = Array(qText.unicodeScalars)
        // A value that is not a number is the default (python answers the same
        // clamp for `0`/absent; on a non-numeric it falls to `limit = 100`).
        let limit = max(1, min((try? pyIntOr(payload["limit"], 100)) ?? 100, 100))
        // The per-line cut. A bare default (200) that the model can raise: a rule
        // written as one long line lost its tail at 200 and the model concluded
        // grep could not reach it (measured 2026-09-08, python fold_long).
        let width = max(20, min((try? pyIntOr(payload["width"], 200)) ?? 200, 20_000))
        let rows = loadHistory(agent: agent, kernel: kernel)
        // A fold id in `ids` is a DESCENT: it expands to the rows it collapsed.
        var scope: Set<Int64>? = nil
        // `list(scope)` is a python SET ordering, and it decides which row owns a
        // line two bodies share — so the order is carried, not re-derived.
        var scopeOrder: [Int64]? = nil
        switch payload["ids"] {
        case .null:
            break
        case .array(let raw):
            let parsed = raw.map { aiAsId($0) }
            if parsed.contains(where: { $0 == nil }) {
                return badArg("grep: `ids` must be a list of message ids / pids (ints)", "ids")
            }
            let ids = parsed.compactMap { $0 }
            // `if ids:` — an EMPTY list is no scope at all, python's truthiness kept.
            if !ids.isEmpty {
                scopeOrder = pySetOrder(AIFolds.expandIds(rows, ids))
                scope = Set(scopeOrder!)
            }
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
            // PYTHON'S LINE SET: `splitlines()` breaks on VT, FF, the information
            // separators U+001C..U+001E, NEL and the Unicode line/paragraph
            // separators too — swift's `isNewline` misses the separators and a
            // single `lines()` sees one line where python sees four.
            for line in pySplitlines(text) {
                n += 1
                guard scalarsContain(Array(line.lowercased().unicodeScalars), q) else { continue }
                let key = pySplitWhitespace(line).joined(separator: " ")
                if seenLines.contains(key) { continue }
                seenLines.insert(key)
                total += 1
                if matches.count < limit {
                    let cut = String(
                        String.UnicodeScalarView(pyStrip(line).unicodeScalars.prefix(width)))
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
                if let p = pidOf(block).flatMap({ aiAsId(.string($0)) }) { pids.append(p) }
            }
        }
        if let order = scopeOrder {
            var seen: Set<Int64> = []
            pids = (order + pids).filter { seen.insert($0).inserted }
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
