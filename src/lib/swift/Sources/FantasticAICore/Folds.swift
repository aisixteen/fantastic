// FOLDS — a compaction MARK, not a deletion.
//
// A fold is an ordinary appended row that names the rows it collapses (`refs`),
// carries a title (what the model reads every turn), a body (a short summary,
// pulled if it wants more), and stays one descent from its members: `grep`/`pull`
// on a fold id expand to its refs. Nothing is rewritten, nothing is lost, and the
// history becomes a tree of marks the model walks down as far as it needs.
//
// CHAINS ARE DERIVED, NEVER STORED. A chain is a root row plus everything
// reachable from it through `parent`. Measured 2026-09-06 (python): the model
// attributed rows 5/5 through 16 interleaved chains from `parent` alone, so the
// agent carries no thread concept — this module walks the links when a projection
// needs to know what is closed.
//
// Python `ai_core/folds.py` is the immutable reference; rust `folds.rs` the
// structural guide.

import FantasticJSON
import Foundation
import OrderedCollections

/// The opener a fold row's rendered content carries. Python `FOLD_TAG`.
let FOLD_TAG = "[fold "

/// Chars handed to the summarizer per fold — never a 60k reply. Python
/// `SUMMARY_INPUT_CAP`.
let SUMMARY_INPUT_CAP = 24_000

/// A row's `id`, when it has one.
func rowId(_ row: JSON) -> Int64? { row["id"].asInt }

/// A row's `parent`, when it has one.
func rowParent(_ row: JSON) -> Int64? { row["parent"].asInt }

/// A row's `content` as text — python's `str(r.get("content", ""))`: a string
/// as itself, anything else serialized, absence the empty string.
func rowContent(_ row: JSON) -> String {
    switch row.asObject?["content"] {
    case nil: return ""
    case .some(.string(let s)): return s
    case .some(.null): return "None"
    case .some(let v): return v.serialize()
    }
}

/// A row's `role`.
func rowRole(_ row: JSON) -> String { row["role"].asString ?? "" }

/// Is this row a fold MARK (rather than a turn)? Python `is_fold`.
func isFold(_ row: JSON) -> Bool { row["kind"].asString == "fold" }

/// The `refs` a fold names.
func refsOf(_ row: JSON) -> [Int64] {
    (row["refs"].asArray ?? []).compactMap { $0.asInt }
}

/// A row with one field replaced — python's `{**row, key: value}`.
func withField(_ row: JSON, _ key: String, _ value: JSON) -> JSON {
    var obj = row.asObject ?? [:]
    obj[key] = value
    return .object(obj)
}

enum AIFolds {
    /// Row indices grouped by root, in root arrival order. A row whose parent is
    /// unknown (absent, or pointing outside `rows`) is its own root. Python
    /// `chains`.
    static func chains(_ rows: [JSON]) -> [[Int]] {
        // python's dict comprehension keeps the LAST index for a duplicated id
        var byId: [Int64: Int] = [:]
        for (i, r) in rows.enumerated() {
            if let id = rowId(r), id != 0 { byId[id] = i }
        }
        var rootOf: [Int: Int] = [:]
        func root(_ i: Int) -> Int {
            if let r = rootOf[i] { return r }
            var seen = [i]
            var cur = i
            while true {
                guard let par = rowParent(rows[cur]), par != 0, let j = byId[par], !seen.contains(j)
                else { break }
                seen.append(j)
                cur = j
            }
            for k in seen { rootOf[k] = cur }
            return cur
        }
        var groups: [Int: [Int]] = [:]
        for i in rows.indices {
            groups[root(i), default: []].append(i)
        }
        return groups.keys.sorted().map { groups[$0]! }
    }

