// THE KERNEL'S MEMORY STORE — the id counter, the execution rows' way in, the
// start sequence, the flusher, the store-side retention window, the `ledger`
// projection, named-call recall and the dead letters.
//
// Port of python `kernel/_kernel.py` (canonical): `mint_id`, `rows_put`,
// `start`, `_flusher`, `_is_own_flush_row`, `flush_executions`,
// `prune_executions`, `_store_read`, `ledger_entries`, `recall_executions`,
// `announce_deadletter`, `stored_outcome`.
//
// THE KERNEL KEEPS NO CONVERSATION and writes no tape. Its execution rows (`call`,
// `settle`, `task`, `ack`) live in RAM until the FLUSHER — kernel code, not the
// loader — writes them to the root's memory store (`memory_store_id`) and drops
// them. `kernel_start` refuses to run without that store, so the kernel owns
// writing to it.

import FantasticJSON
import Foundation
import OrderedCollections

/// THE DEAD ENDINGS. An execution that settled one of these is a DEAD LETTER: it
/// is announced (`on_deadletter`), listed by `ledger`, never retried silently,
/// and retired by `ack_deadletter`. Python `DEAD_STATUSES`.
public let DEAD_STATUSES: Set<String> = ["failed", "timed_out", "cancelled"]

/// ONE WRITE AT A TIME. A FIFO async mutex — swift has no built-in async lock.
public actor AsyncMutex {
    private var locked = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    public init() {}

    public func acquire() async {
        if !locked {
            locked = true
            return
        }
        await withCheckedContinuation { waiters.append($0) }
    }

    public func release() {
        if waiters.isEmpty {
            locked = false
        } else {
            waiters.removeFirst().resume()
        }
    }
}

/// `print(..., flush=True)` — stdout is block-buffered on a pipe, so a line the
/// operator is meant to see must not wait for the buffer. Routed through
/// `RuntimeLog.out` so a test can read what an operator would.
public func printFlushed(_ line: String) {
    RuntimeLog.out(line)
}

/// WHAT A SETTLE ROW KEEPS of an outcome — the one definition (decided
/// 2026-09-26). Python `stored_outcome`.
///
/// A NAMED call keeps its whole result: its name is a dedup contract, and a
/// re-send returns the recorded outcome (C9). A DEAD LETTER keeps its error text:
/// someone has to read why it died. Every other settle keeps a digest —
/// `{status, bytes}`.
///
/// Why not the body everywhere: the kernel's own store reads are executions too
/// (the `ledger` projection, every AI agent's head read), so a settle holding its
/// body re-recorded the store inside the store — measured ~6x growth per `ledger`
/// call (2 KB → 14.8 MB in five). And a reply body is not persisted anyway: its
/// receiver holds it in RAM with the one-line `origin` that gets it again.
///
/// `result` is the reply AS THE STORE HOLDS IT — raw bytes already reduced to
/// `{len: N}` (`Kernel.storeValue`).
public func storedOutcome(status: String, result: JSON, named: Bool) -> Row {
    if named {
        return ["status": .string(status), "result": result]
    }
    if DEAD_STATUSES.contains(status) {
        var kept: Row = ["status": .string(status)]
        if case .object(let res) = result {
            for k in ["error", "reason", "side", "cancelled", "timed_out"] {
                guard let v = res[k] else { continue }
                if case .string(let s) = v {
                    kept[k] = .string(String(String.UnicodeScalarView(s.unicodeScalars.prefix(500))))
                } else {
                    kept[k] = v
                }
            }
        }
        return kept
    }
    // `len(json.dumps(_store_safe(res), default=str))` — python's separators and
    // `ensure_ascii` escapes, counted in CODE POINTS.
    return ["status": .string(status), "bytes": .integer(Int64(pyJsonDumps(result).unicodeScalars.count))]
}

extension Kernel {
    // MARK: - the counter

    /// The next id this kernel will mint.
    public var nextId: Int64 {
        counterLock.lock()
        defer { counterLock.unlock() }
        return _nextId
    }

    /// The upper bound of the block reserved so far (python `id_reserved`).
    public var idReserved: Int64 {
        counterLock.lock()
        defer { counterLock.unlock() }
        return _idReserved
    }

