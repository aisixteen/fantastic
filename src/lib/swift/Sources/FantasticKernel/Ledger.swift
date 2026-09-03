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
    case join(verb: String)
    /// Already settled: this IS the answer; the handler must not run again.
    case settled(Outcome, verb: String)
    /// The id is unusable — the refusal to hand back verbatim.
    case refused(JSON)
}

/// One execution and what it became.
private struct Row {
    var target: AgentId
    /// The verb this execution ran — the question the gate must be asked when
    /// somebody joins it.
    var verb: String
    var retries: UInt32
    var attempts: UInt32
    var named: Bool
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
    private var rows: [String: Row] = [:]
    /// Insertion order, for the ring. Mirrors python's dict ordering.
    private var order: [String] = []
    private var cap: Int?

    public init(cap: Int? = defaultLedgerCap) { self.cap = cap }

    /// Claim `execId` for an execution, or discover it is already taken.
    ///
    /// `named` distinguishes a caller-chosen id from a minted pid: only the
    /// former is a dedup contract, and the ring evicts the latter first.
    public func open(
        _ execId: String, target: AgentId, verb: String, retries: UInt32, named: Bool,
        origin: AgentId?
    ) -> Opened {
        lock.lock()
        defer { lock.unlock() }

        if let row = rows[execId] {
            // AN ID IS BOUND TO ITS FIRST TARGET. The same id aimed elsewhere is
            // a different intent wearing the same name — returning the other
            // execution's outcome would be silently wrong.
            if row.target != target {
                return .refused(
                    Refusal.make(
                        Refusal.badArg,
                        "send: id \"\(execId)\" is bound to \(row.target.value) — a named "
                            + "execution cannot be re-aimed",
                        extra: [
                            ("arg", .string("id")), ("bound_to", .string(row.target.value)),
                        ]))
            }
            if let o = row.outcome { return .settled(o, verb: row.verb) }
            return .join(verb: row.verb)
        }

        // A COLON ID REFERENCES an existing minted execution; it never creates
        // one. The caller cannot mint into the kernel's namespace, so an unknown
        // colon id is a dangling reference, not a new job.
        if named && execId.contains(":") {
            return .refused(
                Refusal.make(
                    Refusal.badArg,
                    "send: no execution \"\(execId)\" — a pid REFERENCES, it never creates",
                    extra: [("arg", .string("id"))]))
        }

        rows[execId] = Row(
            target: target, verb: verb, retries: retries, attempts: 0, named: named,
            origin: origin,
            outcome: nil)
        order.append(execId)
        evict()
        return .fresh
    }

    /// Count one dispatch attempt; returns the new count.
    @discardableResult
    /// Whether a row exists under `execId` (named or minted, settled or not).
    public func has(_ execId: String) -> Bool {
        lock.withLock { rows[execId] != nil }
    }

    public func attempt(_ execId: String) -> UInt32 {
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
    public func shouldRetry(_ execId: String, _ status: Status) -> Bool {
        guard status.isRetryable else { return false }
        lock.lock()
        defer { lock.unlock() }
        guard let r = rows[execId] else { return false }
        return r.attempts <= r.retries
    }

    /// Record the FINAL outcome. Stamps `attempts`, so a caller sees what its
    /// work actually cost.
    @discardableResult
    public func settle(_ execId: String, _ outcome: Outcome) -> Outcome {
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
    public func settledOutcome(_ execId: String) -> Outcome? {
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
                "id": .string(id),
                "target": .string(r.target.value),
                "status": .string(r.outcome?.status.rawValue ?? "in_flight"),
                "attempts": .integer(Int64(r.attempts)),
                "retries": .integer(Int64(r.retries)),
                "named": .bool(r.named),
            ]
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
            out[id] = .object([
                "target": .string(r.target.value),
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
        for (id, r) in src {
            guard let target = r["target"].asString,
                let statusText = r["outcome"]["status"].asString,
                let status = Status(rawValue: statusText)
            else { continue }
            let outcome = Outcome(
                pid: r["outcome"]["pid"].asString.map { TaskId($0) },
                status: status,
                result: r["outcome"]["result"],
                attempts: UInt32(r["outcome"]["attempts"].asInt ?? 1))
            rows[id] = Row(
                target: AgentId(target),
                verb: r["verb"].asString ?? "",
                retries: UInt32(r["retries"].asInt ?? 0),
                attempts: UInt32(r["attempts"].asInt ?? 1),
                named: r["named"].asBool ?? true,
                origin: r["origin"].asString.map { AgentId($0) },
                outcome: outcome)
            order.append(id)
            n += 1
        }
        evict()
        return n
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
