// The EXECUTION LEDGER, process side — every dispatch is an execution, named or
// not.
//
// Port of python `kernel/_agent.py`'s `_ledger_open`/`_record` + `_kernel.py`'s
// `executions`/`ledger_put` (canonical).
//
// A batch item's optional `id` NAMES the execution — ENSURE-semantics, exactly
// what a supplied `create_agent` id already means, applied to dispatch:
//
//   unknown id ⇒ dispatch and record (a name, not a new mechanism)
//   in flight  ⇒ JOIN — no second run (C10: once-only under concurrency)
//   settled    ⇒ the recorded outcome returns, the handler never runs
//                (C11 idempotency; C9 across the flush and a restart — a settled
//                 execution that left RAM lives on as its `call` + `settle` rows
//                 in the kernel's memory store, and `Kernel.recallExecutions`
//                 brings it back before `send` launches an item naming it)
//
// ONE ID SPACE. The caller's id IS the pid that `ps`/`cancel`/reply tags carry;
// an UNNAMED execution is ledgered under its minted pid.
//
// RAM HOLDS WHAT IS IN FLIGHT OR NOT YET FLUSHED. Once an execution's settle row
// landed in the store it leaves this table (`forget`); the `ledger` verb is a
// PROJECTION over the store plus what is still here.
//
// RETENTION IS A RING (for a kernel that never started and so never flushes).
// Past `cap`, the OLDEST SETTLED rows are evicted, unnamed before named — a named
// row carries a dedup contract, and evicting it silently re-arms a side effect.
// In-flight rows are never evicted.

import FantasticJSON
import Foundation
import OrderedCollections

/// Default ring size. Big enough that a normal session never evicts, bounded so
/// a long-lived kernel cannot grow without limit.
public let defaultLedgerCap = 1_000_000

/// THE FUTURE of one execution — python's `fut`. A joiner holds the box it was
/// handed at launch, so the outcome reaches it even after the execution left
/// the table (flushed and forgotten between two polls).
public final class OutcomeBox: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Outcome?

    init(_ value: Outcome? = nil) { self.value = value }

    public var outcome: Outcome? {
        lock.lock()
        defer { lock.unlock() }
        return value
    }

    func set(_ o: Outcome) {
        lock.lock()
        defer { lock.unlock() }
        if value == nil { value = o }
    }
}

/// What ``ExecutionLedger/open(_:target:payload:retries:name:caller:callerId:)``
/// decided.
public enum Opened {
    /// A new execution: dispatch it, then settle it.
    case fresh
    /// Someone else is running this id — await their outcome, run nothing.
    /// Carries the execution's ORIGINAL PAYLOAD: a joiner is handed that
    /// execution's outcome verbatim, so the gate must be asked about the call
    /// that was actually made. `nil` after a recall from the store, where
    /// python's row carries the payload but the joiner is gated on it the same.
    case join(payload: JSON?, id: Int64, box: OutcomeBox)
    /// Already settled: this IS the answer; the handler must not run again.
    case settled(Outcome, payload: JSON?, id: Int64)
    /// The id is unusable — the refusal to hand back verbatim.
    case refused(JSON)
}

/// One execution in RAM, as the ledger projection reads it.
public struct LedgerEntry: Sendable {
    public let id: Int64
    public let target: AgentId
    public let retries: UInt32
    public let attempts: UInt32
    public let name: String?
    public var named: Bool { name != nil }
    public let caller: AgentId?
    public let outcome: Outcome?
    public let acked: Bool
    /// The outcome AS RECORDED when the item marked `secret` paths — what the
    /// `ledger` read-back shows instead of `outcome` (python `shown_out`).
    let shown: ShownOutcome?
}