    /// THE NEXT MESSAGE ID. The ONLY allocator in the kernel: a call, a forked
    /// task, a conversation row, a checkpoint all take their id here, so one
    /// number names one thing everywhere (`ps`, `cancel`, a reply tag, a store
    /// row). Reserves a new block in the root record when the old one is spent —
    /// written only by a STARTED kernel (python `mint_id`).
    public func mintId() -> Int64 {
        counterLock.lock()
        let n = _nextId
        _nextId = n + 1
        var reserved: Int64? = nil
        if n >= _idReserved {
            _idReserved = n + Kernel.ID_BLOCK
            reserved = _idReserved
        }
        counterLock.unlock()
        if let reserved, started, let r = root {
            // The reservation reaches the loader through its `updated` event —
            // except inside the kernel's own flush (`stateQuiet`), where NOTHING
            // publishes (CLI-B): the record holds the bound, the next published
            // update carries it.
            _ = update(r.id, ["next_id": .integer(reserved)])
        }
        return n
    }

    /// `load`: the root record's reserved bound is where this run starts, and this
    /// run's first block is reserved IN MEMORY (the record is untouched, so save →
    /// load → save round-trips). Python `Kernel.load`'s counter tail.
    func restoreCounter(from rootMeta: OrderedDictionary<String, JSON>) {
        counterLock.lock()
        defer { counterLock.unlock() }
        if case .integer(let nid)? = rootMeta["next_id"], nid > 0 {
            _nextId = max(_nextId, nid)
        }
        _idReserved = _nextId + Kernel.ID_BLOCK
    }

    // MARK: - the execution rows

    /// Append one EXECUTION row (`agent` = the agent it is about); mint its id
    /// when absent, accept a PRE-MINTED unused one, refuse anything else — the
    /// kernel is the only allocator. Bookkeeping from above, never gated. Wakes
    /// the flusher. Returns the id. Python `rows_put`.
    @discardableResult
    public func rowsPut(_ agent: AgentId, _ row: Row) throws -> Int64 {
        let rid: Int64
        switch row["id"] {
        case nil, .some(.null):
            rid = mintId()
        case .some(let v):
            guard case .integer(let n) = v, n > 0, n < nextId, !rows.contains(n) else {
                throw RowStoreError.badId(
                    "rows_put: id must be absent or a pre-minted unused id, got \(pyRepr(v))")
            }
            rid = n
        }
        var stored = row
        stored["id"] = .integer(rid)
        stored["agent"] = .string(agent.value)
        if stored["ts"] == nil {
            stored["ts"] = .double(Date().timeIntervalSince1970)
        }
        guard rows.insert(rid, stored) else {
            throw RowStoreError.badId(
                "rows_put: id must be absent or a pre-minted unused id, got \(rid)")
        }
        let wake = storeLock.withLock { _flushWake }
        wake?.yield()
        publishState(Self.rowEvent(agent, stored))
        return rid
    }

    /// What a row event carries of an EXECUTION row besides its id and kind —
    /// never a payload or an outcome: `parent` (the task, or the settled
    /// execution), `caller` (who made a call / an ack; null = the operator),
    /// `settled` (the agent a settle closes).
    static let rowEventKeys = ["parent", "caller", "settled"]

    /// The state event for an execution row just put (owner, 2026-10-08): a reader
    /// learns "row <id> arrived for <agent>" and fetches rows only when told — no
    /// polling. Content-free. Published like every state event (gated by
    /// `state_subscribe` on the root) and, like them, never from inside the
    /// kernel's own ledger flush, so an idle kernel publishes nothing. Python
    /// `row_event`.
    static func rowEvent(_ agent: AgentId, _ row: Row) -> JSON {
        var digest: OrderedDictionary<String, JSON> = [
            "id": row["id"] ?? .null, "kind": row["kind"] ?? .null,
        ]
        for k in rowEventKeys {
            if let v = row[k] { digest[k] = v }
        }
        return .object([
            "agent_id": .string(agent.value), "kind": .string("row"), "row": .object(digest),
        ])
    }

