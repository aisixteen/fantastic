// ram_store — a MEMORY STORE agent held in RAM.
//
// Port of python `bundled_agents/memory/ram_store/src/ram_store/tools.py`, which
// is canonical.
//
// A memory store keeps conversation rows for MANY owners (multi-tenant). Every
// row is keyed by `(owner, id)`; the owner is never a field a caller writes — it
// is WHO CALLED: `<kernel guid>|<caller agent id>` for a local caller, the
// qualified address a bridge leg relays for a caller on another kernel, and
// `<guid>|kernel` for the operator (`sender=None`). So a caller writes ONLY under
// its own key, by construction.
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
//   grants     PRIMARY KEY gid                     {owner, reader, scope, mode,
//                                                   anchor, as_of, until, why, …}
//              FK (owner, anchor) → rows ON DELETE CASCADE
//              UNIQUE (owner, reader, scope, mode) an identical share is one grant
//   text_idx   (owner, id) → lowercased text       the FTS5 counterpart; derived
//
// ACCESS, nothing implicit:
//   - any caller: its OWN key (append / read / search / prune / share / unshare /
//     grants), plus rows another owner GRANTED it (`read`/`search` with `grant`).
//   - `operator: true` on the record serves the operator at all (as `kernel`).
//   - `admins: [ids]` (`"kernel"` = the operator) may name another `owner` and may
//     `inject` / `dump` the whole store.
// The kernel's call gate still decides who may reach this agent at all.
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
// guards the slot map and every `Tables` in it — rows, order, fold refs, grants,
// the text index. No verb awaits, so the lock is never held across a suspension.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

let OPERATOR = "kernel"
let MODES = ["frozen", "live"]
let SCOPE_KINDS = ["head_until", "range", "fold", "row"]
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