    /// A chain is OPEN while its last row is an assistant turn that made a `send`
    /// — a reply is still owed. A cancelled or failed call settles as a reply row,
    /// so a cancel closes a chain the same way an answer does. Python `is_open`.
    static func isOpen(_ rows: [JSON], _ chain: [Int]) -> Bool {
        guard let lastI = chain.last else { return false }
        let last = rows[lastI]
        if rowRole(last) != "assistant" { return false }
        return extractToolCalls(rowContent(last)).contains { $0.name == "send" }
    }

    /// Every id named by any fold's refs. Python `covered`.
    static func covered(_ rows: [JSON]) -> Set<Int64> {
        var out: Set<Int64> = []
        for r in rows where isFold(r) {
            out.formUnion(refsOf(r))
        }
        return out
    }

    /// A fold title lives inside the tag; it may not close it or quote itself.
    /// Python `clean_title`: `" ".join(str(t).replace("]", ")").replace('"', "'").split())[:140] or "(untitled)"`.
    static func cleanTitle(_ t: String) -> String {
        let squashed = t.replacingOccurrences(of: "]", with: ")")
            .replacingOccurrences(of: "\"", with: "'")
            .split(whereSeparator: { $0.isWhitespace })
            .joined(separator: " ")
        let cut = String(squashed.unicodeScalars.prefix(140))
        return cut.isEmpty ? "(untitled)" : cut
    }

    /// A fold row. Its id is MINTED BY THE STORE (`kernel.rows.mintId()`) and
    /// passed in — a fold is a message like any other, never self-named. Python
    /// `make_fold`.
    static func makeFold(
        refs: [Int64], title: String, body: String, degraded: Bool,
        id: Int64, lvl: Int64 = 1, parent: Int64? = nil
    ) -> JSON {
        let title = cleanTitle(title)
        let head = "\(FOLD_TAG)\(id) · \(refs.count) refs · L\(lvl)] \"\(title)\""
        let note = degraded ? "  (summary unavailable — refs and titles only)" : ""
        return .object([
            "role": .string("user"),
            "id": .integer(id),
            // A fold sits where its members sat: its parent is its first member's
            // parent, so it stays inside the chain it collapsed instead of becoming
            // a root.
            "parent": parent.map(JSON.integer) ?? .null,
            "kind": .string("fold"),
            "refs": .array(refs.map(JSON.integer)),
            "title": .string(title),
            "body": .string(body),
            "level": .integer(lvl),  // 1 = rows, 2 = a fold among its members, …
            "content": .string("\(head)\(note)\n\(body)"),
        ])
    }

    /// The view with folded rows hidden and each fold sitting where its first ref
    /// was. Deterministic in `rows`, so a view renders the same bytes until a new
    /// fold lands. Python `render`.
    static func render(_ rows: [JSON]) -> [JSON] {
        let cov = covered(rows)
        var firstPos: [Int64: Int] = [:]
        for (i, r) in rows.enumerated() {
            if let rid = rowId(r), cov.contains(rid), firstPos[rid] == nil { firstPos[rid] = i }
        }
        var place: [Int: [JSON]] = [:]
        for r in rows where isFold(r) {
            let at = refsOf(r).compactMap { firstPos[$0] }.min()
            if let at, !(rowId(r).map { cov.contains($0) } ?? false) {
                place[at, default: []].append(r)
            }
        }
        var out: [JSON] = []
        for (i, r) in rows.enumerated() {
            for f in place[i] ?? [] { out.append(f) }
            if (rowId(r).map { cov.contains($0) } ?? false) || isFold(r) {
                continue  // hidden, or a fold rendered at its first ref (or itself folded)
            }
            out.append(r)
        }
        // a fold whose refs are all absent from the view (already hidden by another
        // fold) is itself covered or unplaceable; nothing to do — it stays out of
        // the view.
        return out
    }