    /// python `_json_safe` of a reply whose raw bytes ride beside it (the
    /// outcome's body): the chunk reads `<bytes:N>` AT ITS SLOT, the telemetry
    /// rule. What the in-RAM `ledger` shows of a NAMED result before its settle
    /// row flushes as `{len: N}`.
    public static func jsonSafeValue(_ value: JSON, body: Blob?) -> JSON {
        withBody(value, body, .string("<bytes:\(body?.bytes.count ?? 0)>"))
    }

    /// A row value with RAW BYTES reduced to their LENGTH — THE ONE REDUCTION.
    /// A store holds no raw bytes and base64 is banned, so a reply's chunk is
    /// recorded as `{"len": N}` AT ITS SLOT — python reduces the bytes value it
    /// holds INSIDE the dict (`_store_safe`); swift carries those bytes on the
    /// binary side channel (`Outcome.body`) and puts the length back where they
    /// sat. A call's PAYLOAD body sits at its own path: `tapedPayload`.
    public static func storeValue(_ value: JSON, body: Blob?) -> JSON {
        withBody(value, body, .object(["len": .integer(Int64(body?.bytes.count ?? 0))]))
    }

    /// `value` with `stand` written at the body's slot — kept in place when the
    /// reply names it, appended when it does not (python's dict assignment). No
    /// body ⇒ the value verbatim; a slot the value cannot hold (a non-object
    /// reply) is not written. Rust `jsonsafe::with_body`.
    private static func withBody(_ value: JSON, _ body: Blob?, _ stand: JSON) -> JSON {
        guard let body else { return value }
        var out = value
        do {
            try pySetPath(&out, parts: body.parts, stand)
        } catch {
            return value
        }
        return out
    }

    /// `storeValue` for a call's PAYLOAD: its request body (`Item.binary`) is
    /// written as its LENGTH at the path it rides on — where python's own payload
    /// dict holds it, which python's `_store_safe` writes as `{len: N}` in place.
    /// Everything else — all text, all JSON — is untouched. Rust `taped_payload`.
    static func tapedPayload(_ value: JSON, _ body: Blob?) -> JSON {
        guard let body else { return value }
        var out = value
        // admitted at launch: the path names a slot of the payload
        try? pySetPath(&out, parts: body.parts, .object(["len": .integer(Int64(body.bytes.count))]))
        return out
    }

    // MARK: - the store state

    /// The memory store the kernel writes to, once started.
    public var storeId: AgentId? {
        storeLock.lock()
        defer { storeLock.unlock() }
        return _storeId
    }

    /// Has `kernel_start` run to the end?
    public var started: Bool {
        storeLock.lock()
        defer { storeLock.unlock() }
        return _started
    }

    /// Seconds of quiet before a batch is written (python `FLUSH_DEBOUNCE`).
    static let flushDebounceNanos: UInt64 = 150_000_000

    /// THE KERNEL'S OWN CALL to its memory store — bookkeeping, a plain call
    /// into the store's handler (`runHandler`), never a send: no gate, no rows
    /// (the store keys the kernel's rows as the operator's, `<guid>|kernel`). A
    /// store no longer in the tree is a refusal.
    func storeCall(_ store: AgentId, _ payload: JSON) async -> (JSON, Status) {
        guard let a = agent(store) else {
            return (Refusal.unknown(store.value, live: allAgents().map { $0.id.value }), .errored)
        }
        return await runHandler(a, payload)
    }

    /// `storeCall` over the BINARY channel: `body` the request's chunk at
    /// `bytes`, or `nil` to have the reply's chunk at `bytes` come back raw.
    func storeCallBinary(_ store: AgentId, _ header: JSON, _ body: Data?) async -> (JSON, Data?) {
        guard let a = agent(store) else {
            return (Refusal.unknown(store.value, live: allAgents().map { $0.id.value }), nil)
        }
        let (reply, chunk, _) = await runHandlerBinary(a, header, body.map { Blob("bytes", $0) })
        return (reply, bodyAt(chunk, "bytes"))
    }

    // MARK: - kernel_start

