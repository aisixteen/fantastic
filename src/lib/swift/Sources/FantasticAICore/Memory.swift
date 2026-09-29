// THE MEMORY STORE — this bundle owns no rows and no files.
//
// Port of python `ai_core/core.py` (canonical): `_mem`, `MemoryStoreError`,
// `_store_of`, `_store_call`, `_head`, `_refresh_head`, `_load_history`,
// `_save_history`, `_store_reply`, `_load_reply`, `FORK_FRAMING`,
// `_render_shared`, `_expand_forks`, `_row_digest`, `_my_grants`,
// `_grant_covers`, `_announce_row`, `_save_view`, `_load_view`.
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
/// the cached grants, the row watchers — keyed by kernel, so two kernels in one
/// process never share them. Python `_mem(kernel)`.
final class AIMemoryState: @unchecked Sendable {
    let lock = NSLock()
    var heads: [String: [JSON]] = [:]
    var bodies: [String: OrderedDictionary<Int64, String>] = [:]
    var grants: [String: [JSON]] = [:]
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
}

/// The store holds this agent's conversation AND its view rows. The CONVERSATION a
/// reader sees is everything but the view (an allow-list of kinds to HIDE: a kind
/// forgotten here lands in the model's prompt). Python `_BOOKKEEPING`.
let BOOKKEEPING: Set<String> = ["view"]

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

