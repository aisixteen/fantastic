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

/// What ``ExecutionLedger/open(_:target:retries:named:origin:)`` decided.
public enum Opened {
    /// A new execution: dispatch it, then settle it.
    case fresh
    /// Someone else is running this id — await their outcome, run nothing.
    /// Carries the VERB that execution ran: a joiner is handed its outcome
    /// verbatim, so it must be authorized for THAT verb, not for whatever it
    /// happened to send.
    case join(verb: String, id: Int64)
    /// Already settled: this IS the answer; the handler must not run again.
    case settled(Outcome, verb: String, id: Int64)
    /// The id is unusable — the refusal to hand back verbatim.
    case refused(JSON)
}

/// One execution and what it became.
private struct LedgerRow {
    var target: AgentId
    /// The verb this execution ran — the question the gate must be asked when
    /// somebody joins it.
    var verb: String
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

    public func open(
        _ execId: Int64, target: AgentId, verb: String, retries: UInt32, name: String?,
        origin: AgentId?
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
            if row.target != target {
                return .refused(
                    Refusal.make(
                        Refusal.badArg,
                        "send: id \(execId) is bound to \(row.target.value) — a named "
                            + "execution cannot be re-aimed",
                        extra: [
                            ("arg", .string("id")), ("bound_to", .string(row.target.value)),
                        ]))
            }
            if let o = row.outcome { return .settled(o, verb: row.verb, id: execId) }
            return .join(verb: row.verb, id: execId)
        }

        // A COLON ID REFERENCES an existing minted execution; it never creates
        // one. The caller cannot mint into the kernel's namespace, so an unknown
        // colon id is a dangling reference, not a new job.
        if let n = name { names[n] = execId }
        rows[execId] = LedgerRow(
            target: target, verb: verb, retries: retries, attempts: 0, name: name,
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
            // NO `verb` KEY. Python's row is exactly {id, target, status,
            // attempts, retries, named, outcome?} and the ports carried an eighth
            // key, so the same wiring answered a 7-key row here and an 8-key row
            // there. The verb is not lost — it is what the SIDECAR stores
            // (`settledSnapshot`), where it authorizes a joiner after a reboot;
            // the read verb never needed it.
            var o: OrderedDictionary<String, JSON> = [
                "id": .integer(id),
                "target": .string(r.target.value),
                "status": .string(r.outcome?.status.rawValue ?? "in_flight"),
                "attempts": .integer(Int64(r.attempts)),
                "retries": .integer(Int64(r.retries)),
                "named": .bool(r.named),
            ]
            if let n = r.name { o["name"] = .string(n) }
            if let out = r.outcome { o["outcome"] = out.asJSON }
            return .object(o)
        }
    }

    /// Every SETTLED row, as the durable sidecar stores them. In-flight rows are
    /// deliberately absent: work that never completed did not complete, and a
    /// post-reboot re-send re-running it is the truth, not a loss.
    public func settledSnapshot() -> JSON {
        lock.lock()
        defer { lock.unlock() }
        var out: OrderedDictionary<String, JSON> = [:]
        for id in order {
            guard let r = rows[id], let outcome = r.outcome else { continue }
            out[String(id)] = .object([
                "target": .string(r.target.value),
                "name": r.name.map { JSON.string($0) } ?? .null,
                "retries": .integer(Int64(r.retries)),
                "attempts": .integer(Int64(r.attempts)),
                "named": .bool(r.named),
                "verb": .string(r.verb),
                "origin": r.origin.map { JSON.string($0.value) } ?? .null,
                "outcome": outcome.asJSON,
            ])
        }
        return .object(["v": .integer(1), "rows": .object(out)])
    }

    /// Restore SETTLED rows from a sidecar, DONE, so dedup holds across a
    /// reboot: a completed step is never re-executed by a fresh process and a
    /// parked dead letter stays dead.
    @discardableResult
    public func rehydrate(_ doc: JSON) -> Int {
        guard let src = doc["rows"].asObject else { return 0 }
        lock.lock()
        defer { lock.unlock() }
        var n = 0
        for (key, r) in src {
            guard let id = Int64(key), let target = r["target"].asString,
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
                verb: r["verb"].asString ?? "",
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
    public func ledgerSettled() {
        ledgerFlusherLock.lock()
        let f = _ledgerFlusher
        ledgerFlusherLock.unlock()
        f?()
    }
}
