// The EXECUTION LEDGER — every dispatch is a row, named or not.
//
// Port of python `kernel/_agent.py`'s `_ledger_open`/`_record` + `_kernel.py`'s
// `executions`/`ledger_put` (canonical). `tmp/pychanges.md` #34–#37.
//
// A batch item's optional `id` NAMES the execution — ENSURE-semantics, exactly
// what a supplied `create_agent` id already means, applied to dispatch:
//
//   unknown id ⇒ dispatch and record (a name, not a new mechanism)
//   in flight  ⇒ JOIN — no second run (C10: once-only under concurrency)
//   settled    ⇒ the recorded outcome returns, the handler never runs
//                (C11 idempotency; C9 in-process, and across restart once persisted)
//
// ONE ID SPACE. The caller's id IS the pid that `ps`/`cancel`/reply tags carry;
// an UNNAMED execution is ledgered under its minted pid. So any reply is
// re-fetchable by pid, and there is no second id type to reconcile — a minted
// pid can never dedup (the caller learns it only after dispatch), a
// caller-chosen id can.
//
// RETENTION IS A RING. Past `cap`, the OLDEST SETTLED rows are evicted, unnamed
// before named — a named row carries a dedup contract, and evicting it silently
// re-arms a side effect. In-flight rows are never evicted.
//
// THE VIEW IS ORIGIN-SCOPED. Rows record who originated them; an agent sees only
// its own, the operator sees all. Unscoped, `ledger` would be a side-channel
// around `callable_by` — reading other agents' result bodies without ever being
// granted an edge to them.

import FantasticJSON
import Foundation
import OrderedCollections

/// Default ring size. Big enough that a normal session never evicts, bounded so
/// a long-lived kernel cannot grow without limit.
public let defaultLedgerCap = 1_000_000

/// The `ledger.json` sidecar format. v2 (2026-09-08): ids are the kernel's int64
/// message ids, so rows are an ARRAY — a JSON object key would stringify them —
/// and a named row carries `name`. An unknown version starts empty LOUDLY; there
/// is no migration.
public let ledgerSidecarVersion = 2

/// What ``ExecutionLedger/open(_:target:retries:named:origin:)`` decided.
public enum Opened {
    /// A new execution: dispatch it, then settle it.
    case fresh
    /// Someone else is running this id — await their outcome, run nothing.
    /// Carries the execution's ORIGINAL PAYLOAD: a joiner is handed that
    /// execution's outcome verbatim, so the gate must be asked about the call
    /// that was actually made — its verb AND its arguments (a `create_agent`'s
    /// `parent_id` is the placement the gate reads). `nil` after a rehydrate,
    /// where python's row carries `payload: None`.
    case join(payload: JSON?, id: Int64)
    /// Already settled: this IS the answer; the handler must not run again.
    case settled(Outcome, payload: JSON?, id: Int64)
    /// The id is unusable — the refusal to hand back verbatim.
    case refused(JSON)
}

/// One execution and what it became.
private struct LedgerRow {
    var target: AgentId
    /// The PAYLOAD this execution ran — the question the gate must be asked when
    /// somebody joins it, arguments included. `nil` on a rehydrated row: a
    /// settled execution has nothing left to re-run, and python stores
    /// `payload: None` for exactly that reason.
    var payload: JSON?
    var retries: UInt32
    var attempts: UInt32
    /// The caller's NAME for this execution, when it gave one — the dedup /
    /// join / retry handle. Its id is the kernel's message id either way.
    var name: String?
    var named: Bool { name != nil }
    var origin: AgentId?
    /// `nil` while in flight. Set ONCE, at the FINAL outcome — a retrying
    /// execution does not settle its intermediate attempts, so a joiner never
    /// sees an answer that was later retried away.
    var outcome: Outcome?
}

/// The kernel's execution ledger. Session-lifetime; the durable half is written
/// through the discovered store, because the kernel never touches disk itself.
public final class ExecutionLedger: @unchecked Sendable {
    private let lock = NSLock()
    private var rows: [Int64: LedgerRow] = [:]
    /// Caller name → the id it opened. `resolveName` is the ONE bridge.
    private var names: [String: Int64] = [:]
    private var order: [Int64] = []
    private var cap: Int?

