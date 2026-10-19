// THE MEMORY STORE — this bundle owns no rows and no files.
//
// Port of python `ai_core/core.py` (canonical): `_mem`, `MemoryStoreError`,
// `_store_of`, `_store_call`, `_head`, `_refresh_head`, `_load_history`,
// `_save_history`, `_store_reply`, `_load_reply`, `_row_digest`,
// `_announce_row`, `_save_view`, `_load_view`.
//
// An AI agent's conversation lives in the MEMORY STORE its record names
// (`memory_store_id`) — a separate agent (`ram_store`, or a durable one) that keys
// every row by WHO WROTE IT. So this agent reads and writes ITS OWN rows only, by
// plain gated sends: the creator wires `can_call` to the store and the store's
// `callable_by` to this agent, explicitly. No store named ⇒ the agent fails loud
// at boot and at its first turn; a store that refuses or is unreachable mid-turn
// ⇒ the turn fails fast. Nothing falls back to RAM.
//
// THE HEAD CACHE. The rows from the last `clear` on (the head) are read ONCE, on
// the first turn after boot, and then kept in RAM WRITE-THROUGH: every row is
// appended to the store FIRST and enters the cache only when the store took it, so
// the cache never holds what the store does not. Each turn-start read of the head
// also reads the rows AFTER the newest cached id and merges them by id: the store
// is passive, and a row another writer appended under this key (an admin plant)
// must reach the next turn. Reads that reach behind the head (recall, grep,
// `history {fold}`) go to the store.
//
// BODIES ARE NEVER PERSISTED. A reply too large to inline is kept in RAM under its
// pid for this incarnation only; what the store keeps is the reply's `origin` line
// (how to get it again). After a restart there are no bodies — the model re-reads.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// The agent's memory store is missing, refused, or unreachable — fail fast.
/// Python `MemoryStoreError`.
public struct MemoryStoreError: Error, CustomStringConvertible, Sendable {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}

/// This bundle's RAM for ONE kernel — the head caches, the bodies held this run,
/// the row watchers — keyed by kernel, so two kernels in one
/// process never share them. Python `_mem(kernel)`.
final class AIMemoryState: @unchecked Sendable {
    let lock = NSLock()
    var heads: [String: [JSON]] = [:]
    var bodies: [String: OrderedDictionary<Int64, String>] = [:]
    /// agent → (watcher id → wants bodies), in registration order.
    var watchers: [String: OrderedDictionary<String, Bool>] = [:]

    func with<T>(_ body: (AIMemoryState) -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body(self)
    }
}

enum AIMemory {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var states: [UInt64: AIMemoryState] = [:]

    static func of(_ kernel: Kernel) -> AIMemoryState {
        lock.lock()
        defer { lock.unlock() }
        let key = kernel.serial
        if let s = states[key] { return s }
        let s = AIMemoryState()
        states[key] = s
        return s
    }

    /// Drop a dead kernel's memory — see `AISideTables`.
    static func forget(_ kernel: Kernel) {
        lock.lock()
        states.removeValue(forKey: kernel.serial)
        lock.unlock()
    }

    /// Drop a deleted agent's head, bodies and watchers in this kernel
    /// (`AIBackend.onDelete`).
    static func forget(_ kernel: Kernel, _ id: String) {
        of(kernel).with { m in
            m.heads[id] = nil
            m.bodies[id] = nil
            m.watchers[id] = nil
        }
    }
}

/// The store holds this agent's conversation AND its view rows. The CONVERSATION a
/// reader sees is everything but the view and the recorded fold anchors (an
/// allow-list of kinds to HIDE: a kind forgotten here lands in the model's prompt).
/// Python `_BOOKKEEPING`.
let BOOKKEEPING: Set<String> = ["view", "anchors"]

/// The conversation = rows with a chat `role`. Python `_TURN_ROLES`.
let TURN_ROLES: Set<String> = ["user", "assistant", "system"]