    /// THE START SEQUENCE — the `kernel_start` verb, and what the daemon runs.
    ///
    /// 1. The root record must name `memory_store_id`. None ⇒ FAIL LOUD: the kernel
    ///    keeps its execution rows (the ledger, dead letters, named-call dedup) in
    ///    a memory store and does not start without one. No built-in default.
    /// 2. Boot the root (it mints the guid every store key carries), then THAT
    ///    store — before anyone who might call it.
    /// 3. The store must serve the operator (the kernel writes as `kernel`) —
    ///    probed now, refused now, never discovered on the first flush.
    /// 4. Start flushing: every execution row minted so far and from now on is
    ///    written into the store and leaves RAM.
    /// 5. `bootAll`: boot every other agent in registration order (AI agents boot
    ///    lazily — they read their own store on their first turn).
    /// Idempotent: a second call reports the store it is already writing to.
    public func start(bootAll: Bool = true) async -> JSON {
        if started {
            return .object([
                "started": .bool(true), "already": .bool(true),
                "store": storeId.map { JSON.string($0.value) } ?? .null,
            ])
        }
        guard let root else {
            return .object(["error": .string("kernel: no root agent registered")])
        }
        guard case .string(let sid)? = root.metaValue(forKey: "memory_store_id"), !sid.isEmpty
        else {
            return Refusal.make(
                Refusal.missingArg,
                "kernel_start: the root record names no `memory_store_id` — the kernel "
                    + "keeps its execution rows (ledger, dead letters, named-call dedup) in a "
                    + "memory store and will not start without one",
                hint: "a memory store is needed — create a ram_store and name it in "
                    + "memory_store_id if you have none: `create_agent parent_id=\(root.id.value) "
                    + "handler_module=ram_store.tools id=memory`, then "
                    + "`update_agent id=\(root.id.value) memory_store_id=memory`",
                extra: [("arg", .string("memory_store_id"))])
        }
        // A LITERAL id (python `self.agents.get(sid)`): `kernel` is the send
        // address of the root, not an agent a store can be.
        guard let store = agent(AgentId(sid)) else {
            var r = Refusal.unknown(sid, live: allAgents().map { $0.id.value })
            if case .object(var o) = r {
                o["hint"] = .string("the root's `memory_store_id` names no agent in this tree")
                r = .object(o)
            }
            return r
        }
        var booted: [String] = []
        for a in [root, store] {
            let (reply, status) = await runHandler(a, .object(["type": .string("boot")]))
            if status != .completed {
                return Refusal.make(
                    Refusal.unhandled,
                    "kernel_start: boot of \(pyRepr(a.id.value)) did not complete "
                        + "(\(status.rawValue)): \(pyStr(reply))")
            }
            booted.append(a.id.value)
        }
        let (probe, _) = await runHandler(
            store, .object(["type": .string("read"), "limit": .integer(1)]))
        if case .object(let p) = probe, pyTruthy(p["error"] ?? .null) {
            let hint: String =
                (p["hint"].flatMap { pyTruthy($0) ? pyStr($0) : nil })
                ?? "the store must answer the operator's `read` — check \(store.id.value)"
            return Refusal.make(
                Refusal.unauthorized,
                "kernel_start: the memory store \(pyRepr(store.id.value)) refuses the kernel: "
                    + pyStr(p["error"] ?? .null),
                hint: hint)
        }
        // The reserved id bound goes to the loader now (the root is booted, so its
        // record writer is listening): a crash from here on skips ids, never reuses.
        update(root.id, ["next_id": .integer(idReserved)])
        let (wake, wakeUp) = AsyncStream<Void>.makeStream(bufferingPolicy: .bufferingNewest(1))
        storeLock.withLock {
            _storeId = store.id
            _flushWake = wakeUp
            _started = true
        }
        let fid = spawnTask(agent: root.id, verb: "flush_executions", keepalive: true) {
            [self] in
            for await _ in wake {
                if Task.isCancelled { return }
                try? await Task.sleep(nanoseconds: Kernel.flushDebounceNanos)
                if Task.isCancelled { return }
                _ = await self.flushExecutions()
            }
        }
        storeLock.withLock {
            _flusherTaskId = fid
        }
        wakeUp.yield()
        if bootAll {
            for a in allAgents() where a.id != root.id && a.id != store.id {
                let (reply, status) = await runHandler(a, .object(["type": .string("boot")]))
                if status == .completed {
                    booted.append(a.id.value)
                } else {
                    RuntimeLog.warn(
                        "  [kernel] boot \(pyRepr(a.id.value)): \(status.rawValue) "
                            + pyStr(reply))
                }
            }
        }
        return .object([
            "started": .bool(true), "store": .string(store.id.value),
            "booted": .array(booted.map { .string($0) }),
        ])
    }

