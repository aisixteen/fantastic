// ram_store — a MEMORY STORE agent held in RAM.
//
// Port of python `bundled_agents/memory/ram_store/src/ram_store/tools.py`, which
// is canonical.
//
// A memory store keeps conversation rows for MANY owners (multi-tenant). Every
// row is keyed by `(owner, id)`; the owner is never a field a caller writes — it
// is WHO CALLED: `<kernel guid>|<caller agent id>` (a call a bridge leg serves
// is the leg's own), and `<guid>|kernel` for the operator (`sender=None`). So a
// caller writes ONLY under its own key, by construction.
//
// THE TOPOLOGY IS SQLITE'S. The tables below are the ones a durable
// `sqlite_store` keeps, with the same keys, the same uniqueness, the same indexes
// and the same foreign-key cascades; only the query language is missing:
//
//   rows       PRIMARY KEY (owner, id)            the row dict, minus `refs`
//              INDEX (owner, id) ordered           paging, `from_last_kind`
//   fold_refs  (owner, fold_id, member_id)         a fold's members, in order
//              FK (owner, fold_id)   → rows ON DELETE CASCADE
//              FK (owner, member_id) → rows ON DELETE CASCADE
//   text_idx   (owner, id) → lowercased text       the FTS5 counterpart; derived
//
// ACCESS: a caller reads and writes only its OWN key (append / read / search /
// prune); `inject` / `dump` are plain verbs. The kernel's call gate decides who
// may reach this agent at all.
//
// Ids are the KERNEL's: a row carries the int the kernel minted, or none and this
// store takes the next one from the kernel's counter — never an id of its own.
//
// Everything here dies with the process. That is what `ram` says. NO `onDelete`
// hook: a teardown walk must not empty the store before the final flush.
//
// ONE CALLER AT A TIME (swift only). Python's store runs on one asyncio thread, so
// a verb body never interleaves with another. Swift dispatches on a thread pool:
// the kernel's flusher appends while AI agents read and append their own rows. So
// every verb body runs under the bundle's ONE lock (`RamStoreBundle.lock`), which
// guards the slot map and every `Tables` in it — rows, order, fold refs,
// the text index. No verb awaits, so the lock is never held across a suspension.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

let OPERATOR = "kernel"
/// Kinds whose `refs` name members (the fold_refs table): a fold, a clear-fold.
let REF_KINDS = ["fold", "clear"]

/// Python `_as_id`: a positive int, or an ASCII digit string (`'٣'` and
/// `'²'` are not ids — a row id is a counter value); never a bool. A digit
/// string is NOT checked for > 0, so `'0'` is 0 — falsy where python tests it.
func asId(_ x: JSON?) -> Int64? {
    switch x {
    case .integer(let n)?: return n > 0 ? n : nil
    case .string(let s)?:
        let t = pyStrip(s)
        guard t.unicodeScalars.allSatisfy(\.isASCII), pyIsDigitString(t),
            let n = try? pyInt(.string(t))
        else { return nil }
        return Int64(n)
    default: return nil
    }
}

/// python `x in container` for JSON values: a list's members (`==`), a str's
/// SUBSTRINGS (a str operand only), a dict's KEYS. Anything else — or a str
/// test with a non-str operand, a dict test with an unhashable one — raises
/// python's `TypeError`.
func pyIn(_ x: JSON, _ container: JSON) throws(PyError) -> Bool {
    switch container {
    case .array(let items):
        return items.contains { pyEq($0, x) }
    case .string(let s):
        guard case .string(let needle) = x else {
            throw PyError(
                "TypeError", "'in <string>' requires string as left operand, not \(pyTypeName(x))")
        }
        return scalarsContain(Array(s.unicodeScalars), Array(needle.unicodeScalars))
    case .object(let o):
        switch x {
        case .string(let k): return o[k] != nil
        case .array, .object: throw PyError("TypeError", "unhashable type: '\(pyTypeName(x))'")
        default: return false  // JSON keys are strings: no number or None matches
        }
    default:
        throw PyError("TypeError", "argument of type '\(pyTypeName(container))' is not iterable")
    }
}

