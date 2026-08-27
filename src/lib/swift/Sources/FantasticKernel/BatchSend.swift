// ARRAY-ONLY batch send: `[Item] -> [Outcome]`.
//
// Port of python `kernel/_agent.py:Agent.send` (canonical), `tmp/pychanges.md`
// #29 + #34–#37.
//
// There is deliberately NO single-call form — one shape prevents single/batch
// drift. A single call is `(await kernel.send([Item(t, p)]))[0].result`.
//
// Why a batch at all: a model turn that fans out N calls used to get its answers
// spread over the next N turns and re-asked in between (measured 2026-08-25:
// 7 calls → replies `1 of 7`…`7 of 7`, `list_agents` re-asked five times,
// nothing built). The industry shape is the same — all results of one turn
// return together.

import FantasticJSON
import Foundation

extension Kernel {
    /// Dispatch every item CONCURRENTLY; the returned array preserves input
    /// order.
    ///
    /// Every dispatched item is a REGISTERED task (one `ps` row, `cancel`-able by
    /// the pid the outcome carries). Pids are minted in a SYNCHRONOUS launch
    /// phase, so the whole batch is visible in `ps` before the first await.
    ///
    /// `timeout` is per-item seconds and the kernel NEVER defaults it: absent =
    /// wait forever. On expiry the item is killed through the FRONT DOOR
    /// (`cancelTask`), so every bundle's `onCancel` hook runs before the abort —
    /// dropping the task would strand the OS resources those hooks free.
    ///
    /// Nothing propagates out: a handler that throws settles as `failed` with
    /// `raised: true`, a cancel settles as `cancelled`.
    public func send(_ items: [Item]) async -> [Outcome] {
        // SYNCHRONOUS launch: resolve, gate and claim the ledger row for EVERY
        // item before the first await, so `ps` shows the whole batch rather than
        // it appearing one row at a time.
        var launched: [Launched] = []
        launched.reserveCapacity(items.count)
        for item in items { launched.append(launch(item)) }

        // Concurrent settle. Swift's structured child tasks INHERIT the enclosing
        // task-locals, so `currentSender`/`currentTask`/`currentOrigin` survive
        // the fan-out untouched — the property tokio does not have.
        var out = [Outcome?](repeating: nil, count: launched.count)
        await withTaskGroup(of: (Int, Outcome).self) { group in
            for (i, l) in launched.enumerated() {
                group.addTask { (i, await self.settle(l)) }
            }
            for await (i, o) in group { out[i] = o }
        }
        return out.map { $0 ?? Outcome.refused(.object([:])) }
    }

    /// One item's SYNCHRONOUS half: validate, resolve, gate, claim the row.
    func launch(_ item: Item) -> Launched {
        if let t = item.timeout, !(t.isFinite && t > 0) {
            return .settled(
                Outcome.refused(Refusal.bad("send", "timeout", "a positive number of seconds")))
        }

        // Resolve target. Special id "kernel" aliases to the root.
        let resolved: Agent? = item.target.value == "kernel" ? root : agent(item.target)
        guard let target = resolved else {
            return .settled(
                Outcome.refused(
                    Refusal.unknown(item.target.value, live: allAgents().map { $0.id.value })))
        }

        let verb = item.payload["type"].asString ?? ""

        // ── CALL GATE — fully-gated, two-sided, from-above ──────────────
        // An operator send (`currentSender` is nil) is the origin root of trust
        // and never reaches here. Every AGENT send — ANY verb, `reflect`
        // included — needs BOTH ends to grant the edge.
        if let orig = KernelTaskLocals.currentSender {
            if let denial = gate(sender: orig, verb: verb, target: target, payload: item.payload) {
                return .settled(Outcome.refused(denial))
            }
        }

        // MINT SYNCHRONOUSLY. The pid must exist before the first await — it is
        // the ledger key for an unnamed item, and it is what makes the WHOLE
        // batch visible in `ps` at once.
        let taskId = tasks.mint(target.id)
        // ONE ID SPACE: a caller's id names the execution; without one, the
        // minted pid does. Either way there is exactly one handle.
        let named = item.id != nil
        let execId = item.id ?? taskId.value
        let retries = item.retries ?? 0
        let origin = KernelTaskLocals.currentSender

        switch ledger.open(
            execId, target: target.id, verb: verb, retries: retries, named: named,
            origin: origin)
        {
        case .refused(let r):
            return .settled(Outcome.refused(r))
        // ALREADY SETTLED — the recorded outcome IS the answer, and the handler
        // must not run a second time. Idempotency and once-only in one field.
        case .settled(let o, let origVerb):
            // AUTHORIZED AGAINST THE EXECUTION IT JOINS. The recorded outcome
            // belongs to whoever ran it, so handing it back is equivalent to
            // having made THAT call — the gate must be asked that question, not
            // about the verb this caller happened to send. Measured in python
            // 2026-08-28: a caller granted only a harmless verb reused another's
            // id and received its secret verbatim.
            if let d = joinDenial(origin: origin, verb: origVerb, target: target) {
                return .settled(Outcome.refused(d))
            }
            return .settled(o)
        // IN FLIGHT — join it. A joiner's own `timeout` bounds only ITS wait: it
        // does not own the execution and must not kill it.
        case .join(let origVerb):
            if let d = joinDenial(origin: origin, verb: origVerb, target: target) {
                return .settled(Outcome.refused(d))
            }
            return .joining(execId, item.timeout)
        case .fresh:
            break
        }

        return .dispatch(
            Dispatch(
                target: target, payload: item.payload, verb: verb,
                timeout: item.timeout, execId: execId, taskId: taskId))
    }