/// The store `self_id`'s record names, or the loud refusal. Python `_store_of`.
func storeOf(_ selfId: String, _ kernel: Kernel) throws -> String {
    let sid = kernel.agent(AgentId(selfId))?.metaValue(forKey: "memory_store_id")
    guard case .string(let s)? = sid, !s.isEmpty else {
        throw MemoryStoreError(
            "\(selfId): no `memory_store_id` — an AI agent keeps its conversation "
                + "in a memory store and runs without none. Create one (e.g. a ram_store), "
                + "set memory_store_id on \(selfId), and grant \(selfId) can_call on it "
                + "AND the store callable_by for it")
    }
    return s
}

/// ONE gated send to this agent's memory store, as this agent — this code runs in
/// its own dispatch or a task it spawned, so the send is its own (the store keys
/// a row by its writer). A refusal or a failed dispatch is a `MemoryStoreError` —
/// the turn fails fast. Python `_store_call`.
@discardableResult
func storeCall(
    _ selfId: String, _ kernel: Kernel, _ payload: OrderedDictionary<String, JSON>
) async throws -> JSON {
    let sid = try storeOf(selfId, kernel)
    let out = await kernel.send([Item(AgentId(sid), .object(payload))])[0]
    let res = out.result
    if out.status != .completed || (res.asObject != nil && pyTruthy(res["error"])) {
        let err: String = res.asObject != nil ? pyStr(res["error"]) : out.status.rawValue
        let hint: JSON = res.asObject != nil ? res["hint"] : .null
        throw MemoryStoreError(
            "\(selfId): memory store \(pyRepr(sid)) \(pyStr(payload["type"] ?? .null)) failed: \(err)"
                + (pyTruthy(hint) ? " (hint: \(pyStr(hint)))" : ""))
    }
    return res.asObject != nil ? res : .object([:])
}

/// A row as the head cache keeps it: the store's row with `agent` = this agent.
private func owned(_ r: JSON, _ selfId: String) -> JSON {
    withField(r, "agent", .string(selfId))
}

/// The head cache — loaded from the store on first use (lazy boot). Python
/// `_head`.
func loadHead(_ selfId: String, _ kernel: Kernel) async throws -> [JSON] {
    let mem = AIMemory.of(kernel)
    if let h = mem.with({ $0.heads[selfId] }) { return h }
    let res = try await storeCall(
        selfId, kernel, ["type": .string("read"), "from_last_kind": .string("clear")])
    let rows = (res["rows"].asArray ?? []).map { owned($0, selfId) }
    return mem.with { m in
        if let h = m.heads[selfId] { return h }
        m.heads[selfId] = rows
        return rows
    }
}

/// THE STORE IS THE TRUTH; the head cache must never diverge from it. The cache
/// holds this agent's own writes (write-through), but another writer may append
/// under this key too. So each read of the head first reads every row AFTER the
/// newest cached id and merges it by id (a row the agent wrote meanwhile is
/// already there). A `clear` among them restarts the head. Python `_refresh_head`.
func refreshHead(_ selfId: String, _ kernel: Kernel) async throws -> [JSON] {
    let mem = AIMemory.of(kernel)
    let cached = mem.with { $0.heads[selfId] }
    let ids = (cached ?? []).compactMap { rowId($0) }
    guard cached != nil, let top = ids.max() else {
        _ = mem.with { $0.heads.removeValue(forKey: selfId) }
        return try await loadHead(selfId, kernel)
    }
    let res = try await storeCall(
        selfId, kernel, ["type": .string("read"), "after": .integer(top)])
    return mem.with { m in
        var h = m.heads[selfId] ?? []
        var seen = Set(h.compactMap { rowId($0) })
        for r in res["rows"].asArray ?? [] {
            if let rid = rowId(r), seen.contains(rid) { continue }
            if r["kind"].asString == "clear" { h.removeAll() }
            h.append(owned(r, selfId))
            if let rid = rowId(r) { seen.insert(rid) }
        }
        m.heads[selfId] = h
        return h
    }
}