    public init(cap: Int? = defaultLedgerCap) { self.cap = cap }

    /// Claim `execId` for an execution, or discover it is already taken.
    ///
    /// `named` distinguishes a caller-chosen id from a minted pid: only the
    /// former is a dedup contract, and the ring evicts the latter first.
    /// The id a caller's NAME resolves to, if that name has opened an execution.
    public func resolveName(_ name: String) -> Int64? {
        lock.lock()
        defer { lock.unlock() }
        return names[name]
    }

    /// `callerId` is the CALLER's raw id AS GIVEN — a string name or an int
    /// (C38) — used ONLY to render the rebind refusal in python's exact words
    /// (`.string`s pyRepr'd, `.integer`s bare digits); `name` is `Some` only
    /// for the string-name case.
    public func open(
        _ execId: Int64, target: AgentId, payload: JSON, retries: UInt32, name: String?,
        origin: AgentId?, callerId: JSON
    ) -> Opened {
        lock.lock()
        defer { lock.unlock() }

        // A NAME THAT ALREADY OPENED AN EXECUTION joins it — resolved here, under
        // the lock, so two callers racing on one name cannot both open. (The
        // resolve in `launch` is the fast path; this is the one that holds.)
        var execId = execId
        if let n = name, let existing = names[n] { execId = existing }
        if let row = rows[execId] {
            // AN ID IS BOUND TO ITS FIRST TARGET. The same id aimed elsewhere is
            // a different intent wearing the same name — returning the other
            // execution's outcome would be silently wrong.
            //
            // PYTHON'S PROSE, verbatim (C31 r2 correction): BOTH the error text
            // and the extras render the CALLER's RAW id, never the minted
            // `execId` — `pyRepr` for a string, bare digits for an int; the
            // target names via `pyRepr` too. NO `bound_to`.
            if row.target != target {
                return .refused(
                    Refusal.make(
                        Refusal.badArg,
                        "send: id \(pyRepr(callerId)) already names an execution on "
                            + "\(pyRepr(row.target.value)), not \(pyRepr(target.value)) — an id "
                            + "is bound to its first execution; a retry aimed elsewhere needs a "
                            + "new id",
                        extra: [("arg", .string("id"))]))
            }
            if let o = row.outcome { return .settled(o, payload: row.payload, id: execId) }
            return .join(payload: row.payload, id: execId)
        }

        // A COLON ID REFERENCES an existing minted execution; it never creates
        // one. The caller cannot mint into the kernel's namespace, so an unknown
        // colon id is a dangling reference, not a new job.
        if let n = name { names[n] = execId }
        rows[execId] = LedgerRow(
            target: target, payload: payload, retries: retries, attempts: 0, name: name,
            origin: origin,
            outcome: nil)
        order.append(execId)
        evict()
        return .fresh
    }

    /// Count one dispatch attempt; returns the new count.
    @discardableResult
    /// Whether a row exists under `execId` (named or minted, settled or not).
    public func has(_ execId: Int64) -> Bool {
        lock.withLock { rows[execId] != nil }
    }

    public func attempt(_ execId: Int64) -> UInt32 {
        lock.lock()
        defer { lock.unlock() }
        guard var r = rows[execId] else { return 1 }
        r.attempts += 1
        rows[execId] = r
        return r.attempts
    }

    /// May this execution be dispatched again? Only a terminal `failed`/
    /// `timed_out` with budget left. `cancelled` is ABSORBING; `completed` never
    /// retries.
    public func shouldRetry(_ execId: Int64, _ status: Status) -> Bool {
        guard status.isRetryable else { return false }
        lock.lock()
        defer { lock.unlock() }
        guard let r = rows[execId] else { return false }
        return r.attempts <= r.retries
    }

    /// Record the FINAL outcome. Stamps `attempts`, so a caller sees what its
    /// work actually cost.
    @discardableResult
    public func settle(_ execId: Int64, _ outcome: Outcome) -> Outcome {
        lock.lock()
        defer { lock.unlock() }
        guard var r = rows[execId] else { return outcome }
        var out = outcome
        out.attempts = max(r.attempts, 1)
        r.outcome = out
        rows[execId] = r
        return out
    }