    /// Stop the flush loop — the last step of a graceful stop, after the final
    /// flush landed.
    func stopFlusher() async {
        let (fid, wake): (TaskId?, AsyncStream<Void>.Continuation?) = storeLock.withLock {
            let f = _flusherTaskId
            let w = _flushWake
            _flusherTaskId = nil
            _flushWake = nil
            return (f, w)
        }
        wake?.finish()
        if let fid { _ = await cancelTask(fid) }
    }

    // MARK: - the flush

    /// Write every execution row still in RAM to the memory store, as ONE batch
    /// closed by a self-describing `flush` row; on success the rows leave RAM and
    /// a settled execution whose settle row landed leaves the ledger (the store is
    /// its record now). All or nothing: a refused batch stays in RAM for the next
    /// flush, and the refusal is said on stderr.
    ///
    /// The flush is the kernel's OWN call into the store (`storeCall`), not a
    /// send: it mints no execution of its own, so nothing about the flush needs
    /// flushing in turn. The `flush` row in the batch is its record.
    @discardableResult
    public func flushExecutions() async -> JSON {
        guard let store = storeId else { return .object(["flushed": .integer(0)]) }
        await flushMutex.acquire()
        let (res, ids, fid) = await flushLocked(store)
        await flushMutex.release()
        if let res { return res }
        var pruned = 0
        if let cap = ledger.cap, storedCalls > cap {
            pruned = await pruneExecutions()
        }
        return .object([
            "flushed": .integer(Int64(ids)), "flush": .integer(fid), "pruned": .integer(Int64(pruned)),
        ])
    }

    private var storedCalls: Int {
        storeLock.lock()
        defer { storeLock.unlock() }
        return _storedCalls
    }

    /// The flush body, with the flush mutex held. `(early reply, flushed, fid)`.
    private func flushLocked(_ store: AgentId) async -> (JSON?, Int, Int64) {
        let ids = rows.ids()
        if ids.isEmpty { return (.object(["flushed": .integer(0)]), 0, 0) }
        // Nothing of this flush is published — its own id mint (and an id-block
        // reservation it crosses) included (audit CLI-B).
        let (fid, kinds, result, status) = await KernelTaskLocals.$stateQuiet.withValue(true) {
            let fid = mintId()
            var kinds: [Int64: String] = [:]
            var batch: [JSON] = []
            for i in ids {
                guard let r = rows.getOne(i) else { continue }
                kinds[i] = r["kind"]?.asString ?? ""
                batch.append(.object(r))
            }
            batch.append(
                .object([
                    "id": .integer(fid), "kind": .string("flush"),
                    "covers": .array(ids.map { .integer($0) }), "status": .string("done"),
                    "ts": .double(Date().timeIntervalSince1970),
                ]))
            let (result, status) = await storeCall(
                store, .object(["type": .string("append"), "rows": .array(batch)]))
            return (fid, kinds, result, status)
        }
        if status != .completed {
            RuntimeLog.warn(
                "  [kernel] flush of \(ids.count) execution row(s) to \(pyRepr(store.value)) "
                    + "failed: \(status.rawValue) \(pyStr(result))")
            return (.object(["flushed": .integer(0), "error": result]), 0, fid)
        }
        for i in ids {
            guard let r = rows.remove(i) else { continue }
            if r["kind"]?.asString == "settle", let parent = r["parent"]?.asInt,
                ledger.settledOutcome(parent) != nil
            {
                ledger.forget(parent)
            }
        }
        let calls = ids.filter { kinds[$0] == "call" }.count
        storeLock.withLock {
            _storedCalls += calls
        }
        return (nil, ids.count, fid)
    }

    // MARK: - retention

