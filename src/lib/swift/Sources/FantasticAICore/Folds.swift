// FOLDS — a compaction MARK, not a deletion.
//
// A fold is an ordinary appended row that names the rows it collapses (`refs`),
// carries a title (what the model reads every turn), a body (a short summary,
// pulled if it wants more), and stays one descent from its members: `grep`/`pull`
// on a fold id expand to its refs. Nothing is rewritten, nothing is lost, and the
// history becomes a tree of marks the model walks down as far as it needs.
//
// CHAINS ARE DERIVED, NEVER STORED. A chain is a root row plus everything
// reachable from it through `parent`. A model attributes rows to
// interleaved chains from `parent` alone, so the
// agent carries no thread concept — this module walks the links when a projection
// needs to know what is closed.
//
// A FOLD CARRIES ITS OWN PLACE. Its `anchor` is the smallest LEAF id it covers (a
// leaf is a non-fold row; a member fold contributes its own anchor). Row ids come
// from the kernel's one counter, so an anchor orders a fold in time without any of
// its members present: the view array names folds without the rows they hide, and
// `context_forget` prunes those rows from the store. A fold written before folds
// carried the field is anchored by the `anchors` row `context_forget` recorded for
// it, or, while its members are still stored, by deriving the same value from its
// refs.
//
// Python `ai_core/folds.py` is the immutable reference; rust `folds.rs` the
// structural guide.

import FantasticJSON
import Foundation
import OrderedCollections