/// `json.dumps(v, sort_keys=True)` — the grant uniqueness key.
func canonical(_ v: JSON) -> String {
    switch v {
    case .object(let o):
        return "{" + o.keys.sorted().map { pyRepr($0) + ":" + canonical(o[$0]!) }.joined(separator: ",") + "}"
    case .array(let a):
        return "[" + a.map(canonical).joined(separator: ",") + "]"
    default:
        return pyJsonDumps(v)
    }
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
    var grants: [Int64: Row] = [:]
    var grantSeq: Int64 = 0
    var grantByAnchor: [Key: Set<Int64>] = [:]
    var grantKey: [String: Int64] = [:]
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
    /// Returns (rows removed, grants removed).
    func delete(_ owner: String, _ doomedIn: Set<Int64>) -> (Int, Int) {
        let table = rows[owner] ?? [:]
        let doomed = doomedIn.filter { table[$0] != nil }
        var grantsGone = 0
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
            // FK grants.anchor → rows: a grant anchored here is gone.
            for gid in grantByAnchor.removeValue(forKey: k) ?? [] {
                if dropGrant(gid) { grantsGone += 1 }
            }
        }
        if !doomed.isEmpty {
            order[owner] = (order[owner] ?? []).filter { !doomed.contains($0) }
        }
        return (doomed.count, grantsGone)
    }

    // ── grants ──
    static func uniqueKey(_ g: Row) -> String {
        [
            g["owner"]?.asString ?? "", g["reader"]?.asString ?? "",
            canonical(g["scope"] ?? .null), g["mode"]?.asString ?? "",
        ].joined(separator: "\u{0}")
    }

    func addGrant(_ g: Row) -> (Int64, Bool) {
        let key = Tables.uniqueKey(g)
        if let gid = grantKey[key] { return (gid, false) }
        grantSeq += 1
        let gid = grantSeq
        var stored = g
        stored["grant"] = .integer(gid)
        grants[gid] = stored
        grantKey[key] = gid
        let anchor = Key(owner: g["owner"]?.asString ?? "", id: g["anchor"]?.asInt ?? 0)
        grantByAnchor[anchor, default: []].insert(gid)
        return (gid, true)
    }

    @discardableResult
    func dropGrant(_ gid: Int64) -> Bool {
        guard let g = grants.removeValue(forKey: gid) else { return false }
        grantKey.removeValue(forKey: Tables.uniqueKey(g))
        let anchor = Key(owner: g["owner"]?.asString ?? "", id: g["anchor"]?.asInt ?? 0)
        grantByAnchor[anchor]?.remove(gid)
        if grantByAnchor[anchor]?.isEmpty == true { grantByAnchor.removeValue(forKey: anchor) }
        return true
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
        let grantsOut: [JSON] = grants.keys.sorted().map { .object(grants[$0]!) }
        return .object([
            "rows": .array(rowsOut), "fold_refs": .array(refsOut), "grants": .array(grantsOut),
        ])
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
    for g in tab["grants"]?.asArray ?? [] {
        guard case .object(let gd) = g, let owner = gd["owner"]?.asString,
            let anchor = gd["anchor"]?.asInt, t.has(owner, anchor)
        else {
            return bad("inject: a grant's anchor names no row")
        }
        guard let gid = asId(gd["grant"]) else {
            return bad("inject: a grant needs an int `grant`")
        }
        var body = gd
        body.removeValue(forKey: "grant")
        var stored = body
        stored["grant"] = .integer(gid)
        t.grants[gid] = stored
        t.grantKey[Tables.uniqueKey(body)] = gid
        t.grantByAnchor[Tables.Key(owner: owner, id: anchor), default: []].insert(gid)
        t.grantSeq = max(t.grantSeq, gid)
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
    /// — the in-process twin of an admin `append {owner}` for a test harness.
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

    /// An owner/reader NAME as a table key: a qualified `guid|id` verbatim, a bare
    /// local id under this kernel's guid.
    private func key(_ kernel: Kernel, _ name: JSON?) -> String? {
        guard case .string(let raw)? = name else { return nil }
        let n = pyStrip(raw)
        if n.isEmpty { return nil }
        if n.contains("|") { return n }
        guard let g = guid(kernel) else { return nil }
        return "\(g)|\(n)"
    }

    /// A key as a caller reads it: the bare id for this kernel, qualified
    /// otherwise.
    private func display(_ kernel: Kernel, _ key: String) -> String {
        if let g = guid(kernel), key.hasPrefix(g + "|") {
            return String(key.dropFirst(g.count + 1))
        }
        return key
    }

    /// `(key, display)` for whoever is calling, or a refusal.
    private func caller(_ agent: Agent, _ kernel: Kernel) -> Result<(String, String), RefusalBody> {
        if let remote = KernelTaskLocals.currentRemote, !remote.isEmpty {
            return .success((remote, remote))
        }
        guard let g = guid(kernel) else {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.notFound,
                        "ram_store: this kernel has no guid yet — the store keys every row by it",
                        hint: "boot the kernel root first (it mints the guid)")))
        }
        guard let origin = KernelTaskLocals.currentOrigin else {
            if !pyTruthy(agent.metaValue(forKey: "operator") ?? .null) {
                return .failure(
                    RefusalBody(
                        Refusal.make(
                            Refusal.unauthorized,
                            "\(agent.id.value): the operator is not served here",
                            hint: "update_agent id=\(agent.id.value) operator=true to serve the "
                                + "operator (as 'kernel')",
                            extra: [("side", .string("store"))])))
            }
            return .success(("\(g)|\(OPERATOR)", OPERATOR))
        }
        return .success(("\(g)|\(origin.value)", origin.value))
    }

    private func isAdmin(_ agent: Agent, _ display: String) -> Bool {
        switch agent.metaValue(forKey: "admins") {
        case .string(let s)?: return s == display
        case .array(let a)?: return a.contains(.string(display))
        default: return false
        }
    }

    /// (owner key, caller key, caller display) for a verb that acts on ONE owner:
    /// the caller's own key, or `owner` when the caller is an admin.
    private func ownerFor(_ agent: Agent, _ kernel: Kernel, _ payload: JSON, _ verb: String)
        -> Result<(String, String, String), RefusalBody>
    {
        let who: (String, String)
        switch caller(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let w): who = w
        }
        let (me, disp) = who
        let name = payload["owner"]
        if name == .null { return .success((me, me, disp)) }
        guard let owner = key(kernel, name) else {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.badArg, "\(verb): `owner` must be an agent id",
                        extra: [("arg", .string("owner"))])))
        }
        if owner != me && !isAdmin(agent, disp) {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.unauthorized,
                        "\(verb): \(pyRepr(disp)) may act only on its own rows, not \(pyRepr(name))",
                        hint: "a caller writes and reads its OWN key; another owner's rows are "
                            + "reached through a grant it gives (`share`), or by an agent listed in "
                            + "\(agent.id.value).admins",
                        extra: [("side", .string("store"))])))
        }
        return .success((owner, me, disp))
    }

    // MARK: - scopes

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

    /// The rows a grant reaches, as of its bound (frozen) or following the head
    /// (live).
    private func scopeIds(_ t: Tables, _ g: Row) -> Set<Int64> {
        let owner = g["owner"]?.asString ?? ""
        let scope = g["scope"] ?? .null
        let live = g["mode"]?.asString == "live"
        let ids = t.ids(owner)
        let idSet = Set(ids)
        var base: Set<Int64>
        if let r = scope["row"].asInt, scope.asObject?["row"] != nil {
            base = idSet.contains(r) ? [r] : []
        } else if scope.asObject?["fold"] != nil {
            base = foldClosure(t, owner, scope["fold"].asInt ?? 0).intersection(idSet)
        } else if scope.asObject?["range"] != nil {
            let lo = scope["range"]["from"].asInt ?? 0
            let hi = scope["range"]["to"].asInt ?? 0
            base = Set(ids.filter { $0 >= lo && (live || $0 <= hi) })
        } else {
            let n = scope["head_until"].asInt ?? 0
            base = Set(ids.filter { live || $0 <= n })
        }
        if !live, let asOf = g["as_of"]?.asInt {
            base = base.filter { $0 <= asOf }
        }
        return base
    }

    /// (normalised scope, anchor) — the anchor is the scope's LOWEST existing row,
    /// so a prune that touches any part of the scope removes it (FK cascade).
    private func parseScope(_ t: Tables, _ owner: String, _ scope: JSON)
        -> Result<(JSON, Int64), RefusalBody>
    {
        func refuse(_ code: String, _ msg: String) -> Result<(JSON, Int64), RefusalBody> {
            .failure(RefusalBody(Refusal.make(code, msg, extra: [("arg", .string("scope"))])))
        }
        guard case .object(let sc) = scope, sc.count == 1, let kind = sc.keys.first,
            SCOPE_KINDS.contains(kind)
        else {
            return refuse(
                Refusal.badArg,
                "share: `scope` must be ONE of {head_until: <row>}, {range: {from, to}}, "
                    + "{fold: <id>}, {row: <id>} — a descriptor, never a list of messages")
        }
        let val = sc[kind]!
        let ids = t.ids(owner)
        if kind == "range" {
            guard case .object(let rv) = val else {
                return refuse(Refusal.badArg, "share: range must be {from, to}")
            }
            guard let lo = asId(rv["from"]), let hi = asId(rv["to"]), lo <= hi else {
                return refuse(Refusal.badArg, "share: range needs from <= to (row ids)")
            }
            let inside = ids.filter { lo <= $0 && $0 <= hi }
            guard let first = inside.first else {
                return refuse(Refusal.notFound, "share: none of YOUR rows lie in \(lo)..\(hi)")
            }
            return .success(
                (.object(["range": .object(["from": .integer(lo), "to": .integer(hi)])]), first))
        }
        guard let rid = asId(val) else {
            return refuse(Refusal.badArg, "share: \(kind) must be a row id")
        }
        if kind == "head_until" {
            guard let first = ids.first, first <= rid else {
                return refuse(Refusal.notFound, "share: you have no rows up to \(rid)")
            }
            return .success((.object(["head_until": .integer(rid)]), first))
        }
        guard t.has(owner, rid) else {
            // ZERO TOLERANCE: a row that is not under YOUR key is not yours to
            // share — a foreign row, or one shared WITH you, simply is not found.
            return refuse(
                Refusal.notFound,
                "share: row \(rid) is not one of YOUR rows — only rows you own can be shared")
        }
        if kind == "fold" {
            let k = t.rows[owner]?[rid]?["kind"]?.asString ?? ""
            if !REF_KINDS.contains(k) {
                return refuse(Refusal.badArg, "share: row \(rid) is not a fold")
            }
        }
        return .success((.object([kind: .integer(rid)]), rid))
    }

    private func grantView(_ kernel: Kernel, _ g: Row) -> JSON {
        .object([
            "grant": g["grant"] ?? .null,
            "owner": .string(display(kernel, g["owner"]?.asString ?? "")),
            "reader": .string(display(kernel, g["reader"]?.asString ?? "")),
            "scope": g["scope"] ?? .null,
            "mode": g["mode"] ?? .null,
            "as_of": g["as_of"] ?? .null,
            "until": g["until"] ?? .null,
            "why": g["why"] ?? .null,
            "created_by": g["created_by"] ?? .null,
            "ts": g["ts"] ?? .null,
        ])
    }

    /// (owner key, allowed ids | nil = all, grant | nil) for a read-side verb:
    /// `grant` reads another owner's scope; else the caller's own key (or
    /// `owner`, admin).
    private func readable(_ agent: Agent, _ kernel: Kernel, _ t: Tables, _ payload: JSON, _ verb: String)
        -> Result<(String, Set<Int64>?, Row?), RefusalBody>
    {
        let gid = payload["grant"]
        if gid == .null {
            switch ownerFor(agent, kernel, payload, verb) {
            case .failure(let r): return .failure(r)
            case .success(let o): return .success((o.0, nil, nil))
            }
        }
        let who: (String, String)
        switch caller(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let w): who = w
        }
        let (me, disp) = who
        guard let g = t.grants[asId(gid) ?? -1] else {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.notFound,
                        "\(verb): grant \(pyRepr(gid)) does not exist — revoked (unshare) or its "
                            + "rows were pruned",
                        extra: [("arg", .string("grant"))])))
        }
        if g["reader"]?.asString != me && !isAdmin(agent, disp) {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.unauthorized,
                        "\(verb): grant \(pyStr(gid)) was given to "
                            + "\(pyRepr(display(kernel, g["reader"]?.asString ?? ""))), not "
                            + "\(pyRepr(disp))",
                        extra: [("side", .string("store"))])))
        }
        return .success((g["owner"]?.asString ?? "", scopeIds(t, g), g))
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
        case "share": return unwrap(share(agent, kernel, payload))
        case "unshare": return unwrap(unshare(agent, kernel, payload))
        case "grants": return unwrap(grantsVerb(agent, kernel, payload))
        case "prune": return unwrap(try prune(agent, kernel, payload))
        case "inject": return unwrap(inject(agent, kernel, payload))
        case "dump": return unwrap(dump(agent, kernel))
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
        var verbs: OrderedDictionary<String, JSON> = [:]
        for (k, v) in Self.VERBS {
            verbs[k] = .string(String(v.split(separator: "\n", omittingEmptySubsequences: false).first ?? ""))
        }
        return .object([
            "id": .string(agent.id.value),
            "sentence": .string(
                "Memory store in RAM — rows per owner (the caller), folds, grants, "
                    + "search; ephemeral: everything is lost when the process exits."),
            "durable": .bool(false),
            "owners": .integer(Int64(t.rows.values.filter { !$0.isEmpty }.count)),
            "rows": .integer(Int64(t.rows.values.reduce(0) { $0 + $1.count })),
            "grants": .integer(Int64(t.grants.count)),
            "operator": .bool(pyTruthy(agent.metaValue(forKey: "operator") ?? .null)),
            "admins": {
                let a = agent.metaValue(forKey: "admins") ?? .null
                return pyTruthy(a) ? a : .array([])
            }(),
            "optional_verbs": .array([.string("search")]),
            "verbs": .object(verbs),
        ])
    }

    private func append(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) -> Result<JSON, RefusalBody> {
        let owner: String
        switch ownerFor(agent, kernel, payload, "append") {
        case .failure(let r): return .failure(r)
        case .success(let o): owner = o.0
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
        let allowed: Set<Int64>?
        switch readable(agent, kernel, t, payload, "read") {
        case .failure(let r): return .failure(r)
        case .success(let r): (owner, allowed) = (r.0, r.1)
        }
        var ids = t.ids(owner)
        if let allowed { ids = ids.filter { allowed.contains($0) } }
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
        let allowed: Set<Int64>?
        switch readable(agent, kernel, t, payload, "search") {
        case .failure(let r): return .failure(r)
        case .success(let r): (owner, allowed) = (r.0, r.1)
        }
        var ids = t.ids(owner)
        if let allowed { ids = ids.filter { allowed.contains($0) } }
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

    private func share(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) -> Result<JSON, RefusalBody> {
        let who: (String, String)
        switch caller(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let w): who = w
        }
        let me = who.0
        guard let reader = key(kernel, payload["with"]) else {
            return .failure(RefusalBody(Refusal.missing("share", "with")))
        }
        if reader == me {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.badArg, "share: you already read your own rows",
                        extra: [("arg", .string("with"))])))
        }
        let modeRaw = payload["mode"]
        let mode = pyTruthy(modeRaw) ? modeRaw : .string("frozen")
        guard let m = mode.asString, MODES.contains(m) else {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.badArg, "share: mode must be one of ('frozen', 'live')",
                        extra: [("arg", .string("mode"))])))
        }
        let t = tables(agent)
        let scope: JSON
        let anchor: Int64
        switch parseScope(t, me, payload["scope"]) {
        case .failure(let r): return .failure(r)
        case .success(let p): (scope, anchor) = p
        }
        let ids = t.ids(me)
        let whyRaw = payload["why"]
        let whyText = pySplitWhitespace(pyTruthy(whyRaw) ? pyStr(whyRaw) : "").joined(separator: " ")
        let why = String(String.UnicodeScalarView(whyText.unicodeScalars.prefix(300)))
        let grant: Row = [
            "owner": .string(me),
            "reader": .string(reader),
            "scope": scope,
            "mode": .string(m),
            "anchor": .integer(anchor),
            "as_of": (m == "frozen" && !ids.isEmpty) ? .integer(ids.last!) : .null,
            "until": .null,  // reserved: a row limit / time bound — unused
            "why": why.isEmpty ? .null : .string(why),
            "created_by": KernelTaskLocals.currentTask.map { JSON.integer($0.value) } ?? .null,
            "ts": .double(Date().timeIntervalSince1970),
        ]
        let (gid, created) = t.addGrant(grant)
        let g = t.grants[gid] ?? [:]
        return .success(
            .object([
                "grant": .integer(gid), "anchor": .integer(anchor), "as_of": g["as_of"] ?? .null,
                "created": .bool(created),
            ]))
    }

    private func unshare(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) -> Result<JSON, RefusalBody> {
        let who: (String, String)
        switch caller(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let w): who = w
        }
        guard let gid = asId(payload["grant"]) else {
            return .failure(RefusalBody(Refusal.missing("unshare", "grant")))
        }
        let t = tables(agent)
        guard let g = t.grants[gid] else {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.notFound, "unshare: no grant \(gid)",
                        extra: [("arg", .string("grant"))])))
        }
        if g["owner"]?.asString != who.0 && !isAdmin(agent, who.1) {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.unauthorized, "unshare: grant \(gid) is not yours to revoke",
                        extra: [("side", .string("store"))])))
        }
        t.dropGrant(gid)
        return .success(.object(["unshared": .bool(true), "grant": .integer(gid)]))
    }

    private func grantsVerb(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) -> Result<JSON, RefusalBody> {
        let who: (String, String)
        switch caller(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let w): who = w
        }
        let side = payload["side"].asString
        guard side == "given" || side == "received" else {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.badArg, "grants: side must be 'given' or 'received'",
                        extra: [("arg", .string("side"))])))
        }
        let field = side == "given" ? "owner" : "reader"
        let t = tables(agent)
        let out: [JSON] = t.grants.keys.sorted().compactMap { gid in
            guard let g = t.grants[gid], g[field]?.asString == who.0 else { return nil }
            return grantView(kernel, g)
        }
        return .success(.object(["grants": .array(out)]))
    }

    private func prune(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) throws(PyError)
        -> Result<JSON, RefusalBody>
    {
        let owner: String
        switch ownerFor(agent, kernel, payload, "prune") {
        case .failure(let r): return .failure(r)
        case .success(let o): owner = o.0
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
        let (n, g) = t.delete(owner, doomed)
        return .success(.object(["pruned": .integer(Int64(n)), "grants_removed": .integer(Int64(g))]))
    }

    private func inject(_ agent: Agent, _ kernel: Kernel, _ payload: JSON) -> Result<JSON, RefusalBody> {
        let who: (String, String)
        switch caller(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let w): who = w
        }
        guard isAdmin(agent, who.1) else {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.unauthorized,
                        "inject: \(pyRepr(who.1)) is not an admin of \(agent.id.value)",
                        extra: [("side", .string("store"))])))
        }
        switch loadTables(payload["tables"]) {
        case .failure(let r): return .failure(r)
        case .success(let t):
            replaceTables(agent, t)
            return .success(
                .object([
                    "rows": .integer(Int64(t.rows.values.reduce(0) { $0 + $1.count })),
                    "grants": .integer(Int64(t.grants.count)),
                ]))
        }
    }

    private func dump(_ agent: Agent, _ kernel: Kernel) -> Result<JSON, RefusalBody> {
        let who: (String, String)
        switch caller(agent, kernel) {
        case .failure(let r): return .failure(r)
        case .success(let w): who = w
        }
        guard isAdmin(agent, who.1) else {
            return .failure(
                RefusalBody(
                    Refusal.make(
                        Refusal.unauthorized,
                        "dump: \(pyRepr(who.1)) is not an admin of \(agent.id.value)",
                        extra: [("side", .string("store"))])))
        }
        return .success(.object(["tables": tables(agent).dump()]))
    }

    /// `operator` is a bool; `admins` a list of ids. Python `validate_meta`.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        if let op = meta["operator"] {
            if case .bool = op {} else { return "operator must be true or false" }
        }
        if let a = meta["admins"] {
            if case .string = a { return nil }
            guard case .array(let list) = a,
                list.allSatisfy({ if case .string(let s) = $0 { return !s.isEmpty } else { return false } })
            else {
                return "admins must be a list of agent ids ('kernel' = the operator)"
            }
        }
        return nil
    }

    // MARK: - verb docs + readme (python's, verbatim)

    static let VERBS: OrderedDictionary<String, String> = [
        "reflect": "Identity + posture: how many owners/rows/grants, who is admin, whether the operator is served, which optional verbs exist. No args.",
        "boot": "No-op — the tables start empty (RAM) or are filled by `inject`.",
        "append": "args: rows:list[dict] (req), owner:str? (admins only). Appends rows under the CALLER's key, all or none. A row carries the kernel id it was minted with, or none and the next id is taken from the kernel's counter; `refs` must name rows of the same owner (this batch included). Returns {ids:[int], appended:int}.",
        "read": "args: grant:int? (read another owner's scope), owner:str? (admins), after:int?, before:int?, limit:int?, order:'asc'|'desc'?, kinds:list?, match:dict? (exact field equality), ids:list?, from_last_kind:str? (start at the newest row of that kind). Rows whole, oldest first by default. Returns {rows, next, total}.",
        "search": "args: query:str (req — case-insensitive substring), grant:int?, owner:str? (admins), ids:list? (rows to search; a fold id includes its members), limit:int? (default 100), width:int? (chars per line, default 200). Every matching LINE with its row id and line number. Returns {matches:[{id, line, text}], total, truncated}.",
        "share": "args: with:str (req — the reader's agent id), scope:dict (req — ONE of {head_until:<row>}, {range:{from,to}}, {fold:<id>}, {row:<id>}), mode:'frozen'|'live'? (default frozen), why:str?. Grants `with` read access to that scope of YOUR rows — only rows you own (never a foreign row, never one shared with you). An identical share returns the existing grant. Returns {grant, anchor, as_of, created}.",
        "unshare": "args: grant:int (req). Revokes a grant YOU gave (admins: any). The reader's next read under it fails. Returns {unshared:true, grant}.",
        "grants": "args: side:'given'|'received' (req). The grants YOU gave, or the ones given TO you. Returns {grants:[{grant, owner, reader, scope, mode, as_of, until, why, created_by, ts}]}.",
        "prune": "args: before_id:int? and/or ids:list? (req — one of them), keep:list? (kinds kept), owner:str? (admins). Deletes YOUR rows below `before_id` (or exactly `ids`) except kinds in `keep`; every fold ref and every grant anchored on a deleted row goes with it (FK cascade). Returns {pruned, grants_removed}.",
        "inject": "args: tables:dict (req — a `dump`). ADMINS ONLY. Replaces the WHOLE store with the given tables, every key and FK checked; nothing changes on a refusal. Returns {rows, grants}.",
        "dump": "No args. ADMINS ONLY. The whole store as tables — {rows:[{owner, row}], fold_refs:[{owner, fold, members}], grants:[…]} — exactly what `inject` takes back. Returns {tables}.",
    ]

    static let README = """
        # ram_store — memory in RAM

        A MEMORY STORE agent: conversation rows for many owners, folds, grants, search.
        Everything it holds is lost when the process exits.

        **Owner = caller.** Every row is keyed by who wrote it — `<kernel guid>|<agent id>`,
        the qualified address of a caller on another kernel, or `kernel` for the operator. A
        caller writes and reads its OWN rows; there is no `owner` field to forge.

        | verb | what |
        |---|---|
        | `append {rows}` | add rows under your key, all or none; ids are the kernel's |
        | `read {grant?, after?, limit?, kinds?, match?, from_last_kind?, order?}` | your rows, or a granted scope |
        | `search {query, ids?, grant?}` | the LINES that mention something |
        | `share {with, scope, mode?, why?}` | let `with` read a scope of YOUR rows |
        | `unshare {grant}` · `grants {side}` | revoke · list given/received |
        | `prune {before_id \\| ids, keep?}` | delete your rows; folds refs + grants on them go too |
        | `inject {tables}` · `dump` | admins: replace / capture the whole store |

        A **scope** is a descriptor, never a list of messages: `{head_until: <row>}`,
        `{range: {from, to}}`, `{fold: <id>}` (the fold and its members), `{row: <id>}`.
        `frozen` (default) stops at the last row when shared; `live` follows your head.

        **Access, nothing implicit:** `operator: true` serves the operator (as `kernel`);
        `admins: [ids]` (`kernel` = the operator) may name another `owner` and may
        `inject`/`dump`. Everyone else: own rows + grants. The kernel's call gate still
        decides who may reach this agent at all.

        **Same tables as sqlite:** rows `(owner, id)`, fold_refs, grants (FK cascade on
        their anchor row), a text index. A durable store answers the same verbs.

        """
}