    /// THE STORE-SIDE RETENTION WINDOW (`ledger_cap`). Over the cap, the OLDEST
    /// settled executions leave the kernel's store — unnamed first, then named (a
    /// named one carries a dedup contract, so it goes last) — each with its settle
    /// and ack rows, and the task/flush rows that only covered what was pruned. A
    /// DEAD LETTER that nobody acked stays: it is waiting for a person. In-flight
    /// work is never pruned. Returns how many executions went. Python
    /// `prune_executions`.
    @discardableResult
    public func pruneExecutions() async -> Int {
        guard let store = storeId else { return 0 }
        let rows: [Row]
        do {
            rows = try await storeRead([:])
        } catch {
            RuntimeLog.warn("  [kernel] prune of the kernel's store failed: \(error)")
            return 0
        }
        let calls = rows.filter { $0["kind"]?.asString == "call" && !pyTruthy($0["internal"] ?? .null) }
        var settles: [Int64: Row] = [:]
        var acked: Set<Int64> = []
        for r in rows {
            if r["kind"]?.asString == "settle", let p = r["parent"]?.asInt { settles[p] = r }
            if r["kind"]?.asString == "ack", let p = r["parent"]?.asInt { acked.insert(p) }
        }
        storeLock.withLock {
            _storedCalls = calls.count
        }
        let cap = ledger.cap ?? 0
        if calls.count <= cap { return 0 }
        // HYSTERESIS: down to 90% of the window, so a full store is not re-read
        // and re-pruned on every flush once it sits at the cap.
        let over = calls.count - (cap >= 10 ? cap * 9 / 10 : cap)

        func prunable(_ c: Row) -> Bool {
            guard let cid = c["id"]?.asInt, let st = settles[cid], !ledger.has(cid) else {
                return false  // in flight (or still live in RAM)
            }
            let status = st["outcome"]?["status"].asString ?? ""
            return !DEAD_STATUSES.contains(status) || acked.contains(cid)
        }

        var doomedCalls: [Int64] = []
        let sortedCalls = calls.sorted { ($0["id"]?.asInt ?? 0) < ($1["id"]?.asInt ?? 0) }
        for namedPass in [false, true] {
            for c in sortedCalls {
                if doomedCalls.count >= over { break }
                if pyTruthy(c["named"] ?? .null) == namedPass && prunable(c),
                    let cid = c["id"]?.asInt
                {
                    doomedCalls.append(cid)
                }
            }
        }
        if doomedCalls.isEmpty { return 0 }
        let gone = Set(doomedCalls)
        var doomed = gone
        for r in rows {
            let k = r["kind"]?.asString
            if k == "settle" || k == "ack", let p = r["parent"]?.asInt, gone.contains(p),
                let rid = r["id"]?.asInt
            {
                doomed.insert(rid)
            }
        }
        let keptCalls = calls.compactMap { $0["id"]?.asInt }.filter { !gone.contains($0) }
        let floor = keptCalls.min()
        for r in rows {
            guard let rid = r["id"]?.asInt else { continue }
            let k = r["kind"]?.asString
            if k == "flush" {
                let covers = (r["covers"]?.asArray ?? []).compactMap { $0.asInt }
                let below = Set(covers.filter { x in floor.map { x < $0 } ?? false })
                if Set(covers).isSubset(of: doomed.union(below)) { doomed.insert(rid) }
            } else if k == "task", let f = floor, rid < f {
                doomed.insert(rid)
            }
        }
        let (out, _) = await storeCall(
            store,
            .object([
                "type": .string("prune"),
                "ids": .array(doomed.sorted().map { .integer($0) }),
            ]))
        if pyTruthy(out["error"]) {
            RuntimeLog.warn("  [kernel] prune of the kernel's store failed: \(pyStr(out))")
            return 0
        }
        storeLock.withLock {
            _storedCalls -= gone.count
        }
        return gone.count
    }

    // MARK: - reading the store back

    /// Every row of the kernel's own key matching `payload`, all pages. A store
    /// that refuses is a RAISE (python `RuntimeError`).
    public func storeRead(_ payload: OrderedDictionary<String, JSON>) async throws(PyError) -> [Row] {
        guard let store = storeId else { return [] }
        var out: [Row] = []
        var after: JSON = .null
        while true {
            var q: OrderedDictionary<String, JSON> = ["type": .string("read")]
            for (k, v) in payload { q[k] = v }
            q["limit"] = .integer(1000)
            if after != .null { q["after"] = after }
            // QUIET like the flush: a ledger read publishes nothing on the state
            // stream (python `_store_read`).
            let (res, _) = await KernelTaskLocals.$stateQuiet.withValue(true) {
                await storeCall(store, .object(q))
            }
            guard case .object(let r) = res, !pyTruthy(r["error"] ?? .null) else {
                throw PyError("RuntimeError", "kernel: memory store read failed: \(pyStr(res))")
            }
            for row in r["rows"]?.asArray ?? [] {
                if case .object(let o) = row { out.append(o) }
            }
            after = r["next"] ?? .null
            if after == .null { return out }
        }
    }