    /// The settled outcome for `execId`, if it has one — how a joiner reads the
    /// owner's answer.
    public func settledOutcome(_ execId: Int64) -> Outcome? {
        lock.lock()
        defer { lock.unlock() }
        return rows[execId]?.outcome
    }

    /// Rows as the `ledger` verb renders them, oldest first, scoped to `viewer`.
    ///
    /// `viewer` is the ORIGIN of the reading dispatch: `nil` = operator, who sees
    /// everything; otherwise an agent, which sees only what it originated.
    public func rows(for viewer: AgentId?) -> [JSON] {
        lock.lock()
        defer { lock.unlock() }
        return order.compactMap { id -> JSON? in
            guard let r = rows[id] else { return nil }
            if let v = viewer, r.origin != v { return nil }
            // NO `verb` KEY. Python's row is exactly {id, target, attempts,
            // retries, named, name?, status, outcome?} and the ports carried an
            // eighth key, so the same wiring answered a 7-key row here and an
            // 8-key row there. The verb is not lost — a live row keeps the whole
            // ORIGINAL PAYLOAD, which is what authorizes a joiner; the read verb
            // never needed it. C16: `status`/`outcome` land AFTER `name`, not
            // right after `target` — python builds the base dict, then
            // optionally `name`, then `status` (+`outcome` only when settled).
            var o: OrderedDictionary<String, JSON> = [
                "id": .integer(id),
                "target": .string(r.target.value),
                "attempts": .integer(Int64(r.attempts)),
                "retries": .integer(Int64(r.retries)),
                "named": .bool(r.named),
            ]
            if let n = r.name { o["name"] = .string(n) }
            if let out = r.outcome {
                o["status"] = .string(out.status.rawValue)
                o["outcome"] = out.asJSON
            } else {
                o["status"] = .string("in_flight")
            }
            return .object(o)
        }
    }

    /// Every SETTLED row, as the durable sidecar stores them — python's v2
    /// document `{"v": 2, "rows": [...]}`.
    ///
    /// AN ARRAY, not an object: the ids are the kernel's int64 message ids and a
    /// JSON object key would stringify every one of them. Each row carries
    /// python's key order `{id, target, retries, attempts, named, name, origin,
    /// outcome}` — and NO `verb`: the joiner's gate question is sourced from the
    /// ORIGINAL PAYLOAD a live row holds, and a rehydrated row has none.
    ///
    /// In-flight rows are deliberately absent: work that never completed did not
    /// complete, and a post-reboot re-send re-running it is the truth, not a
    /// loss.
    ///
    /// AND SO IS A ROW WHOSE OUTCOME CARRIED RAW BYTES. Python's
    /// `_ledger_dump` is a STRICT round-trip filter: an outcome that
    /// `json.dumps` cannot encode — "bytes in a read_stream result" is the
    /// case it names — stays RAM-only rather than persisting a `default=str`
    /// repr that rehydrates as a confidently wrong recorded outcome. Swift
    /// carries those bytes on the outcome's own binary channel instead of
    /// inside `result`, so the JSON always encodes and this port kept every
    /// such row: measured on `store_files_*/disk_raw/ledger.json`, swift
    /// persisted `{path, offset, size, next_offset, eof}` rows python has
    /// none of. The channel is the difference, not the rule — a settled
    /// outcome with a `body` is exactly python's unencodable one.
    public func settledSnapshot() -> JSON {
        lock.lock()
        defer { lock.unlock() }
        var out: [JSON] = []
        for id in order {
            guard let r = rows[id], let outcome = r.outcome else { continue }
            guard outcome.body == nil else { continue }
            out.append(
                .object([
                    "id": .integer(id),
                    "target": .string(r.target.value),
                    "retries": .integer(Int64(r.retries)),
                    "attempts": .integer(Int64(r.attempts)),
                    "named": .bool(r.named),
                    "name": r.name.map { JSON.string($0) } ?? .null,
                    "origin": r.origin.map { JSON.string($0.value) } ?? .null,
                    "outcome": outcome.asJSON,
                ]))
        }
        return .object(["v": .integer(Int64(ledgerSidecarVersion)), "rows": .array(out)])
    }