/// `needle` as a CODE-POINT subsequence of `hay` — python's `in` on a `str`.
func scalarsContain(_ hay: [Unicode.Scalar], _ needle: [Unicode.Scalar]) -> Bool {
    if needle.isEmpty { return true }
    if needle.count > hay.count { return false }
    outer: for start in 0...(hay.count - needle.count) {
        for k in 0..<needle.count where hay[start + k] != needle[k] { continue outer }
        return true
    }
    return false
}

/// Python `==` between two JSON values: `1 == 1.0 == True`.
func pyEq(_ a: JSON, _ b: JSON) -> Bool {
    func num(_ v: JSON) -> Double? {
        switch v {
        case .integer(let n): return Double(n)
        case .double(let d): return d
        case .bool(let b): return b ? 1 : 0
        default: return nil
        }
    }
    if let x = num(a), let y = num(b) { return x == y }
    return a == b
}

/// Python `_text_of`: the content (a string, else its json.dumps), title first.
func textOf(_ row: Row) -> String {
    let c = row["content"] ?? .null
    var text: String
    if case .string(let s) = c { text = s } else { text = pyJsonDumps(c, ensureAscii: false) }
    if case .string(let t)? = row["title"], !t.isEmpty { text = "\(t)\n\(text)" }
    return text
}

/// The store's tables — RAM dictionaries in sqlite's shape. NOT thread-safe on
/// its own: touched only under `RamStoreBundle.lock` (or before it is published).
final class Tables: @unchecked Sendable {
    struct Key: Hashable {
        let owner: String
        let id: Int64
    }

    var rows: [String: [Int64: Row]] = [:]
    var order: [String: [Int64]] = [:]
    var foldRefs: [Key: [Int64]] = [:]
    var refByMember: [Key: Set<Int64>] = [:]
    var text: [Key: String] = [:]

    // ── rows ──
    func has(_ owner: String, _ rid: Int64) -> Bool { rows[owner]?[rid] != nil }
    func ids(_ owner: String) -> [Int64] { order[owner] ?? [] }

    func get(_ owner: String, _ rid: Int64) -> Row? {
        guard var out = rows[owner]?[rid] else { return nil }
        if let refs = foldRefs[Key(owner: owner, id: rid)] {
            out["refs"] = .array(refs.map { .integer($0) })
        }
        return out
    }

    /// All-or-nothing: the caller has validated every row and every ref.
    func insert(_ owner: String, _ batch: [Row]) {
        for row in batch {
            guard let rid = row["id"]?.asInt else { continue }
            var stored = row
            let refs = stored.removeValue(forKey: "refs")
            rows[owner, default: [:]][rid] = stored
            var o = order[owner] ?? []
            let at = o.firstIndex { $0 > rid } ?? o.count
            o.insert(rid, at: at)
            order[owner] = o
            if case .array(let list)? = refs {
                let members = list.compactMap { $0.asInt }
                foldRefs[Key(owner: owner, id: rid)] = members
                for m in members { refByMember[Key(owner: owner, id: m), default: []].insert(rid) }
            }
            text[Key(owner: owner, id: rid)] = textOf(row).lowercased()
        }
    }

    /// DELETE FROM rows WHERE owner=? AND id IN doomed — with every cascade.
    /// Returns the rows removed.
    func delete(_ owner: String, _ doomedIn: Set<Int64>) -> Int {
        let table = rows[owner] ?? [:]
        let doomed = doomedIn.filter { table[$0] != nil }
        for rid in doomed {
            rows[owner]?.removeValue(forKey: rid)
            let k = Key(owner: owner, id: rid)
            text.removeValue(forKey: k)
            // FK fold_refs.fold_id → rows: the fold's member list goes with it.
            for m in foldRefs.removeValue(forKey: k) ?? [] {
                let mk = Key(owner: owner, id: m)
                refByMember[mk]?.remove(rid)
                if refByMember[mk]?.isEmpty == true { refByMember.removeValue(forKey: mk) }
            }
            // FK fold_refs.member_id → rows: the member leaves every fold naming it.
            for f in refByMember.removeValue(forKey: k) ?? [] {
                let fk = Key(owner: owner, id: f)
                if let refs = foldRefs[fk] { foldRefs[fk] = refs.filter { $0 != rid } }
            }
        }
        if !doomed.isEmpty {
            order[owner] = (order[owner] ?? []).filter { !doomed.contains($0) }
        }
        return doomed.count
    }