/// One execution and what it became.
private struct LedgerRow {
    var target: AgentId
    /// The PAYLOAD this execution ran — the question the gate must be asked when
    /// somebody joins it, arguments included.
    var payload: JSON?
    var retries: UInt32
    var attempts: UInt32
    /// The caller's NAME for this execution, when it gave one — the dedup /
    /// join / retry handle. Its id is the kernel's message id either way.
    var name: String?
    var named: Bool { name != nil }
    /// WHO CALLED — `nil` for the operator. Scopes `ledger`, `ack_deadletter`
    /// and the `on_deadletter` announcement. (Python renamed `origin` →
    /// `caller`: replies own `origin` now.)
    var caller: AgentId?
    /// The future: `nil` inside while in flight. Set ONCE, at the FINAL outcome —
    /// a retrying execution does not settle its intermediate attempts.
    var box: OutcomeBox
    /// A dead letter someone acknowledged (`ack_deadletter`).
    var acked: Bool = false
    /// RAM only: the item's `secret` paths and values — what the settle masks the
    /// recorded outcome with. Never on a recalled row.
    var secret: Secret? = nil
    /// The outcome AS RECORDED (python `shown_out`), set at the settle of a call
    /// with `secret`; the `ledger` read-back shows it, the caller and joiners got
    /// the real one.
    var shown: ShownOutcome? = nil
    var outcome: Outcome? { box.outcome }
}

/// The kernel's execution ledger, process side.
public final class ExecutionLedger: @unchecked Sendable {
    private let lock = NSLock()
    private var rows: [Int64: LedgerRow] = [:]
    /// Caller name → the id it opened. `resolveName` is the ONE bridge.
    private var names: [String: Int64] = [:]
    private var order: [Int64] = []
    private var _cap: Int?

    public init(cap: Int? = defaultLedgerCap) { self._cap = cap }

    /// The retention window — the RAM ring here, the store-side prune window
    /// in the kernel's memory store (python `ledger_cap`). `nil` = unbounded.
    public var cap: Int? {
        lock.lock()
        defer { lock.unlock() }
        return _cap
    }

    /// The id a caller's NAME resolves to, while its execution is held in RAM.
    public func resolveName(_ name: String) -> Int64? {
        lock.lock()
        defer { lock.unlock() }
        return names[name]
    }

    /// Claim `execId` for an execution, or discover it is already taken.
    ///
    /// `callerId` is the CALLER's raw id AS GIVEN — a string name or an int
    /// (C38) — used ONLY to render the rebind refusal in python's exact words.
    ///
    /// `payload` is the payload AS SHOWN when the item marked `secret` paths —
    /// the RAM entry never holds the values; `secret` is kept for the settle.
    public func open(
        _ execId: Int64, target: AgentId, payload: JSON, retries: UInt32, name: String?,
        caller: AgentId?, callerId: JSON, secret: Secret?
    ) -> Opened {
        lock.lock()
        defer { lock.unlock() }

        // A NAME THAT ALREADY OPENED AN EXECUTION joins it — resolved here, under
        // the lock, so two callers racing on one name cannot both open.
        var execId = execId
        if let n = name, let existing = names[n] { execId = existing }
        if let row = rows[execId] {
            // AN ID IS BOUND TO ITS FIRST TARGET.
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
            return .join(payload: row.payload, id: execId, box: row.box)
        }

        if let n = name { names[n] = execId }
        rows[execId] = LedgerRow(
            target: target, payload: payload, retries: retries, attempts: 0, name: name,
            caller: caller, box: OutcomeBox(), secret: secret)
        order.append(execId)
        evict()
        return .fresh
    }

    /// Whether a row exists under `execId` (named or minted, settled or not).
    public func has(_ execId: Int64) -> Bool {
        lock.withLock { rows[execId] != nil }
    }

