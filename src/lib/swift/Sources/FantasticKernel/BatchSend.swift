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

        // The item's OWN fields are validated before anything is resolved, gated
        // or minted (python order). `type` is the verb; there is no send without
        // one — a typeless payload is a coded refusal for EVERY sender, never an
        // `unhandled` from the target (measured 2026-09-02).
        guard case .object(let obj) = item.payload else {
            return .settled(Outcome.refused(Refusal.missing("send", "payload")))
        }
        guard let typeV = obj["type"] else {
            return .settled(Outcome.refused(Refusal.missing("send", "payload.type")))
        }
        guard let verb = typeV.asString, !verb.isEmpty else {
            return .settled(
                Outcome.refused(Refusal.bad("send", "payload.type", "a non-empty string")))
        }
        if let id = item.id {
            // ONE ID SPACE: a caller id may not be empty and may not sit in the
            // kernel-minted `<agent>:<hex>` namespace unless it NAMES an existing
            // execution — minted ids are referenced, never created.
            if id.isEmpty {
                return .settled(Outcome.refused(Refusal.bad("send", "id", "a non-empty string")))
            }
        }

        // A DYING TASK REGISTERS NO NEW WORK. `cancelTask` marks the whole
        // subtree before it aborts anything, because `onCancel` hooks await —
        // so a doomed handler gets scheduling turns on its way out, and what it
        // sent there used to start fresh dispatches that outlived the cancel.
        // Settled here, before an id is minted: a refused send costs nothing.
        if KernelTaskLocals.currentKilled?.isKilled == true {
            return .settled(
                Outcome(pid: nil, status: .cancelled, result: .object(["cancelled": .bool(true)])))
        }
        // WHILE DRAINING, NO NEW ROOTS. A send from inside live work (a parent
        // task in the table — including an agent's own `shutdown` turn using its
        // tools) passes; the kernel's own shutdown traffic passes; everything
        // else is a new root and is refused before it costs an id.
        // The root's own control surface stays open while draining: kernel verbs
        // (`shutdown_agents` again, `shutdown_kernel`, `ps`, `messages`,
        // `ledger`) and `reflect` are how an operator watches and finishes the
        // stop. Python `Agent._launch` (`kernel/_agent.py`).
        if draining && !KernelTaskLocals.inShutdown {
            let toRoot = item.target.value == "kernel" || root?.id == item.target
            let control = toRoot && (Kernel.isSystemVerb(verb) || verb == "reflect")
            let cur = KernelTaskLocals.currentTask
            let insideLiveWork = cur.map { tasks.contains($0) } ?? false
            if !control && !insideLiveWork && !tasks.isRegistered() {
                return .settled(
                    Outcome.refused(
                        Refusal.make(
                            Refusal.draining,
                            "send: the kernel is draining for shutdown — no new work is "
                                + "accepted; in-flight work may finish")))
            }
        }

        // Resolve target. Special id "kernel" aliases to the root.
        let resolved: Agent? = item.target.value == "kernel" ? root : agent(item.target)
        guard let target = resolved else {
            return .settled(
                Outcome.refused(
                    Refusal.unknown(item.target.value, live: allAgents().map { $0.id.value })))
        }

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
        // ONE ID SPACE, ONE HANDLE. A NAMED execution registers in the task
        // table under the caller's own id, so `ps` shows that id and `cancel
        // task_id=<id>` kills it — the id the caller already holds IS the
        // handle. Minting a second, private pid for a named item meant the
        // caller could not cancel or find its own execution (measured
        // 2026-09-02). An unnamed item still mints, synchronously.
        // ONE ID SPACE. A caller-named execution JOINS the id its name opened;
        // an unnamed one takes the next int — the same allocator that mints
        // every row, so a pid, a reply tag and a ledger row agree.
        let name = item.id
        let execId: Int64 = name.flatMap { ledger.resolveName($0) } ?? rows.mintId()
        let taskId = TaskId(execId)
        let retries = item.retries ?? 0
        let origin = KernelTaskLocals.currentSender

        switch ledger.open(
            execId, target: target.id, verb: verb, retries: retries, name: name,
            origin: origin)
        {
        case .refused(let r):
            return .settled(Outcome.refused(r))
        // ALREADY SETTLED — the recorded outcome IS the answer, and the handler
        // must not run a second time. Idempotency and once-only in one field.
        case .settled(let o, let origVerb, _):
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
        case .join(let origVerb, let jid):
            if let d = joinDenial(origin: origin, verb: origVerb, target: target) {
                return .settled(Outcome.refused(d))
            }
            return .joining(jid, item.timeout)
        case .fresh:
            // THE CALL ROW, on the TARGET, under the pre-minted id — the trace of
            // who was asked what, parented to the dispatch that asked. Python
            // `rows_put(target, {id, kind:"call", verb, parent})`.
            var callRow: Row = [
                "id": .integer(execId), "kind": .string("call"), "verb": .string(verb),
            ]
            callRow["parent"] = KernelTaskLocals.currentTask.map { JSON.integer($0.value) } ?? .null
            // `origin` says who asked; absent (an operator's call) is what the
            // loader's message pump keys its own-traffic skip on.
            callRow["origin"] = origin.map { JSON.string($0.value) } ?? .null
            if let name { callRow["name"] = .string(name) }
            _ = try? rows.put(target.id, callRow)
        }

        return .dispatch(
            Dispatch(
                target: target, payload: item.payload, verb: verb,
                timeout: item.timeout, execId: execId, taskId: taskId,
                binary: item.binary))
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
                    taskOut: box, timeout: d.timeout, taskId: nextPid, binary: d.binary)
                reply = r
                status = s
                pid = box.value
                if !ledger.shouldRetry(d.execId, s) { break }
                // Same exec id across attempts; a fresh task row per attempt,
                // because the previous one is already forgotten. Cancelling the
                // execution kills the current attempt AND this loop — a cancel
                // is absorbing.
                // A retry is the SAME execution, so it re-registers under the
                // SAME handle: cancelling that id kills the attempt AND the loop.
                nextPid = d.taskId
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
            // THE OUTCOME IS A ROW TOO: the call row learns how it ended, and a
            // `settle` row lands on the ROOT — never on the target's own list,
            // which readers treat as the CONVERSATION. Python `_stamp_call`.
            // The stamp is python's `{status, error?, reason?}` — an error's first
            // 200 characters, so a cold read of the log can say how a call ended.
            var stamp: Row = ["status": .string(status.rawValue)]
            if let err = reply["error"].asString, !err.isEmpty {
                stamp["error"] = .string(String(err.prefix(200)))
                if let reason = reply["reason"].asString, !reason.isEmpty {
                    stamp["reason"] = .string(reason)
                }
            }
            rows.stamp(d.execId, ["outcome": .object(stamp)])
            if let r = root {
                let settleRow: Row = [
                    "kind": .string("settle"), "parent": .integer(d.execId),
                    "settled": .string(d.target.id.value), "outcome": .object(stamp),
                ]
                _ = try? rows.put(r.id, settleRow)
            }
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
    func joinExecution(_ execId: Int64, timeout: Double?) async -> Outcome {
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
    case joining(Int64, Double?)
    /// Passed the gate and claimed its row.
    case dispatch(Dispatch)
}

/// One item on its way to a handler.
struct Dispatch {
    let target: Agent
    let payload: JSON
    let verb: String
    let timeout: Double?
    /// The ledger key AND the task-table handle: one int from the one counter.
    /// A caller's name resolves to it; a retry re-registers under the same id.
    let execId: Int64
    let taskId: TaskId
    /// A RAW request body: present ⇒ dispatch through the BINARY channel.
    let binary: Data?
}