    // ── dump ──
    func dump() -> JSON {
        var rowsOut: [JSON] = []
        for owner in rows.keys.sorted() {
            for rid in order[owner] ?? [] {
                guard let r = rows[owner]?[rid] else { continue }
                rowsOut.append(.object(["owner": .string(owner), "row": .object(r)]))
            }
        }
        let refsOut: [JSON] = foldRefs.keys
            .sorted { ($0.owner, $0.id) < ($1.owner, $1.id) }
            .map { k in
                .object([
                    "owner": .string(k.owner), "fold": .integer(k.id),
                    "members": .array((foldRefs[k] ?? []).map { .integer($0) }),
                ])
            }
        return .object(["rows": .array(rowsOut), "fold_refs": .array(refsOut)])
    }
}

/// Build `Tables` from a `dump` — every key and FK checked; a refusal on the
/// first violation (nothing half-loaded). Python `load_tables`.
func loadTables(_ tables: JSON) -> Result<Tables, RefusalBody> {
    func bad(_ msg: String) -> Result<Tables, RefusalBody> {
        .failure(RefusalBody(Refusal.make(Refusal.badArg, msg, extra: [("arg", .string("tables"))])))
    }
    guard case .object(let tab) = tables else {
        return bad("inject: `tables` must be a dump dict")
    }
    let t = Tables()
    let rowsIn = tab["rows"] ?? .null
    let rowList: [JSON]
    switch rowsIn {
    case .null: rowList = []
    case .array(let a): rowList = a
    default:
        if !pyTruthy(rowsIn) { rowList = [] } else { return bad("inject: tables.rows must be a list") }
    }
    var byOwner: OrderedDictionary<String, [Row]> = [:]
    for item in rowList {
        guard case .object(let it) = item, case .string(let owner)? = it["owner"] else {
            return bad("inject: every row needs {owner, row}")
        }
        guard case .object(var row)? = it["row"], asId(row["id"]) != nil else {
            return bad("inject: every row needs an int id")
        }
        row.removeValue(forKey: "refs")
        byOwner[owner, default: []].append(row)
    }
    for (owner, rs) in byOwner {
        var seen: Set<Int64> = []
        for r in rs {
            let rid = r["id"]?.asInt ?? 0
            if seen.contains(rid) {
                return .failure(
                    RefusalBody(
                        Refusal.make(
                            Refusal.exists, "inject: duplicate row (\(owner), \(rid))",
                            extra: [("id", .integer(rid))])))
            }
            seen.insert(rid)
        }
        t.insert(owner, rs)
    }
    for fr in tab["fold_refs"]?.asArray ?? [] {
        let owner = fr["owner"].asString ?? ""
        let fold = asId(fr["fold"])
        let members = fr["members"].asArray ?? []
        guard let fold, t.has(owner, fold) else {
            return bad("inject: fold_refs names no row \(fold.map { String($0) } ?? "None")")
        }
        if !members.allSatisfy({ t.has(owner, asId($0) ?? -1) }) {
            return bad("inject: fold \(fold) refs a missing row")
        }
        let ms = members.compactMap { asId($0) }
        t.foldRefs[Tables.Key(owner: owner, id: fold)] = ms
        for m in ms { t.refByMember[Tables.Key(owner: owner, id: m), default: []].insert(fold) }
        if let row = t.rows[owner]?[fold] {
            t.text[Tables.Key(owner: owner, id: fold)] = textOf(row).lowercased()
        }
    }
    return .success(t)
}

/// A refusal carried through a `Result`.
struct RefusalBody: Error {
    let body: JSON
    init(_ body: JSON) { self.body = body }
}

/// `ram_store.tools`.
public final class RamStoreBundle: AgentBundle, @unchecked Sendable {
    public static let handlerModule = "ram_store.tools"

