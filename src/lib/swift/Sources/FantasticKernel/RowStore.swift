// THE EXECUTION ROWS — the `call`, `settle`, `task`, `ack` rows the kernel minted
// and has NOT YET FLUSHED to its memory store, id-addressed.
//
// The kernel keeps NO conversation: an AI agent's rows live in that agent's
// memory store. Once a row is flushed (`Kernel.flushExecutions`) it leaves RAM; a
// kernel that was never started keeps them here until the process ends. Python
// `kernel/_kernel.py` (`rows`, `rows_put`, `rows_stream`), which is canonical.
//
// ONE COUNTER MINTS EVERYTHING — but the counter is the KERNEL's
// (`Kernel.mintId`), not this table's: it is persisted by block reservation in
// the root record, and only the kernel can write that record. This table holds
// rows; `Kernel.rowsPut` is the one way in.

import FantasticJSON
import Foundation
import OrderedCollections

/// One stored row, as the table keeps it: the caller's fields plus `id`, `agent`
/// and `ts`.
public typealias Row = OrderedDictionary<String, JSON>

/// THE TAIL — every row still in RAM with `id > after` in id order, then every
/// row as it is appended. Replay then live, deduped by id. Carries row IDS, not
/// snapshots: a snapshot would lose the outcome stamped onto a `call` row after it
/// was queued. Yielding is not a message: no send, no task, no row.
public struct RowTail: Sendable {
    /// The ids, replay first.
    public let ids: AsyncStream<Int64>
    let continuation: AsyncStream<Int64>.Continuation
}

public final class RowStore: @unchecked Sendable {
    private let lock = NSLock()
    private var rows: [Int64: Row] = [:]
    private var tails: [UUID: AsyncStream<Int64>.Continuation] = [:]

    public init() {}

    /// Insert one row under its (already validated) id. `false` when the id is
    /// taken — the caller refuses.
    func insert(_ id: Int64, _ row: Row) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if rows[id] != nil { return false }
        rows[id] = row
        for (k, c) in tails {
            if case .terminated = c.yield(id) { tails.removeValue(forKey: k) }
        }
        return true
    }

    /// Is `id` a row still in RAM?
    public func contains(_ id: Int64) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return rows[id] != nil
    }

    public func getOne(_ id: Int64) -> Row? {
        lock.lock()
        defer { lock.unlock() }
        return rows[id]
    }

    /// Every row still in RAM, in ID order.
    public func all() -> [Row] {
        lock.lock()
        defer { lock.unlock() }
        return rows.keys.sorted().compactMap { rows[$0] }
    }

    /// The ids still in RAM, ascending.
    public func ids() -> [Int64] {
        lock.lock()
        defer { lock.unlock() }
        return rows.keys.sorted()
    }

    /// How many rows are still in RAM.
    public var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return rows.count
    }

    /// Drop one row (it was flushed). Returns it.
    @discardableResult
    func remove(_ id: Int64) -> Row? {
        lock.lock()
        defer { lock.unlock() }
        return rows.removeValue(forKey: id)
    }

    /// Merge `stamp` onto a row in place — how a live `call` row learns its
    /// attempt count and its outcome while it is still in RAM. A row already
    /// flushed is gone, and the stamp is simply not needed there: the settle row
    /// is the record of how it ended.
    public func stamp(_ id: Int64, _ stamp: Row) {
        lock.lock()
        defer { lock.unlock() }
        guard var row = rows[id] else { return }
        for (k, v) in stamp { row[k] = v }
        rows[id] = row
    }

    /// THE TAIL. Registering and snapshotting happen under ONE lock acquisition,
    /// so no row can land in neither half. UNBOUNDED buffering: a dropping policy
    /// would lose rows under a burst, with no error.
    public func subscribe(after: Int64) -> RowTail {
        lock.lock()
        defer { lock.unlock() }
        let (stream, continuation) = AsyncStream<Int64>.makeStream(bufferingPolicy: .unbounded)
        for id in rows.keys.sorted() where id > after {
            continuation.yield(id)
        }
        let key = UUID()
        tails[key] = continuation
        // A consumer that stopped is DROPPED, not left buffering forever
        // (python's `finally: self._tails.remove(q)`).
        continuation.onTermination = { [weak self] _ in self?.dropTail(key) }
        return RowTail(ids: stream, continuation: continuation)
    }

    private func dropTail(_ key: UUID) {
        lock.lock()
        defer { lock.unlock() }
        tails.removeValue(forKey: key)
    }

    /// How many tails are registered — a consumer that stopped is dropped at the
    /// next append (python asserts `k._tails == []`).
    public var tailCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return tails.count
    }
}

/// The one way `rowsPut` refuses.
public enum RowStoreError: Error, CustomStringConvertible {
    case badId(String)
    public var description: String {
        switch self {
        case .badId(let s): return s
        }
    }
}