/// The agent's conversation, in order. AGENT ID IS CONVERSATION ID: a second
/// conversation is a second agent.
///
/// A CLEAR IS A BOUNDARY, NOT A DELETION (`context_clear`): the head starts at the
/// LAST `clear` row. `behind: true` (recall, grep, `history {fold}` — the verbs
/// that reach what is not in view) reads the WHOLE conversation from the store.
///
/// THE CONVERSATION IS ITS TURNS — rows with a chat `role`. A row the agent
/// `append`s to its own store without one (a note to itself) is memory, not a
/// turn: it stays in the store, `grep` finds it, the view never renders it.
///
/// Python `_load_history`.
func loadHistory(_ selfId: String, _ kernel: Kernel, behind: Bool = false) async throws -> [JSON] {
    var rows: [JSON]
    if behind {
        let res = try await storeCall(selfId, kernel, ["type": .string("read")])
        rows = (res["rows"].asArray ?? []).map { owned($0, selfId) }
    } else {
        rows = try await refreshHead(selfId, kernel)
    }
    rows = rows.filter { r in
        let kind = r["kind"].asString
        if let kind, BOOKKEEPING.contains(kind) { return false }
        guard let role = r["role"].asString else { return false }
        return TURN_ROLES.contains(role)
    }
    return rows
}

/// APPEND rows to the memory store, then to the head cache (write-through). A row
/// without an id takes the kernel's next one. A `clear` restarts the head.
/// Python `_save_history`.
func saveHistory(_ selfId: String, _ kernel: Kernel, _ newTurns: [JSON]) async throws {
    var rows: [JSON] = []
    for t in newTurns {
        var r = t.asObject ?? [:]
        r.removeValue(forKey: "agent")
        if r["id"] == nil || r["id"] == .null { r["id"] = .integer(kernel.mintId()) }
        rows.append(.object(r))
    }
    if rows.isEmpty { return }
    try await storeCall(selfId, kernel, ["type": .string("append"), "rows": .array(rows)])
    let mem = AIMemory.of(kernel)
    var announce: [JSON] = []
    mem.with { m in
        let cached = m.heads[selfId] != nil
        for r in rows {
            let stored = owned(r, selfId)
            if cached {
                if r["kind"].asString == "clear" { m.heads[selfId] = [] }
                m.heads[selfId]?.append(stored)
            }
            if !BOOKKEEPING.contains(r["kind"].asString ?? "") { announce.append(stored) }
        }
    }
    for s in announce { announceRow(selfId, kernel, s) }
}

/// A reply too big to hand the model as a turn: kept IN RAM under its pid (never
/// persisted); the model is handed its digest + `origin` line. Python
/// `_store_reply`.
@discardableResult
func storeReply(_ selfId: String, _ kernel: Kernel, pid: Int64, body: String) -> Bool {
    AIMemory.of(kernel).with { $0.bodies[selfId, default: [:]][pid] = body }
    return true
}

/// The bodies held in RAM this run for `selfId`, in the order they arrived.
func heldBodies(_ selfId: String, _ kernel: Kernel) -> OrderedDictionary<Int64, String> {
    AIMemory.of(kernel).with { $0.bodies[selfId] ?? [:] }
}

// ─── the row event ───────────────────────────────────────────────

/// ROW EVENT digest — content-free. Python `_row_digest`.
func memoryRowDigest(_ row: JSON) -> OrderedDictionary<String, JSON> {
    let c = row["content"]
    let text = c.asString ?? pyJsonDumps(c)
    return [
        "id": row["id"],
        "kind": pyTruthy(row["kind"]) ? row["kind"] : row["role"],
        "role": row["role"],
        "bytes": .integer(Int64(text.unicodeScalars.count)),
    ]
}

/// Put the row event in every watcher's inbox: a content-free digest, plus the
/// content for a `body:true` watcher. A watcher whose inbox is gone is dropped.
/// Python `_announce_row`.
func announceRow(_ selfId: String, _ kernel: Kernel, _ row: JSON) {
    let mem = AIMemory.of(kernel)
    let ws = mem.with { $0.watchers[selfId] ?? [:] }
    if ws.isEmpty { return }
    for (wid, body) in ws {
        guard kernel.hasInbox(AgentId(wid)) else {
            _ = mem.with { $0.watchers[selfId]?.removeValue(forKey: wid) }
            continue
        }
        var digest = memoryRowDigest(row)
        if body { digest["content"] = row["content"] }
        let ev: JSON = .object([
            "type": .string("row"), "source": .string(selfId), "row": .object(digest),
        ])
        kernel.deliverToInbox(AgentId(wid), .text(ev))
    }
}