    /// The store's tables live ON THE AGENT INSTANCE: one kernel's `mem` is never
    /// another kernel's `mem`, even in one process, and a re-created store starts
    /// empty. Python sets `agent._ram_tables`. RAM by design.
    private final class Slot {
        weak var agent: Agent?
        var tables: Tables
        init(_ agent: Agent, _ tables: Tables) {
            self.agent = agent
            self.tables = tables
        }
    }

    /// THE lock: guards `slots` and every `Tables` reachable from it. Taken once
    /// per verb (`handle`) and by the test accessors; every helper below runs with
    /// it held and never takes it again.
    private let lock = NSLock()
    private var slots: [ObjectIdentifier: Slot] = [:]

    public init() {}

    public var name: String { "ram_store" }

    public var verbs: [String] { Array(Self.VERBS.keys) }
    public var verbDocTable: JSON { .object(Self.VERBS.mapValues { JSON.string($0) }) }

    public var readme: String? { Self.README }

    // MARK: - tables

    /// With `lock` held.
    private func tables(_ agent: Agent) -> Tables {
        // Drop the slots of agents that are gone (python's tables die with the
        // agent object).
        slots = slots.filter { $0.value.agent != nil }
        if let s = slots[ObjectIdentifier(agent)] { return s.tables }
        let t = Tables()
        slots[ObjectIdentifier(agent)] = Slot(agent, t)
        return t
    }

    /// One owner's rows (refs included), in id order — a synchronous read for an
    /// in-process test harness; callers go through the gated verbs.
    func ownerRows(_ agent: Agent, owner: String) -> [JSON] {
        lock.withLock {
            let t = tables(agent)
            return t.ids(owner).compactMap { t.get(owner, $0).map { JSON.object($0) } }
        }
    }

    /// Seed rows straight into one owner's table (ids given, minted when absent)
    /// — a test harness seeding another owner's rows in-process.
    func seedRows(_ agent: Agent, owner: String, _ rows: [JSON]) {
        lock.withLock {
            let t = tables(agent)
            t.insert(
                owner,
                rows.compactMap { $0.asObject }.filter {
                    guard let id = $0["id"]?.asInt else { return false }
                    return !t.has(owner, id)
                })
        }
    }

    /// With `lock` held.
    private func replaceTables(_ agent: Agent, _ t: Tables) {
        slots[ObjectIdentifier(agent)] = Slot(agent, t)
    }

    // MARK: - who is calling

    private func guid(_ kernel: Kernel) -> String? {
        guard let g = kernel.root?.metaValue(forKey: "guid"), pyTruthy(g), let s = g.asString
        else { return nil }
        return s
    }