    /// THE LEDGER PROJECTION: flushed `call`/`settle`/`ack` rows from the store,
    /// overlaid by what is still in RAM (live executions, unflushed rows). Each
    /// entry carries its originator as `_caller` for the verb to scope by, and in
    /// python's key order: `id, target, attempts, retries, named, _caller, name?,
    /// status, outcome?, acked?`. Python `ledger_entries`.
    public func ledgerEntries() async throws(PyError) -> [OrderedDictionary<String, JSON>] {
        var entries: [Int64: OrderedDictionary<String, JSON>] = [:]
        var acked: Set<Int64> = []
        var rowsIn: [Row] = []
        if storeId != nil {
            rowsIn = try await storeRead([
                "kinds": .array([.string("call"), .string("settle"), .string("ack")])
            ])
        }
        rowsIn += rows.all().filter {
            ["call", "settle", "ack"].contains($0["kind"]?.asString ?? "")
        }
        var settles: [Int64: Row] = [:]
        for r in rowsIn {
            switch r["kind"]?.asString {
            case "ack": if let p = r["parent"]?.asInt { acked.insert(p) }
            case "settle": if let p = r["parent"]?.asInt { settles[p] = r }
            default: break
            }
        }
        for r in rowsIn {
            guard r["kind"]?.asString == "call", !pyTruthy(r["internal"] ?? .null),
                let eid = r["id"]?.asInt
            else { continue }
            var e: OrderedDictionary<String, JSON> = [
                "id": .integer(eid),
                "target": r["agent"] ?? .null,
                "attempts": r["attempts"] ?? .integer(1),
                "retries": r["retries"] ?? .integer(0),
                "named": .bool(pyTruthy(r["named"] ?? .null)),
                "_caller": r["caller"] ?? .null,
            ]
            if let n = r["name"], pyTruthy(n) { e["name"] = n }
            if let st = settles[eid] {
                let oc = st["outcome"]?.asObject ?? [:]
                e["status"] = oc["status"] ?? .null
                if let a = st["attempts"], pyTruthy(a) { e["attempts"] = a }
                var o: OrderedDictionary<String, JSON> = ["pid": .integer(eid)]
                for (k, v) in oc { o[k] = v }
                o["attempts"] = e["attempts"]
                e["outcome"] = .object(o)
            } else {
                e["status"] = .string("in_flight")
            }
            entries[eid] = e
        }
        for x in ledger.entries() {
            var e: OrderedDictionary<String, JSON> = [
                "id": .integer(x.id),
                "target": .string(x.target.value),
                "attempts": .integer(Int64(x.attempts)),
                "retries": .integer(Int64(x.retries)),
                "named": .bool(x.named),
                "_caller": x.caller.map { JSON.string($0.value) } ?? .null,
            ]
            if let n = x.name { e["name"] = .string(n) }
            if let res = x.outcome {
                let status = res.status.rawValue
                let result = res.result
                let body = res.body
                e["status"] = .string(status)
                // THE SAME SHAPE flushed or not: what the settle row keeps —
                // rendered `_json_safe`, as python's in-RAM branch is, so a
                // NAMED result's chunk reads `<bytes:N>` until it flushes as
                // `{len: N}` (an unnamed digest counts the `{len}` form either way).
                var o: OrderedDictionary<String, JSON> = ["pid": .integer(x.id)]
                for (k, v) in storedOutcome(
                    status: status,
                    result: x.named
                        ? Kernel.jsonSafeValue(result, body: body)
                        : Kernel.storeValue(result, body: body),
                    named: x.named)
                {
                    o[k] = v
                }
                o["attempts"] = .integer(Int64(res.attempts))
                e["outcome"] = .object(o)
            } else {
                e["status"] = .string("in_flight")
            }
            if x.acked { acked.insert(x.id) }
            entries[x.id] = e
        }
        for eid in acked where entries[eid] != nil {
            entries[eid]?["acked"] = .bool(true)
        }
        return entries.keys.sorted().compactMap { entries[$0] }
    }