/// Register / drop a row watcher.
func setWatcher(_ selfId: String, _ kernel: Kernel, _ watcher: String, body: Bool?) {
    AIMemory.of(kernel).with { m in
        if let body {
            m.watchers[selfId, default: [:]][watcher] = body
        } else {
            m.watchers[selfId]?.removeValue(forKey: watcher)
        }
    }
}

/// A new incarnation reads its store afresh (`boot`); bodies are this run's only.
func forgetHead(_ selfId: String, _ kernel: Kernel) {
    AIMemory.of(kernel).with { m in
        m.heads.removeValue(forKey: selfId)
    }
}

// ─── the `rows` verb ────────────────────────────────────────────────

/// The `rows` verb: a VIEW of this agent's conversation exactly as the agent HOLDS
/// it in memory — its loaded head, in the order it lived it; no store read, no
/// transform, so what a UI draws is what the agent has. Before the agent has
/// loaded its head (a fresh boot, before its first turn) it holds nothing:
/// `{rows:[], loaded:false}`. `after` (a row id) starts the page after that row —
/// an id not in the head is an empty page; `limit` (a positive int) caps it;
/// `next` is the last row's id when more follow. Python `_rows`.
func rowsVerb(_ selfId: String, _ payload: JSON, _ kernel: Kernel) -> JSON {
    guard let head = AIMemory.of(kernel).with({ $0.heads[selfId] }) else {
        return .object([
            "rows": .array([]), "next": .null, "total": .integer(0), "loaded": .bool(false),
        ])
    }
    var rows = head
    // `after is not None` — then python `==` against each row's `id`.
    let after = payload["after"]
    if after != .null {
        let at = rows.firstIndex { pyEquals($0["id"], after) }
        rows = at.map { Array(rows[($0 + 1)...]) } ?? []
    }
    // `isinstance(limit, int) and limit > 0` — a bool IS an int in python.
    let limit: Int64? =
        switch payload["limit"] {
        case .integer(let n) where n > 0: n
        case .bool(true): 1
        default: nil
        }
    let page = limit.map { Array(rows.prefix(Int(clamping: $0))) } ?? rows
    let more = page.count < rows.count
    return .object([
        "rows": .array(page),
        "next": more && !page.isEmpty ? page[page.count - 1]["id"] : .null,
        "total": .integer(Int64(head.count)),
        "loaded": .bool(true),
    ])
}

/// Python `==` between two JSON values: `1 == 1.0 == True`; anything else equal
/// only as itself.
private func pyEquals(_ a: JSON, _ b: JSON) -> Bool {
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

// ─── the view row ───────────────────────────────────────────────────

/// The agent's STATE: the ordered ids it sees, each at a resolution — a `view`
/// row in its memory store; the newest one in the head is current. Python
/// `_save_view` — the doc is `{kind, v, ts, budget, why, ids}`.
func saveView(_ selfId: String, _ kernel: Kernel, ids: [JSON], budget: Int64, why: String) async throws {
    try await saveHistory(
        selfId, kernel,
        [
            .object([
                "kind": .string("view"),
                "v": .integer(1),
                "ts": .double(Date().timeIntervalSince1970),
                "budget": .integer(budget),
                "why": .string(why),
                "ids": .array(ids),
            ])
        ])
}

/// The current view row, when one has been written and carries an `ids` list.
/// Python `_load_view`.
func loadView(_ selfId: String, _ kernel: Kernel) async throws -> JSON? {
    for r in try await loadHead(selfId, kernel).reversed() where r["kind"].asString == "view" {
        return r["ids"].asArray != nil ? r : nil
    }
    return nil
}