    /// `(key, display)` for whoever is calling, or a refusal.
    private func caller(_ agent: Agent, _ kernel: Kernel) -> Result<(String, String), RefusalBody> {
        guard let g = guid(kernel) else {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.notFound,
                        "ram_store: this kernel has no guid yet — the store keys every row by it",
                        hint: "boot the kernel root first (it mints the guid)")))
        }
        guard let origin = KernelTaskLocals.currentOrigin else {
            return .success(("\(g)|\(OPERATOR)", OPERATOR))
        }
        return .success(("\(g)|\(origin.value)", origin.value))
    }

    // MARK: - who owns, folds

    /// The caller's own key — the one owner every verb acts on.
    private func ownKey(_ agent: Agent, _ kernel: Kernel) -> Result<String, RefusalBody> {
        switch caller(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let w): return .success(w.0)
        }
    }


    private func foldClosure(_ t: Tables, _ owner: String, _ fold: Int64) -> Set<Int64> {
        var out: Set<Int64> = []
        var stack = [fold]
        while let x = stack.popLast() {
            if out.contains(x) { continue }
            out.insert(x)
            stack.append(contentsOf: t.foldRefs[Tables.Key(owner: owner, id: x)] ?? [])
        }
        return out
    }

    // MARK: - dispatch

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard let agent = kernel.agent(agentId) else {
            return Refusal.unknown(agentId.value, live: kernel.allAgents().map { $0.id.value })
        }
        // ONE VERB AT A TIME on this bundle's tables — see the header.
        return try lock.withLock { try dispatch(agent, kernel, payload) }
    }

    /// The verb body, with `lock` held.
    private func dispatch(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) throws(PyError)
        -> JSON?
    {
        let t = payload["type"]
        switch t.asString ?? "" {
        case "reflect": return reflect(agent)
        case "boot": return nil
        case "append": return unwrap(append(agent, kernel, payload))
        case "read": return unwrap(read(agent, kernel, payload))
        case "search": return unwrap(search(agent, kernel, payload))
        case "prune": return unwrap(try prune(agent, kernel, payload))
        case "inject": return unwrap(inject(agent, payload))
        case "dump": return .object(["tables": tables(agent).dump()])
        default:
            return Refusal.make(
                Refusal.unhandled, "ram_store: unknown type \(pyRepr(t))", extra: [("verb", t)])
        }
    }

    private func unwrap(_ r: Result<JSON, RefusalBody>) -> JSON {
        switch r {
        case .success(let v): return v
        case .failure(let e): return e.body
        }
    }

    private func reflect(_ agent: Agent) -> JSON {
        let t = tables(agent)
        return .object([
            "id": .string(agent.id.value),
            "sentence": .string(
                "Memory store in RAM — rows per owner (the caller), folds, "
                    + "search; ephemeral: everything is lost when the process exits."),
            "durable": .bool(false),
            "owners": .integer(Int64(t.rows.values.filter { !$0.isEmpty }.count)),
            "rows": .integer(Int64(t.rows.values.reduce(0) { $0 + $1.count })),
            "optional_verbs": .array([.string("search")]),
        ])
    }

    private func append(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) -> Result<JSON, RefusalBody> {
        let owner: String
        switch ownKey(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let o): owner = o
        }
        guard case .array(let list) = payload["rows"], !list.isEmpty,
            list.allSatisfy({ if case .object = $0 { return true } else { return false } })
        else {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.badArg, "append: `rows` must be a non-empty list of rows",
                        extra: [("arg", .string("rows"))])))
        }
        let t = tables(agent)
        var staged: [Row] = []
        var batch: Set<Int64> = []
        for item in list {
            var r = item.asObject ?? [:]
            let rid: Int64
            switch r["id"] {
            case nil, .some(.null):
                rid = kernel.mintId()
            case .some(.integer(let n)) where n > 0:
                rid = n
            case .some(let v):
                return .failure(
                    RefusalBody(
                        Refusal.make(
                            Refusal.badArg, "append: a row id is a kernel int, got \(pyRepr(v))",
                            extra: [("arg", .string("rows"))])))
            }
            if t.has(owner, rid) || batch.contains(rid) {
                return .failure(
                    RefusalBody(
                        Refusal.make(
                            Refusal.exists, "append: row \(rid) already exists for this owner",
                            extra: [("id", .integer(rid))])))
            }
            batch.insert(rid)
            r["id"] = .integer(rid)
            staged.append(r)
        }
        for r in staged {
            guard let refs = r["refs"], refs != .null else { continue }
            let rid = r["id"]?.asInt ?? 0
            guard case .array(let ms) = refs, ms.allSatisfy({ asId($0) != nil }) else {
                return .failure(
                    RefusalBody(
                        Refusal.make(
                            Refusal.badArg, "append: row \(rid) refs must be row ids",
                            extra: [("arg", .string("rows"))])))
            }
            let missing = ms.filter { m in
                guard case .integer(let n) = m else { return true }
                return !(t.has(owner, n) || batch.contains(n))
            }
            if !missing.isEmpty {
                return .failure(
                    RefusalBody(
                        Refusal.make(
                            Refusal.notFound,
                            "append: row \(rid) refs rows that do not exist for this owner: "
                                + pyRepr(.array(missing)),
                            extra: [("arg", .string("rows"))])))
            }
        }
        t.insert(owner, staged)
        return .success(
            .object([
                "ids": .array(staged.map { $0["id"] ?? .null }),
                "appended": .integer(Int64(staged.count)),
            ]))
    }

    private func read(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) -> Result<JSON, RefusalBody> {
        let t = tables(agent)
        let owner: String
        switch ownKey(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let o): owner = o
        }
        var ids = t.ids(owner)
        let row = { (i: Int64) -> Row in t.rows[owner]?[i] ?? [:] }
        if case .string(let flk) = payload["from_last_kind"], !flk.isEmpty {
            if let last = ids.last(where: { row($0)["kind"]?.asString == flk }) {
                ids = ids.filter { $0 >= last }
            }
        }
        if case .array(let kinds) = payload["kinds"], !kinds.isEmpty {
            ids = ids.filter { i in kinds.contains { pyEq($0, row(i)["kind"] ?? .null) } }
        }
        if case .object(let match) = payload["match"], !match.isEmpty {
            ids = ids.filter { i in match.allSatisfy { k, v in pyEq(row(i)[k] ?? .null, v) } }
        }
        if case .array(let want) = payload["ids"] {
            let wanted = Set(want.compactMap { asId($0) })
            ids = ids.filter { wanted.contains($0) }
        }
        let after = asId(payload["after"])
        let before = asId(payload["before"])
        if payload["order"].asString == "desc" {
            ids.reverse()
            if let after { ids = ids.filter { $0 < after } }
        } else if let after {
            ids = ids.filter { $0 > after }
        }
        if let before { ids = ids.filter { $0 < before } }
        let total = ids.count
        // python `ids[:limit] if limit else ids` — a `'0'` limit is falsy: every
        // row, no cursor.
        let limit = asId(payload["limit"]) ?? 0
        let page = limit != 0 ? Array(ids.prefix(Int(clamping: limit))) : ids
        let next: JSON = (limit != 0 && total > page.count) ? .integer(page.last!) : .null
        return .success(
            .object([
                "rows": .array(page.compactMap { t.get(owner, $0).map { JSON.object($0) } }),
                "next": next,
                "total": .integer(Int64(total)),
            ]))
    }

    private func search(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) -> Result<JSON, RefusalBody> {
        let raw = payload["query"]
        let q = pyStrip(pyTruthy(raw) ? pyStr(raw) : "").lowercased()
        if q.isEmpty { return .failure(RefusalBody(Refusal.missing("search", "query"))) }
        let t = tables(agent)
        let owner: String
        switch ownKey(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let o): owner = o
        }
        var ids = t.ids(owner)
        if case .array(let want) = payload["ids"], !want.isEmpty {
            var scope: Set<Int64> = []
            for x in want {
                if let rid = asId(x) { scope.formUnion(foldClosure(t, owner, rid)) }
            }
            ids = ids.filter { scope.contains($0) }
        }
        // python `_as_id(...) or 100` — a `'0'` is falsy and takes the default.
        let rawLimit = asId(payload["limit"]) ?? 0
        let rawWidth = asId(payload["width"]) ?? 0
        let limit = Int(clamping: rawLimit != 0 ? rawLimit : 100)
        let width = Int(clamping: rawWidth != 0 ? rawWidth : 200)
        var matches: [JSON] = []
        var total = 0
        // A SUBSTRING OF CODE POINTS, as python's `in` is: a `String.contains`
        // compares graphemes under canonical equivalence, which finds text python
        // calls different (NFC vs NFD) and misses `e` inside a decomposed `é`.
        let qs = Array(q.unicodeScalars)
        for i in ids {
            guard let idx = t.text[Tables.Key(owner: owner, id: i)],
                scalarsContain(Array(idx.unicodeScalars), qs),
                let row = t.rows[owner]?[i]
            else { continue }
            for (n, line) in pySplitlines(textOf(row)).enumerated()
            where scalarsContain(Array(line.lowercased().unicodeScalars), qs) {
                total += 1
                if matches.count < limit {
                    let stripped = pyStrip(line)
                    matches.append(
                        .object([
                            "id": .integer(i), "line": .integer(Int64(n + 1)),
                            "text": .string(
                                String(String.UnicodeScalarView(stripped.unicodeScalars.prefix(width)))),
                        ]))
                }
            }
        }
        return .success(
            .object([
                "matches": .array(matches), "total": .integer(Int64(total)),
                "truncated": .bool(total > matches.count),
            ]))
    }

    private func prune(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) throws(PyError)
        -> Result<JSON, RefusalBody>
    {
        let owner: String
        switch ownKey(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let o): owner = o
        }
        let t = tables(agent)
        let before = asId(payload["before_id"])
        let want = payload["ids"].asArray
        if before == nil && want == nil {
            return .failure(RefusalBody(Refusal.missing("prune", "before_id")))
        }
        // python `payload.get("keep") or []`, then `row.get("kind") in keep`:
        // whatever `keep` is, python's `in` decides — a string is a SUBSTRING
        // test, a dict a key test, and a value `in` cannot search raises.
        let keepRaw = payload["keep"]
        let keep: JSON = pyTruthy(keepRaw) ? keepRaw : .array([])
        let wantIds = Set((want ?? []).compactMap { asId($0) })
        var doomed: Set<Int64> = []
        for i in t.ids(owner) {
            let kind = t.rows[owner]?[i]?["kind"] ?? .null
            if try pyIn(kind, keep) { continue }
            if let before, i < before {
                doomed.insert(i)
            } else if want != nil && wantIds.contains(i) {
                doomed.insert(i)
            }
        }
        let n = t.delete(owner, doomed)
        return .success(.object(["pruned": .integer(Int64(n))]))
    }

    private func inject(_ agent: Agent, _ payload: JSON) -> Result<JSON, RefusalBody> {
        switch loadTables(payload["tables"]) {
        case .failure(let r): return .failure(r)
        case .success(let t):
            replaceTables(agent, t)
            return .success(
                .object(["rows": .integer(Int64(t.rows.values.reduce(0) { $0 + $1.count }))]))
        }
    }

    // MARK: - verb docs + readme (python's, verbatim)

    static let VERBS: OrderedDictionary<String, String> = [
        "reflect": "Identity + posture: how many owners/rows, which optional verbs exist. No args.",
        "boot": "No-op — the tables start empty (RAM) or are filled by `inject`.",
        "append": "args: rows:list[dict] (req). Appends rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}.",
        "read": "args: after:int?, before:int?, limit:int?, order:'asc'|'desc'?, kinds:list?, match:dict? (exact field equality), ids:list?, from_last_kind:str? (start at the newest row of that kind). Rows whole, oldest first by default. Returns {rows, next, total}.",
        "search": "args: query:str (req — case-insensitive substring), ids:list? (rows to search; a fold id includes its members), limit:int? (default 100), width:int? (chars per line, default 200). Every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}.",
        "prune": "args: before_id:int? and/or ids:list? (req — one of them), keep:list? (kinds kept). Deletes YOUR rows below `before_id` (or exactly `ids`) except kinds in `keep`; every fold ref naming a deleted row goes with it (FK cascade). Returns {pruned}.",
        "inject": "args: tables:dict (req — a `dump`). Replaces the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows}.",
        "dump": "No args. The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}]} — exactly what `inject` takes back. Returns {tables}.",
    ]

    static let README = """
        # ram_store — memory in RAM

        A MEMORY STORE agent: conversation rows for many owners, folds, search.
        Everything it holds is lost when the process exits.

        **Owner = caller.** Every row is keyed by who wrote it — `<kernel guid>|<agent id>`,
        `<kernel guid>|kernel` for the operator and the kernel's own bookkeeping. A caller
        writes and reads its OWN rows; there is no `owner` field to forge.

        | verb | what |
        |---|---|
        | `append {rows}` | add rows under your key, all or none; ids are the kernel's |
        | `read {after?, limit?, kinds?, match?, from_last_kind?, order?}` | your rows |
        | `search {query, ids?}` | the LINES that mention something |
        | `prune {before_id \\| ids, keep?}` | delete your rows; fold refs on them go too |
        | `inject {tables}` · `dump` | replace / capture the whole store |

        **Access:** a caller reads and writes its own rows. Who may call any verb —
        `inject`/`dump` included — is the kernel's call gate alone.

        **Same tables as sqlite:** rows `(owner, id)`, fold_refs (FK cascade on their
        rows), a text index. A durable store answers the same verbs.

        """
}