    /// Bring a flushed execution back into RAM before `send` launches an item that
    /// names it — a caller NAME not held in RAM, or an int id not in the ledger. A
    /// settled one comes back as a DONE execution holding its recorded outcome
    /// (dedup survives the flush and a restart); one with no settle row was in
    /// flight when the process stopped and rebuilds nothing — re-sending runs it.
    /// Python `recall_executions`.
    func recallExecutions(_ items: [Item]) async throws(PyError) {
        var wanted: [(isName: Bool, name: String, id: Int64)] = []
        for it in items {
            let raw: JSON? = it.rawId ?? it.id.map { JSON.string($0) }
            switch raw {
            case .string(let s)? where !s.isEmpty && ledger.resolveName(s) == nil:
                wanted.append((true, s, 0))
            case .integer(let n)? where !ledger.has(n):
                wanted.append((false, "", n))
            default:
                break
            }
        }
        if wanted.isEmpty { return }
        for w in wanted {
            var q: OrderedDictionary<String, JSON> = ["kinds": .array([.string("call")])]
            if w.isName {
                q["match"] = .object(["name": .string(w.name)])
            } else {
                q["ids"] = .array([.integer(w.id)])
            }
            let calls = try await storeRead(q)
            guard let call = calls.last, let cid = call["id"]?.asInt else { continue }
            let settles = try await storeRead([
                "kinds": .array([.string("settle")]),
                "match": .object(["parent": .integer(cid)]),
            ])
            guard let st = settles.last else { continue }
            let oc = st["outcome"]?.asObject ?? [:]
            let attempts = UInt32(
                clamping: (st["attempts"].flatMap { pyTruthy($0) ? $0.asInt : nil })
                    ?? (call["attempts"].flatMap { pyTruthy($0) ? $0.asInt : nil }) ?? 1)
            // A NAMED call's settle kept its result; an unnamed one (joined by its
            // int id) kept only what `storedOutcome` keeps — and says so.
            let result: JSON
            if let r = oc["result"] {
                result = r
            } else {
                var rest: OrderedDictionary<String, JSON> = [:]
                for (k, v) in oc where k != "status" { rest[k] = v }
                result = .object(rest)
            }
            guard let statusText = oc["status"]?.asString, let status = Status(rawValue: statusText)
            else { continue }
            ledger.putSettled(
                cid,
                target: AgentId(call["agent"]?.asString ?? ""),
                payload: call["payload"],
                retries: UInt32(clamping: call["retries"]?.asInt ?? 0),
                attempts: attempts,
                name: call["name"]?.asString,
                caller: call["caller"]?.asString.map { AgentId($0) },
                outcome: Outcome(
                    pid: TaskId(cid), status: status, result: result, attempts: attempts))
        }
    }

    // MARK: - dead letters

    /// `on_deadletter` — an execution settled failed / timed_out / cancelled.
    /// Emitted on the ORIGINATING caller's own inbox (and so to whoever watches
    /// it, a UI included), or on the root for an operator send. Nothing retries
    /// it; the event is how a person learns there is something to decide. Python
    /// `announce_deadletter`.
    func announceDeadletter(_ pid: Int64, status: Status, attempts: UInt32) {
        let exe = ledger.entry(pid)
        var entry: OrderedDictionary<String, JSON> = [
            "type": .string("on_deadletter"),
            "id": .integer(pid),
            "target": exe.map { JSON.string($0.target.value) } ?? .null,
            "caller": exe?.caller.map { JSON.string($0.value) } ?? .null,
            "status": .string(status.rawValue),
            "attempts": .integer(Int64(attempts)),
        ]
        if let n = exe?.name { entry["name"] = .string(n) }
        let src: Agent? = exe?.caller.flatMap { agent($0) } ?? root
        guard let src else { return }
        fanout(src, "emit", .text(.object(entry)))
    }
}