    /// Restore SETTLED rows from a v2 sidecar, DONE, so dedup holds across a
    /// reboot: a completed step is never re-executed by a fresh process and a
    /// parked dead letter stays dead.
    ///
    /// EVERY terminal status comes back, `errored` included — a row read as
    /// something else would re-arm the side effect the ledger exists to prevent.
    /// A rehydrated row carries NO payload: it is settled, so there is nothing
    /// left to re-run and a joiner is gated on an empty call (python's
    /// `entry["payload"] = None`).
    @discardableResult
    public func rehydrate(_ doc: JSON) -> Int {
        guard let src = doc["rows"].asArray else { return 0 }
        lock.lock()
        defer { lock.unlock() }
        var n = 0
        for r in src {
            guard let id = r["id"].asInt, let target = r["target"].asString,
                let statusText = r["outcome"]["status"].asString,
                let status = Status(rawValue: statusText)
            else { continue }
            let outcome = Outcome(
                pid: r["outcome"]["pid"].asInt.map { TaskId(Int64($0)) },
                status: status,
                result: r["outcome"]["result"],
                attempts: UInt32(r["outcome"]["attempts"].asInt ?? 1))
            rows[id] = LedgerRow(
                target: AgentId(target),
                payload: nil,
                retries: UInt32(r["retries"].asInt ?? 0),
                attempts: UInt32(r["attempts"].asInt ?? 1),
                name: r["name"].asString,
                origin: r["origin"].asString.map { AgentId($0) },
                outcome: outcome)
            if let nm = r["name"].asString { names[nm] = id }
            order.append(id)
            n += 1
        }
        evict()
        return n
    }

    /// Set the ring's cap and immediately re-run eviction against it (C80).
    /// Python sets its `ledger_cap` attribute directly — a lazy change that
    /// only bites on the NEXT insert — but the CLI always sets a cap BEFORE
    /// any execution exists, so the two are equivalent at that one call
    /// site; evicting here too costs nothing and also covers a cap lowered
    /// with rows already live, which python's plain attribute set would
    /// otherwise leave overshooting until the next `open`. `nil` = unbounded.
    public func setCap(_ newCap: Int?) {
        lock.lock()
        defer { lock.unlock() }
        cap = newCap
        evict()
    }

    /// The highest execution id the ledger holds — what hydration raises the
    /// row counter above, so a fresh mint never collides with a settled id.
    public func highestId() -> Int64? {
        lock.lock()
        defer { lock.unlock() }
        return rows.keys.max()
    }

    /// The ring: evict oldest SETTLED rows, unnamed before named, never
    /// in-flight.
    private func evict() {
        guard let cap else { return }
        if rows.count <= cap { return }
        for namedPass in [false, true] {
            var i = 0
            while rows.count > cap && i < order.count {
                let id = order[i]
                if let r = rows[id], r.outcome != nil, r.named == namedPass {
                    rows.removeValue(forKey: id)
                    order.remove(at: i)
                } else {
                    i += 1
                }
            }
            if rows.count <= cap { return }
        }
    }
}

extension Kernel {
    /// Install the durable-ledger write-through. The kernel calls it after every
    /// settle and knows NOTHING about what it does — the store owns the disk.
    public func setLedgerFlusher(_ f: @escaping @Sendable () -> Void) {
        ledgerFlusherLock.lock()
        defer { ledgerFlusherLock.unlock() }
        _ledgerFlusher = f
    }

    /// Fire the durable write-through, if one is installed. No flusher = no
    /// persistence: settled outcomes live and die with the process. That is the
    /// auto-flush law, not a fallback.
    /// Is this settling dispatch the loader persisting ITSELF — an
    /// operator-context call on the discovered store (no `origin`, because
    /// nobody sent it)? Those are the flush's own `ledger.json` write and the
    /// pump's `messages.xml` append; `MessagePump.skips` keeps them out of the
    /// log and they stay out of the ledger's dirty set for the same reason.
    func isOwnPersistence(target: AgentId, execId: Int64) -> Bool {
        guard let store = findStore(), store == target else { return false }
        guard let row = rows.getOne(execId) else { return false }
        return (row["origin"] ?? .null) == .null
    }

    public func ledgerSettled() {
        ledgerFlusherLock.lock()
        let f = _ledgerFlusher
        ledgerFlusherLock.unlock()
        f?()
    }
}
