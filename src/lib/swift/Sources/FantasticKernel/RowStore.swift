// THE MESSAGE STORE — every message the kernel ever made, id-addressed by the
// ONE counter.
//
// Conversations, reply bodies, agent views and the kernel's own `call`/`task`/
// `settle` bookkeeping share one append-only table, so one stream carries
// everything. Starts EMPTY, always: a hydrator fills it from above and a
// persister tails it; the kernel never knows disk.
//
// ONE COUNTER MINTS EVERYTHING. A row id, a pid, a fold, a body all take the
// next int, so one number names one thing everywhere — `ps`, `cancel`, the
// ledger, a reply tag and the message log agree by construction. Python
// `kernel/_kernel.py` (`mint_id`, `rows_put`, `rows_for`, `body_put`/`body_get`,
// `view_put`/`view_get`, `subscribe`), rust `fantastic-kernel/src/rows.rs`.
//
// ONE LOCK, and minting an id, inserting the row and updating the three indexes
// happen under it as a single step — the house style `Kernel` uses.

import FantasticJSON
import Foundation
import OrderedCollections

/// One stored message, as the store keeps it: the caller's fields plus `id`,
/// `agent` and `ts`.
public typealias Row = OrderedDictionary<String, JSON>

/// THE TAIL — every row with `id > after` in id order, then every row as it is
/// appended. Replay then live, deduped by id, so a subscriber that opens at the
/// id it last saw misses nothing and doubles nothing. Carries row IDS, not
/// snapshots: a snapshot would lose the outcome stamped onto a `call` row after
/// it was queued.
public struct RowTail: Sendable {
    /// The ids, replay first.
    public let ids: AsyncStream<Int64>
    let continuation: AsyncStream<Int64>.Continuation
}

public final class RowStore: @unchecked Sendable {
    private let lock = NSLock()
    private var nextIdValue: Int64 = 1
    private var rows: [Int64: Row] = [:]
    /// Insertion order — what `forAgent` and a replay walk. `rows` is a plain
    /// dictionary, so the order lives here.
    private var order: [Int64] = []
    private var byAgent: [AgentId: [Int64]] = [:]
    private var bodyByParent: [Int64: Int64] = [:]
    private var viewByAgent: [AgentId: Int64] = [:]
    private var tails: [AsyncStream<Int64>.Continuation] = []

    public init() {}

    // MARK: the counter

    /// The id the store will mint NEXT.
    public var nextId: Int64 {
        lock.lock()
        defer { lock.unlock() }
        return nextIdValue
    }

    /// Hydration raises the counter above every id it read; it NEVER lowers it.
    public func setNextId(_ n: Int64) {
        lock.lock()
        defer { lock.unlock() }
        nextIdValue = max(nextIdValue, n)
    }