    /// Count one dispatch attempt; returns the new count.
    @discardableResult
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
        guard let r = rows[execId] else { return outcome }
        var out = outcome
        out.attempts = max(r.attempts, 1)
        r.box.set(out)
        return r.box.outcome ?? out
    }

    /// The settled outcome for `execId`, if it has one.
    public func settledOutcome(_ execId: Int64) -> Outcome? {
        lock.lock()
        defer { lock.unlock() }
        return rows[execId]?.outcome
    }

    /// The `secret` an execution's item marked, while it is held in RAM.
    func secret(_ execId: Int64) -> Secret? {
        lock.withLock { rows[execId]?.secret }
    }

    /// Keep the outcome AS RECORDED for the `ledger` read-back (python
    /// `exe["shown_out"] = out`).
    func setShown(_ execId: Int64, _ shown: ShownOutcome) {
        lock.withLock { rows[execId]?.shown = shown }
    }

    /// Is `execId` held and NAMED? (What the settle row keeps depends on it.)
    public func isNamed(_ execId: Int64) -> Bool {
        lock.withLock { rows[execId]?.named ?? false }
    }

    /// One execution's projection fields, if it is still in RAM.
    public func entry(_ execId: Int64) -> LedgerEntry? {
        lock.lock()
        defer { lock.unlock() }
        guard let r = rows[execId] else { return nil }
        return LedgerEntry(
            id: execId, target: r.target, retries: r.retries, attempts: r.attempts,
            name: r.name, caller: r.caller, outcome: r.outcome, acked: r.acked, shown: r.shown)
    }

    /// Every execution still in RAM, oldest first — the RAM half of the ledger
    /// projection.
    public func entries() -> [LedgerEntry] {
        lock.lock()
        defer { lock.unlock() }
        return order.compactMap { id in
            guard let r = rows[id] else { return nil }
            return LedgerEntry(
                id: id, target: r.target, retries: r.retries, attempts: r.attempts,
                name: r.name, caller: r.caller, outcome: r.outcome, acked: r.acked, shown: r.shown)
        }
    }

    /// The ids of executions still IN FLIGHT (no outcome yet).
    public func inFlightIds() -> [Int64] {
        lock.lock()
        defer { lock.unlock() }
        return order.filter { rows[$0]?.outcome == nil && rows[$0] != nil }
    }

    /// `ack_deadletter` on an execution still in RAM.
    public func markAcked(_ execId: Int64) {
        lock.lock()
        defer { lock.unlock() }
        rows[execId]?.acked = true
    }

    /// FORGET one execution — it settled and its settle row landed in the store,
    /// which is its record now. Python `_forget_execution`.
    public func forget(_ execId: Int64) {
        lock.lock()
        defer { lock.unlock() }
        forgetLocked(execId)
    }

    private func forgetLocked(_ execId: Int64) {
        guard let r = rows.removeValue(forKey: execId) else { return }
        if let n = r.name, names[n] == execId { names.removeValue(forKey: n) }
        order.removeAll { $0 == execId }
    }

    /// BRING A FLUSHED EXECUTION BACK — its `call` + `settle` rows read from the
    /// kernel's memory store, so dedup survives the flush and a restart. A row
    /// that is DONE, holding the recorded outcome. A LIVE ROW ALWAYS WINS: an id
    /// the ledger already holds is not overwritten. Returns whether a row was
    /// added. Python `recall_executions` → `ledger_put`.
    @discardableResult
    public func putSettled(
        _ execId: Int64, target: AgentId, payload: JSON?, retries: UInt32, attempts: UInt32,
        name: String?, caller: AgentId?, outcome: Outcome
    ) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if rows[execId] != nil { return false }
        rows[execId] = LedgerRow(
            target: target, payload: payload, retries: retries, attempts: attempts, name: name,
            caller: caller, box: OutcomeBox(outcome))
        if let n = name { names[n] = execId }
        order.append(execId)
        evict()
        return true
    }

    /// Set the ring's cap and immediately re-run eviction against it (C80).
    /// `nil` = unbounded.
    public func setCap(_ newCap: Int?) {
        lock.lock()
        defer { lock.unlock() }
        _cap = newCap
        evict()
    }

    /// The ring: evict oldest SETTLED rows, unnamed before named, never
    /// in-flight.
    private func evict() {
        guard let cap = _cap else { return }
        if rows.count <= cap { return }
        for namedPass in [false, true] {
            var i = 0
            while rows.count > cap && i < order.count {
                let id = order[i]
                if let r = rows[id], r.outcome != nil, r.named == namedPass {
                    rows.removeValue(forKey: id)
                    if let n = r.name, names[n] == id { names.removeValue(forKey: n) }
                    order.remove(at: i)
                } else {
                    i += 1
                }
            }
            if rows.count <= cap { return }
        }
    }
}