/// The opener a fold row's rendered content carries. Python `FOLD_TAG`.
let FOLD_TAG = "[fold "

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
    /// passed in — a fold is a message like any other, never self-named. `anchor`
    /// is the smallest leaf id it covers (`memberAnchor` over its members). Python
    /// `make_fold`.
    static func makeFold(
        refs: [Int64], title: String, body: String, degraded: Bool,
        id: Int64, anchor: Int64, lvl: Int64 = 1, parent: Int64? = nil
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
            "anchor": .integer(anchor),
            "content": .string("\(head)\(note)\n\(body)"),
        ])
    }

    /// An int, as python's `_int`: an integer (never a bool), or a string of
    /// decimal digits.
    static func asAnchorInt(_ x: JSON) -> Int64? {
        switch x {
        case .integer(let v): return v
        case .string(let s):
            guard !s.isEmpty, s.unicodeScalars.allSatisfy({ $0.value >= 48 && $0.value <= 57 }) else {
                return nil
            }
            return Int64(s)
        default: return nil
        }
    }

    /// The anchors `context_forget` recorded: every `anchors` row in `rows`, in
    /// order, a later row winning per fold id. Python `recorded_anchors`.
    static func recordedAnchors(_ rows: [JSON]) -> [Int64: Int64] {
        var out: [Int64: Int64] = [:]
        for r in rows where r["kind"].asString == "anchors" {
            guard let obj = r["anchors"].asObject else { continue }
            for (k, v) in obj {
                if let fid = asAnchorInt(.string(k)), let a = asAnchorInt(v) { out[fid] = a }
            }
        }
        return out
    }

    /// Where `fold` sits: its own `anchor` field; else the anchor `context_forget`
    /// recorded for it (`recorded`, from `recordedAnchors`); else the smallest leaf
    /// id among its refs present in `rows`, through member folds recursively. nil
    /// when none of the three is known — such a fold is counted, never placed by
    /// guessing. Python `anchor_of`.
    static func anchorOf(_ fold: JSON, _ rows: [JSON], _ recorded: [Int64: Int64]) -> Int64? {
        if let own = asAnchorInt(fold["anchor"]) { return own }
        if let fid = rowId(fold), let a = recorded[fid] { return a }
        return derive(fold, byIdOf(rows), recorded, [])
    }

    private static func byIdOf(_ rows: [JSON]) -> [Int64: JSON] {
        var byId: [Int64: JSON] = [:]
        for r in rows {
            if let id = rowId(r) { byId[id] = r }
        }
        return byId
    }

    private static func derive(
        _ fold: JSON, _ byId: [Int64: JSON], _ recorded: [Int64: Int64], _ seenIn: Set<Int64>
    ) -> Int64? {
        let fid = rowId(fold)
        if let fid, seenIn.contains(fid) { return nil }
        var seen = seenIn
        if let fid { seen.insert(fid) }
        var best: Int64? = nil
        for ref in refsOf(fold) {
            guard let r = byId[ref] else { continue }
            var a: Int64?
            if isFold(r) {
                a = asAnchorInt(r["anchor"]) ?? recorded[ref] ?? derive(r, byId, recorded, seen)
            } else {
                a = ref
            }
            if let a, best == nil || a < best! { best = a }
        }
        return best
    }

    /// fold id → anchor for every fold in `rows` whose anchor is known. `rows` are
    /// the agent's HEAD rows (`anchors` rows included), never only the body being
    /// rendered. Python `anchors_of`.
    static func anchorsOf(_ rows: [JSON]) -> [Int64: Int64] {
        let recorded = recordedAnchors(rows)
        let byId = byIdOf(rows)
        var out: [Int64: Int64] = [:]
        for r in rows where isFold(r) {
            guard let id = rowId(r) else { continue }
            let a = asAnchorInt(r["anchor"]) ?? recorded[id] ?? derive(r, byId, recorded, [])
            if let a { out[id] = a }
        }
        return out
    }

    /// The anchor a new fold over `rows[i] for i in idxs` carries: the smallest
    /// leaf id, a member fold contributing its anchor. A member fold with no known
    /// anchor cannot be in the view, so it cannot be a member: that is a
    /// programming error. Python `member_anchor`.
    static func memberAnchor(_ rows: [JSON], _ idxs: [Int], _ anchors: [Int64: Int64]) throws -> Int64 {
        var best: Int64? = nil
        for i in idxs {
            let r = rows[i]
            let a: Int64
            if isFold(r) {
                guard let x = asAnchorInt(r["anchor"]) ?? rowId(r).flatMap({ anchors[$0] }) else {
                    throw PyError("ValueError", "fold: member fold \(pyStr(r["id"])) has no anchor")
                }
                a = x
            } else {
                guard let x = asAnchorInt(r["id"]) else {
                    throw PyError("ValueError", "fold: member row \(pyRepr(r["id"])) has no int id")
                }
                a = x
            }
            if best == nil || a < best! { best = a }
        }
        guard let best else { throw PyError("ValueError", "fold: a fold needs at least one member") }
        return best
    }

    /// (row index → [(anchor, fold)], unanchored). Each fold not covered by
    /// another fold in `rows` sits immediately before the first non-fold row whose
    /// id is at or past its anchor, or at the end when there is none. Python
    /// `_placed`.
    private static func placed(
        _ rows: [JSON], _ anchors: [Int64: Int64]
    ) -> (place: [Int: [(Int64, JSON)]], unanchored: Int) {
        let cov = covered(rows)
        // firstAt(a) = the smallest row index whose id is >= a: ids sorted
        // ascending, each paired with the smallest index among the ids at or above it.
        var eligible: [(Int64, Int)] = []
        for (i, x) in rows.enumerated() where !isFold(x) {
            if let id = asAnchorInt(x["id"]) { eligible.append((id, i)) }
        }
        eligible.sort { $0.0 != $1.0 ? $0.0 < $1.0 : $0.1 < $1.1 }
        var least = [Int](repeating: rows.count, count: eligible.count + 1)
        var j = eligible.count - 1
        while j >= 0 {
            least[j] = min(eligible[j].1, least[j + 1])
            j -= 1
        }
        var place: [Int: [(Int64, JSON)]] = [:]
        var unanchored = 0
        for r in rows where isFold(r) {
            if let id = rowId(r), cov.contains(id) { continue }
            guard let a = asAnchorInt(r["anchor"]) ?? rowId(r).flatMap({ anchors[$0] }) else {
                unanchored += 1
                continue
            }
            // bisect_left over the sorted ids
            var lo = 0
            var hi = eligible.count
            while lo < hi {
                let mid = (lo + hi) / 2
                if eligible[mid].0 < a { lo = mid + 1 } else { hi = mid }
            }
            place[least[lo], default: []].append((a, r))
        }
        return (place, unanchored)
    }

    /// Folds at one slot, by anchor — a stable sort, like python's `sorted`.
    private static func byAnchor(_ xs: [(Int64, JSON)]) -> [JSON] {
        xs.enumerated().sorted { $0.element.0 != $1.element.0 ? $0.element.0 < $1.element.0 : $0.offset < $1.offset }
            .map { $0.element.1 }
    }

    /// The view with folded rows hidden and each fold sitting at its anchor
    /// (`anchors` from `anchorsOf` over the HEAD — the members need not be in
    /// `rows`). A fold covered by another fold in `rows` stays hidden; a fold whose
    /// anchor is unknown is left out (`unanchored` counts it). Deterministic in
    /// `rows`, so a view renders the same bytes until a new fold lands. Python
    /// `render`.
    static func render(_ rows: [JSON], _ anchors: [Int64: Int64]) -> [JSON] {
        let cov = covered(rows)
        let place = placed(rows, anchors).place
        var out: [JSON] = []
        for (i, r) in rows.enumerated() {
            out.append(contentsOf: byAnchor(place[i] ?? []))
            if (rowId(r).map { cov.contains($0) } ?? false) || isFold(r) {
                continue  // hidden, or a fold rendered at its anchor (or itself folded)
            }
            out.append(r)
        }
        out.append(contentsOf: byAnchor(place[rows.count] ?? []))
        return out
    }

    /// How many folds `render` leaves out because no anchor is known for them.
    /// Python `unanchored`.
    static func unanchored(_ rows: [JSON], _ anchors: [Int64: Int64]) -> Int {
        placed(rows, anchors).unanchored
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
        let per = max(400, AIConstants.SUMMARY_INPUT_CAP / max(1, chain.count))
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
        // `json.dumps(items, ensure_ascii=False, default=str)` — PYTHON
        // SEPARATORS (`', '` / `': '`), raw non-ASCII, cut at the cap in CODE
        // POINTS. A compact dump is a different byte count, which is a different
        // cut and therefore a different summarizer input.
        let s = pyJsonDumps(.array(items), ensureAscii: false)
        return String(String.UnicodeScalarView(s.unicodeScalars.prefix(AIConstants.SUMMARY_INPUT_CAP)))
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