    /// Mint one id — a pid, a row, a fold, a body. The only allocator.
    public func mintId() -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        let n = nextIdValue
        nextIdValue = n + 1
        return n
    }

    // MARK: rows

    /// Append one message for `agent`; mint its id when absent, accept a
    /// PRE-MINTED unused one, refuse anything else — the store is the only
    /// allocator. Returns the id.
    @discardableResult
    public func put(_ agent: AgentId, _ row: Row) throws -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        let rid: Int64
        switch row["id"] {
        case nil, .some(.null):
            rid = nextIdValue
            nextIdValue = rid + 1
        case .some(let v):
            guard case .integer(let n) = v, n > 0, n < nextIdValue, rows[n] == nil else {
                throw RowStoreError.badId(
                    "rows_put: id must be absent or a pre-minted unused id, got \(v.serialize())")
            }
            rid = n
        }
        var stored = row
        stored["id"] = .integer(rid)
        stored["agent"] = .string(agent.value)
        if stored["ts"] == nil {
            stored["ts"] = .double(Date().timeIntervalSince1970)
        }
        switch stored["kind"]?.asString {
        case "body":
            if case .integer(let p)? = stored["parent"] {
                bodyByParent[p] = rid
            }
        case "view":
            viewByAgent[agent] = rid
        default:
            break
        }
        rows[rid] = stored
        order.append(rid)
        byAgent[agent, default: []].append(rid)
        tails.removeAll { c in
            if case .terminated = c.yield(rid) { return true }
            return false
        }
        return rid
    }

    /// One agent's rows, oldest first. The list a reader treats as the
    /// CONVERSATION once the bookkeeping kinds are filtered out.
    public func forAgent(_ agent: AgentId) -> [Row] {
        lock.lock()
        defer { lock.unlock() }
        return (byAgent[agent] ?? []).compactMap { rows[$0] }
    }

    /// Rows by id, in the order asked; unknown ids are skipped.
    public func get(_ ids: [Int64]) -> [Row] {
        lock.lock()
        defer { lock.unlock() }
        return ids.compactMap { rows[$0] }
    }

    public func getOne(_ id: Int64) -> Row? {
        lock.lock()
        defer { lock.unlock() }
        return rows[id]
    }

    /// Merge `stamp` onto a row in place — how a `call` row learns its outcome.
    /// NO re-notify: the tail carries ids, and a reader that wants the stamped
    /// row reads it by id.
    public func stamp(_ id: Int64, _ stamp: Row) {
        lock.lock()
        defer { lock.unlock() }
        guard var row = rows[id] else { return }
        for (k, v) in stamp { row[k] = v }
        rows[id] = row
    }

    // MARK: bodies

    /// Keep a reply body too large to inline, under its pid.
    @discardableResult
    public func bodyPut(_ agent: AgentId, pid: Int64, text: String) throws -> Int64 {
        try put(agent, ["kind": .string("body"), "parent": .integer(pid), "content": .string(text)])
    }

    /// One page of a stored body: `(text, nextOffset, eof, size)`. CHARACTERS,
    /// not bytes: python slices a str, and a byte slice would cut a codepoint in
    /// half on any non-ascii body.
    public func bodyGet(_ pid: Int64, offset: Int, length: Int) -> (String, Int, Bool, Int)? {
        lock.lock()
        defer { lock.unlock() }
        guard let rid = bodyByParent[pid], let row = rows[rid] else { return nil }
        let chars = Array(row["content"]?.asString ?? "")
        let start = min(max(offset, 0), chars.count)
        let end = min(start + max(length, 1), chars.count)
        let next = end
        return (String(chars[start..<end]), next, next >= chars.count, chars.count)
    }

    // MARK: views

    /// The agent's VIEW as a `view` row; the newest one is current.
    @discardableResult
    public func viewPut(_ agent: AgentId, _ doc: Row) throws -> Int64 {
        var row = doc
        row["kind"] = .string("view")
        return try put(agent, row)
    }

    /// The current view, without the row's own bookkeeping.
    public func viewGet(_ agent: AgentId) -> Row? {
        lock.lock()
        defer { lock.unlock() }
        guard let rid = viewByAgent[agent], let row = rows[rid] else { return nil }
        var out: Row = [:]
        for (k, v) in row where !["kind", "agent", "id", "parent"].contains(k) {
            out[k] = v
        }
        return out
    }

    // MARK: the tail

    /// Registering and snapshotting happen under ONE lock acquisition, so no row
    /// can land in neither half. UNBOUNDED buffering: a dropping policy would
    /// lose rows from the durable log under a burst, with no error.
    public func subscribe(after: Int64) -> RowTail {
        lock.lock()
        defer { lock.unlock() }
        let (stream, continuation) = AsyncStream<Int64>.makeStream(bufferingPolicy: .unbounded)
        for id in order where id > after {
            continuation.yield(id)
        }
        tails.append(continuation)
        return RowTail(ids: stream, continuation: continuation)
    }
}

/// The one way `put` refuses.
public enum RowStoreError: Error, CustomStringConvertible {
    case badId(String)
    public var description: String {
        switch self {
        case .badId(let s): return s
        }
    }
}