    /// One item's ASYNC half.
    ///
    /// Runs the dispatch, enforces the item's own deadline, and — this is the
    /// RETRY ENGINE — re-dispatches a terminal `failed`/`timed_out` under the
    /// SAME execution id while the budget lasts. Only the FINAL outcome settles
    /// the ledger, so a joiner never sees an attempt that was later retried away.
    func settle(_ launched: Launched) async -> Outcome {
        switch launched {
        case .settled(let o):
            return o
        case .joining(let execId, let timeout):
            return await joinExecution(execId, timeout: timeout)
        case .dispatch(let d):
            var nextPid = d.taskId
            var pid: TaskId? = nil
            var reply: JSON = .null
            var status: Status = .completed
            while true {
                ledger.attempt(d.execId)
                let box = TaskIdBox()
                let (r, s) = await runDispatchOutcome(
                    target: d.target, payload: d.payload, verb: d.verb,
                    taskOut: box, timeout: d.timeout, taskId: nextPid)
                reply = r
                status = s
                pid = box.value
                if !ledger.shouldRetry(d.execId, s) { break }
                // Same exec id across attempts; a fresh task row per attempt,
                // because the previous one is already forgotten. Cancelling the
                // execution kills the current attempt AND this loop — a cancel
                // is absorbing.
                nextPid = tasks.mint(d.target.id)
            }

            if d.verb == "reflect" && status == .completed {
                applyReflectFlags(d.target, d.payload, &reply)
            }
            // State event + watcher fanout (synchronous; cheap subscribers).
            publishState(
                .object([
                    "type": .string("send"),
                    "sender": .string(
                        (KernelTaskLocals.currentSender ?? d.target.id).value),
                    "target": .string(d.target.id.value),
                    "verb": .string(d.verb),
                    "summary": .string(summarize(d.payload)),
                ]))
            fanoutToWatchers(d.target, d.payload)

            let out = ledger.settle(d.execId, Outcome(pid: pid, status: status, result: reply))
            // The DURABLE half, if a store was wired. The kernel does not know
            // what this writes or where.
            ledgerSettled()
            return out
        }
    }

    /// Wait for an execution someone else owns.
    ///
    /// The joiner's `timeout` bounds only its OWN wait — on expiry it settles
    /// `timed_out` and the execution KEEPS RUNNING, because a joiner does not own
    /// it and killing another caller's work would be a surprise.
    func joinExecution(_ execId: String, timeout: Double?) async -> Outcome {
        let deadline = timeout.map { Date().addingTimeInterval($0) }
        while true {
            if let o = ledger.settledOutcome(execId) { return o }
            if let d = deadline, Date() >= d {
                return Outcome(
                    pid: nil, status: .timedOut,
                    result: Refusal.make(
                        Refusal.timeout,
                        "joined execution did not settle within \(timeout!)s"))
            }
            try? await Task.sleep(nanoseconds: 2_000_000)
        }
    }
}

/// A launched batch item.
enum Launched {
    /// Nothing entered the task table — `pid` stays nil.
    case settled(Outcome)
    /// Already in flight under this id: wait for ITS outcome, run nothing.
    case joining(String, Double?)
    /// Passed the gate and claimed its row.
    case dispatch(Dispatch)
}

/// One item on its way to a handler.
struct Dispatch {
    let target: Agent
    let payload: JSON
    let verb: String
    let timeout: Double?
    /// The ledger key: the caller's id, or the minted pid for an unnamed item.
    let execId: String
    /// Minted in the launch phase. A retry re-dispatches under a FRESH pid but
    /// the SAME exec id, so the execution keeps one handle across attempts.
    let taskId: TaskId
}