    /// Replace fold ids with their refs, recursively — the descent path. Python
    /// `expand_ids`.
    static func expandIds(_ rows: [JSON], _ ids: [Int64]) -> [Int64] {
        var folds: [Int64: [Int64]] = [:]
        for r in rows where isFold(r) {
            if let id = rowId(r) { folds[id] = refsOf(r) }
        }
        var out: [Int64] = []
        var stack = ids
        var seen: Set<Int64> = []
        while !stack.isEmpty {
            let x = stack.removeFirst()
            if seen.contains(x) { continue }
            seen.insert(x)
            if let refs = folds[x] {
                stack = refs + stack
            } else {
                out.append(x)
            }
        }
        return out
    }

    /// The foldable PREFIX of an open chain: everything but the last `keepTail`
    /// rows, cut back so it ends on a reply (never between a call and its
    /// answer). A superlong chain never closes, so this is how it still folds.
    /// Python `closed_prefix`.
    static func closedPrefix(_ rows: [JSON], _ chain: [Int], keepTail: Int) -> [Int] {
        if chain.count <= keepTail + 1 { return [] }
        var span = Array(chain[..<(chain.count - keepTail)])
        while let last = span.last, rowRole(rows[last]) == "assistant" {
            span.removeLast()
        }
        return span.count >= 2 ? span : []
    }

    /// Fraction of the view's bytes that are fold rows — the trigger for folding
    /// folds. Python `fold_share`.
    static func foldShare(_ view: [JSON]) -> Double {
        var total = view.reduce(0) { $0 + pyLen(rowContent($1)) }
        if total == 0 { total = 1 }
        let folded = view.filter(isFold).reduce(0) { $0 + pyLen(rowContent($1)) }
        return Double(folded) / Double(total)
    }

    /// 1 for a fold of rows, 2 for a fold of folds, and so on. Python `level`.
    static func level(_ rows: [JSON], _ foldRow: JSON) -> Int64 {
        var byId: [Int64: JSON] = [:]
        for r in rows {
            if let id = rowId(r), id != 0 { byId[id] = r }
        }
        var best: Int64 = 0
        for ref in refsOf(foldRow) {
            if let r = byId[ref], isFold(r) {
                best = max(best, level(rows, r))
            }
        }
        return best + 1
    }

    /// What the summarizer reads. CAPPED per row and in total: a chain with big
    /// inline rows would otherwise make the fold call itself the reply that does
    /// not fit. Python `chain_text`.
    static func chainText(_ rows: [JSON], _ chain: [Int]) -> String {
        let per = max(400, SUMMARY_INPUT_CAP / max(1, chain.count))
        var items: [JSON] = []
        for i in chain {
            var c = rowContent(rows[i])
            let scalars = Array(c.unicodeScalars)
            if scalars.count > per {
                // python: `c[: per // 2] + " …[N chars elided]… " + c[-per // 2 :]` —
                // `-per // 2` floors, so the tail is ceil(per/2) scalars.
                let headN = per / 2
                let tailN = (per + 1) / 2
                var s = String.UnicodeScalarView()
                s.append(contentsOf: scalars[..<headN])
                let head = String(s)
                var t = String.UnicodeScalarView()
                t.append(contentsOf: scalars[(scalars.count - tailN)...])
                c = head + " …[\(scalars.count - per) chars elided]… " + String(t)
            }
            items.append(
                .object([
                    "role": rows[i]["role"], "id": rows[i]["id"], "content": .string(c),
                ]))
        }
        let s = JSON.array(items).serialize()
        return String(s.unicodeScalars.prefix(SUMMARY_INPUT_CAP))
    }

    /// The `· "<title>"` a rendered row carries, if any — python's
    /// `re.search(r'· "([^"]{1,140})"', content)`. A hand scan, like rust's.
    static func quotedTitle(_ content: String) -> String? {
        var from = content.startIndex
        while let r = content.range(of: "· \"", range: from..<content.endIndex) {
            let start = r.upperBound
            if let close = content[start...].firstIndex(of: "\"") {
                let n = content[start..<close].unicodeScalars.count
                if (1...140).contains(n) { return String(content[start..<close]) }
            }
            from = start
        }
        return nil
    }
}