/// ONE gated send to this agent's memory store, AS THIS AGENT — the store keys a
/// row by its writer, and this is this agent's own memory, even when the code runs
/// outside its own dispatch. A refusal or a failed dispatch is a
/// `MemoryStoreError` — the turn fails fast. Python `_store_call`.
@discardableResult
func storeCall(
    _ selfId: String, _ kernel: Kernel, _ payload: OrderedDictionary<String, JSON>
) async throws -> JSON {
    let sid = try storeOf(selfId, kernel)
    let out = await KernelTaskLocals.$currentSender.withValue(AgentId(selfId)) {
        await kernel.send([Item(AgentId(sid), .object(payload))])[0]
    }
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
/// A `fork` row (`fork_from`) stands where another owner's granted rows are READ
/// back each time, under the grant — so a revoked grant fails this read. Python
/// `_load_history`.
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
    if rows.contains(where: { $0["kind"].asString == "fork" }) {
        rows = try await expandForks(selfId, kernel, rows)
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

// ─── sharing: fork rows, grants, and the row event ───────────────────────
//
// CONTEXT IS SHARED BY GRANT, IN THE STORE. A model shares a scope of its OWN rows
// with another agent by calling its store's `share` (an explicit choice,
// recorded); the reader starts from it with `fork_from`, which appends ONE `fork`
// row naming the grant. That row is rendered, every time the conversation is
// read, as the owner's rows READ BACK UNDER THE GRANT — quoted, attributed,
// read-only, never the reader's own turns. A revoked or pruned grant fails that
// read: fail fast.

/// Python `FORK_FRAMING`.
func forkFraming(owner: String, asOf: String) -> String {
    "Above is \(owner)'s thread as of row \(asOf), shared read-only. This is a fork: "
        + "you are a new agent, not \(owner)."
}

/// The owner's granted rows as ONE quoted block. A row that is itself a `fork` in
/// the owner's thread came from a THIRD agent's grant the reader does not hold:
/// it stands as a stub in its place — position kept, no content. Python
/// `_render_shared`.
func renderShared(owner: String, asOf: JSON, rows: [JSON]) -> String {
    var lines = ["[shared from \(owner) \u{b7} read-only \u{b7} as of row \(pyStr(asOf))]"]
    for r in rows {
        if r["kind"].asString == "view" { continue }
        if r["kind"].asString == "fork" {
            // THE THIRD OWNER'S OWN ROW ID — the newest row it shared (`as_of`),
            // not the fork row's id in the middle owner's thread: `request_share
            // {row}` goes to that owner, and it can only share a row that is its
            // own.
            lines.append(
                "[row \(pyStr(r["as_of"])) \u{b7} owner \(pyStr(r["owner"])) \u{b7} not shared with you]")
            continue
        }
        let c = r["content"]
        let text = c.asString ?? pyJsonDumps(c)
        let role = pyTruthy(r["role"]) ? pyStr(r["role"]) : "user"
        lines.append("(\(role) \u{b7} row \(pyStr(r["id"]))) \(text)")
    }
    lines.append("[end of \(owner)'s shared rows]")
    return lines.joined(separator: "\n")
}

/// Every `fork` row read back under its grant. Python `_expand_forks`.
func expandForks(_ selfId: String, _ kernel: Kernel, _ rows: [JSON]) async throws -> [JSON] {
    var out: [JSON] = []
    for r in rows {
        guard r["kind"].asString == "fork" else {
            out.append(r)
            continue
        }
        let res = try await storeCall(
            selfId, kernel, ["type": .string("read"), "grant": r["grant"]])
        let shared = res["rows"].asArray ?? []
        let content = pyTruthy(r["content"]) ? pyStr(r["content"]) : ""
        let head = content.split(separator: "\n", maxSplits: 1, omittingEmptySubsequences: false)
            .first.map(String.init) ?? ""
        out.append(
            withField(
                r, "content",
                .string(
                    head + "\n"
                        + renderShared(owner: pyStr(r["owner"]), asOf: r["as_of"], rows: shared))))
    }
    return out
}

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

/// The grants this agent GAVE, cached write-through (refreshed after the model
/// calls its store's `share`/`unshare`). Python `_my_grants`.
@discardableResult
func myGrants(_ selfId: String, _ kernel: Kernel) async throws -> [JSON] {
    let mem = AIMemory.of(kernel)
    if let g = mem.with({ $0.grants[selfId] }) { return g }
    let res = try await storeCall(
        selfId, kernel, ["type": .string("grants"), "side": .string("given")])
    let g = res["grants"].asArray ?? []
    mem.with { $0.grants[selfId] = g }
    return g
}

/// Drop the cached grants — the next body event re-reads them.
func forgetGrants(_ selfId: String, _ kernel: Kernel) {
    _ = AIMemory.of(kernel).with { $0.grants.removeValue(forKey: selfId) }
}

/// Does grant `g` cover row `rid`? Python `_grant_covers`.
func grantCovers(_ g: JSON, _ rid: Int64, _ head: [JSON]) -> Bool {
    let scope = g["scope"]
    let live = g["mode"].asString == "live"
    if !live, let asOf = g["as_of"].asInt, rid > asOf { return false }
    guard let sc = scope.asObject else { return false }
    if sc["row"] != nil { return rid == scope["row"].asInt }
    if sc["range"] != nil {
        return rid >= (scope["range"]["from"].asInt ?? 0)
            && (live || rid <= (scope["range"]["to"].asInt ?? 0))
    }
    if sc["head_until"] != nil { return live || rid <= (scope["head_until"].asInt ?? 0) }
    if sc["fold"] != nil {
        var byId: [Int64: JSON] = [:]
        for r in head { if let id = rowId(r) { byId[id] = r } }
        var seen: Set<Int64> = []
        var stack = [scope["fold"].asInt ?? 0]
        while let x = stack.popLast() {
            if seen.contains(x) { continue }
            seen.insert(x)
            stack.append(contentsOf: (byId[x]?["refs"].asArray ?? []).compactMap { $0.asInt })
        }
        return seen.contains(rid)
    }
    return false
}

/// Put the row event in every watcher's inbox: a content-free digest, plus the
/// content for a `body:true` watcher that holds a live grant covering the row
/// (the grant check uses the cached grants — see `myGrants`). A watcher whose
/// inbox is gone is dropped. Python `_announce_row`.
func announceRow(_ selfId: String, _ kernel: Kernel, _ row: JSON) {
    let mem = AIMemory.of(kernel)
    let (ws, grants, head) = mem.with { m in
        (m.watchers[selfId] ?? [:], m.grants[selfId] ?? [], m.heads[selfId] ?? [])
    }
    if ws.isEmpty { return }
    for (wid, body) in ws {
        guard kernel.hasInbox(AgentId(wid)) else {
            _ = mem.with { $0.watchers[selfId]?.removeValue(forKey: wid) }
            continue
        }
        var digest = memoryRowDigest(row)
        if body, let rid = rowId(row),
            grants.contains(where: { $0["reader"].asString == wid && grantCovers($0, rid, head) })
        {
            digest["content"] = row["content"]
        }
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
        m.grants.removeValue(forKey: selfId)
    }
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
