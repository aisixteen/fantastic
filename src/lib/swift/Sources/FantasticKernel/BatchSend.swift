// ARRAY-ONLY batch send: `[Item] -> [Outcome]`.
//
// Port of python `kernel/_agent.py:Agent.send` (canonical), `tmp/pychanges.md`
// #29 + #34–#37.
//
// There is deliberately NO single-call form — one shape prevents single/batch
// drift. A single call is `(await kernel.send([Item(t, p)]))[0].result`.
//
// Why a batch at all: a model batch that fans out N calls used to get its answers
// spread over the next N messages and re-asked in between (measured 2026-08-25:
// 7 calls → replies `1 of 7`…`7 of 7`, `list_agents` re-asked five times,
// nothing built). The industry shape is the same — all results of one batch
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
        // NAMED EXECUTIONS OUTLIVE RAM. Once flushed, a settled execution is only
        // in the kernel's store; an item naming one is looked up there FIRST, so
        // the synchronous launch below finds it exactly as if it had never left
        // RAM. Python `Agent.send` → `recall_executions`.
        //
        // A STORE THAT FAILS THE LOOKUP: python's `_store_read` raises out of
        // `send` itself. Swift's `send` does not throw, so every item of the batch
        // settles `failed` with that raise as its body — the caller learns the
        // same thing, and nothing is dispatched on a lookup that did not happen.
        //
        // TODO(parity, owner 2026-10-02 — left as is, rare failure): python's raise
        // also reaches the WIRE: `web_rest` answers HTTP 500 for the whole request,
        // where swift's legs answer 200 with every item failed. Mirroring it needs
        // `send` to throw — either `send` itself becomes `throws` (changes every
        // caller) or a separate throwing entry point used only by the wire legs (two
        // entry points). Both outcomes are loud and carry the store's error text, so
        // only the response SHAPE differs.
        if storeId != nil, items.contains(where: { item in
            let raw = item.rawId ?? item.id.map { JSON.string($0) }
            return raw != nil && raw != .null
        }) {
            do {
                try await recallExecutions(items)
            } catch {
                let err: JSON = .object([
                    "error": .string(error.description), "raised": .bool(true),
                ])
                return items.map { _ in Outcome(pid: nil, status: .failed, result: err) }
            }
        }
        // SYNCHRONOUS launch: resolve, gate and claim the ledger row for EVERY
        // item before the first await, so `ps` shows the whole batch rather than
        // it appearing one row at a time.
        var launched: [Launched] = []
        launched.reserveCapacity(items.count)
        for item in items { launched.append(launch(item)) }

        // Concurrent settle. Swift's structured child tasks INHERIT the enclosing
        // task-locals, so `currentSender`/`currentTask`/`currentOrigin` survive
        // the fan-out untouched — the property tokio does not have.
        //
        // STARTED IN INPUT ORDER. A group starts its children in any order, and a
        // child's first act is its dispatch prologue — the fanout that puts the
        // item in its target's inbox and its watchers' streams. Python creates the
        // dispatch tasks in order and each runs that prologue before the next one
        // starts, so a batch of two items to one agent reaches it in the order it
        // was written. Here each child is started only once the one before it has
        // fanned out (`onFanout`); they still SETTLE concurrently. An item with no
        // dispatch (settled at launch, or joining another execution) lets the next
        // one go at once — a join must never hold the rest of the batch.
        var out = [Outcome?](repeating: nil, count: launched.count)
        await withTaskGroup(of: (Int, Outcome).self) { group in
            for (i, l) in launched.enumerated() {
                let started = StartSignal()
                group.addTask {
                    guard case .dispatch = l else {
                        started.fire()
                        return (i, await self.settle(l))
                    }
                    let o = await KernelTaskLocals.$onFanout.withValue({ started.fire() }) {
                        await self.settle(l)
                    }
                    started.fire()  // a dispatch that ended before its prologue
                    return (i, o)
                }
                await started.wait()
            }
            // Every item has reached its target: the batch is LAUNCHED.
            KernelTaskLocals.onBatchLaunched?()
            for await (i, o) in group { out[i] = o }
        }
        return out.map { $0 ?? Outcome.refused(.object([:])) }
    }

    /// One item's SYNCHRONOUS half: validate, resolve, gate, claim the row.
    /// Python `Agent._launch` (`kernel/_agent.py:601-732`) — C30's check
    /// order: target → payload dict → timeout → payload.type → id → retries
    /// → killed → draining → resolve → gate.
    /// Is this send the root's CONTROL SURFACE — a kernel verb or `reflect`
    /// on the root — directly, or carried by a wire leg's `inbound_call`? The
    /// drain gate keeps it open while the kernel stops. Python
    /// `Agent._is_control`.
    func isControl(target: String, payload: JSON) -> Bool {
        var target = target
        var payload = payload
        if payload["type"].asString == "inbound_call" {
            let carried = payload["item"]
            guard carried["payload"].asObject != nil else { return false }
            target = carried["target"].asString ?? ""
            payload = carried["payload"]
        }
        let verb = payload["type"].asString ?? ""
        let toRoot = target == "kernel" || root?.id.value == target
        return toRoot && (Kernel.isSystemVerb(verb) || verb == "reflect")
    }

    func launch(_ item: Item) -> Launched {
        // 1. target — python: `if not isinstance(target_id, str) or not
        // target_id: missing("send", "target")`.
        if item.target.value.isEmpty {
            return .settled(Outcome.refused(Refusal.missing("send", "target")))
        }
        // 2. payload dict.
        guard case .object(let obj) = item.payload else {
            return .settled(Outcome.refused(Refusal.missing("send", "payload")))
        }
        // 2b. A RAW BODY sits at a slot of its payload — where python's dict holds
        // the bytes. A path that names none could never be mirrored, recorded or
        // read by the handler; every wire leg places its body before it gets here,
        // so this only refuses an in-process caller's typo. Rust `_launch` 2b.
        if let blob = item.binary {
            var probe = item.payload
            do {
                try pySetPath(&probe, parts: blob.parts, .null)
            } catch {
                return .settled(
                    Outcome.refused(
                        Refusal.badGot(
                            "send", "binary", "a body path naming a slot of the payload",
                            got: .array(blob.parts.map(\.json)))))
            }
        }
        // 3. timeout — `item.rawTimeout` (C38) supersedes the typed `item.
        // timeout` when a caller is forwarding a value it did not itself
        // parse.
        let timeout: Deadline?
        switch resolveTimeout(item) {
        case .ok(let t): timeout = t
        case .refused(let r): return .settled(Outcome.refused(r))
        }
        // 4. payload.type — `type` is the verb; there is no send without one.
        // Validated for EVERY sender (operator included) BEFORE the gate, so
        // a typeless payload is a coded refusal, never an `unhandled` from
        // the target (measured 2026-09-02).
        // A RAW BODY at `type` is python's `bytes` value there.
        let bodyAtType = item.binary?.at("type") != nil
        let typeV: JSON
        if let v = obj["type"] {
            typeV = v
        } else if bodyAtType {
            typeV = .null
        } else {
            return .settled(Outcome.refused(Refusal.missing("send", "payload.type")))
        }
        guard let verb = typeV.asString, !verb.isEmpty, !bodyAtType else {
            // A non-string is named by its TYPE, never echoed: the octet REST leg
            // and a binary WS frame can put a whole body here (audit 2026-10-03,
            // W-11) — python's `type(verb).__name__`.
            let got: String
            if bodyAtType {
                got = "bytes"
            } else if case .string = typeV {
                got = pyRepr(typeV)
            } else {
                got = pyTypeName(typeV)
            }
            return .settled(
                Outcome.refused(
                    Refusal.make(
                        Refusal.badArg,
                        "send: payload.type must be a non-empty string, got \(got)",
                        extra: [("arg", .string("payload.type"))])))
        }
        // 5. id — a NAME (non-empty string) opens-or-joins by name; an int
        // JOINS an existing execution or refuses; anything else is `bad_arg`.
        let joinId: Int64?
        let name: String?
        let callerId: JSON
        switch resolveExecId(item) {
        case .ok(let v): (joinId, name, callerId) = v
        case .refused(let r): return .settled(Outcome.refused(r))
        }
        // 6. retries — a non-negative int, never defaulted otherwise.
        let retries: UInt32
        switch resolveRetries(item) {
        case .ok(let r): retries = r
        case .refused(let r): return .settled(Outcome.refused(r))
        }
        // 7. killed — A DYING TASK REGISTERS NO NEW WORK. `cancelTask` marks
        // the whole subtree before it aborts anything, because `onCancel`
        // hooks await — so a doomed handler gets scheduling passes on its way
        // out, and what it sent there used to start fresh dispatches that
        // outlived the cancel. Settled here, before an id is minted: a
        // refused send costs nothing.
        if KernelTaskLocals.currentKilled?.isKilled == true {
            return .settled(
                Outcome(pid: nil, status: .cancelled, result: .object(["cancelled": .bool(true)])))
        }
        // 8. draining — WHILE DRAINING, NO NEW ROOTS. A send from inside live
        // work (a parent task in the table — including an agent's own
        // `shutdown` pass using its tools) passes; the kernel's own shutdown
        // traffic passes; everything else is a new root and is refused
        // before it costs an id. The root's own control surface stays open
        // while draining: kernel verbs (`shutdown_agents` again, `shutdown_
        // kernel`, `ps`, `messages`, `ledger`) and `reflect` are how an
        // operator watches and finishes the stop. Over a wire the operator's
        // call arrives as a leg's `inbound_call` carrying it — judged by the
        // call it carries, or a remote operator could never finish the stop it
        // started. Python `Agent._send` (`kernel/_agent.py`).
        if draining && !KernelTaskLocals.inShutdown {
            let control = isControl(target: item.target.value, payload: item.payload)
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

        // 9. resolve — `"kernel"` names the root, else a literal id, else a
        // QUALIFIED `<guid>|<agent id>`: this kernel's own guid names a local
        // agent, a connected peer's guid an agent there (cross-kernel addressing).
        guard
            let target = resolveAgent(item.target.value)
                ?? qualifiedTarget(item.target.value)
        else {
            return .settled(
                Outcome.refused(
                    Refusal.unknown(item.target.value, live: allAgents().map { $0.id.value })))
        }

        // 10. ── CALL GATE — fully-gated, two-sided, from-above ────────────
        // An operator send (`currentSender` is nil — made outside every
        // dispatch) is the origin root of trust and never reaches here. Every AGENT send — ANY verb, `reflect`
        // included — needs BOTH ends to grant the edge.
        if let orig = KernelTaskLocals.currentSender {
            if let denial = gate(sender: orig, verb: verb, target: target) {
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
        // ONE ID SPACE. A caller-named/joined execution reuses the id it
        // already opened; an unnamed one takes the next int — the same
        // allocator that mints every row, so a pid, a reply tag and a
        // ledger row agree.
        // ── KERNEL-REFLECT FAST PATH ──────────────────────────────────────
        // `reflect` on the `kernel` ADDRESS (the literal target, not the root's
        // own id — python keys on `target_id == "kernel"`) is a PURE IDENTITY
        // READ. Python answers it without a task-table row, and for an UNNAMED
        // item without a ledger row and WITHOUT MINTING AN ID at all
        // (`Agent._launch`: the check sits before the mint, `_Dispatched(pid=
        // None)`). Swift dispatched it like any other verb, so every AI pass's
        // primer reflect minted a `call · reflect · <root> ← ai` row plus a
        // settle python never writes — 55 extra rows, measured 2026-09-21.
        let kernelReflect = item.target.value == "kernel" && verb == "reflect"
        if kernelReflect, joinId == nil, name == nil {
            return .reflectFast(
                ReflectFast(target: target, payload: item.payload, execId: nil))
        }
        let execId: Int64 = joinId ?? mintId()
        let taskId = TaskId(execId)
        let caller = KernelTaskLocals.currentSender

        switch ledger.open(
            execId, target: target.id, payload: item.payload, retries: retries,
            name: name, caller: caller, callerId: callerId)
        {
        case .refused(let r):
            return .settled(Outcome.refused(r))
        // ALREADY SETTLED — the recorded outcome IS the answer, and the handler
        // must not run a second time. Idempotency and once-only in one field.
        case .settled(let o, let origPayload, _):
            // AUTHORIZED AGAINST THE EXECUTION IT JOINS. The recorded outcome
            // belongs to whoever ran it, so handing it back is equivalent to
            // having made THAT call — the gate must be asked that question, not
            // about the verb this caller happened to send. Measured in python
            // 2026-08-28: a caller granted only a harmless verb reused another's
            // id and received another caller's result verbatim.
            if let d = joinDenial(origin: caller, payload: origPayload, target: target) {
                return .settled(Outcome.refused(d))
            }
            return .settled(o)
        // IN FLIGHT — join it. A joiner's own `timeout` bounds only ITS wait: it
        // does not own the execution and must not kill it.
        case .join(let origPayload, let jid, let box):
            if let d = joinDenial(origin: caller, payload: origPayload, target: target) {
                return .settled(Outcome.refused(d))
            }
            return .joining(jid, timeout, box)
        case .fresh:
            // THE CALL ROW, on the TARGET, under the pre-minted id — the trace of
            // who was asked what, parented to the dispatch that asked. It carries
            // the WHOLE call — the dispatch payload verbatim (raw bytes reduced to
            // `{len}` at their path), the retry budget, the attempt count as at
            // dispatch, the deadline — and is what the kernel flushes into its
            // memory store and reads back for `ledger` and named-call dedup.
            // KEY ORDER IS PYTHON'S: id, kind, verb, parent, caller, payload,
            // named, retries, attempts, timeout[, name]. (`origin` → `caller`:
            // replies own `origin` now.)
            var callRow: Row = [
                "id": .integer(execId), "kind": .string("call"), "verb": .string(verb),
            ]
            callRow["parent"] = KernelTaskLocals.currentTask.map { JSON.integer($0.value) } ?? .null
            callRow["caller"] = caller.map { JSON.string($0.value) } ?? .null
            callRow["payload"] = Kernel.tapedPayload(item.payload, item.binary)
            callRow["named"] = .bool(name != nil)
            callRow["retries"] = .integer(Int64(retries))
            callRow["attempts"] = .integer(1)
            // The RAW timeout when the caller forwarded one (an int stays an
            // int, as python's row does); the typed seconds otherwise.
            if let raw = item.rawTimeout, raw != .null {
                callRow["timeout"] = raw
            } else {
                callRow["timeout"] = timeout.map { JSON.double($0.seconds) } ?? .null
            }
            if let name { callRow["name"] = .string(name) }
            _ = try? rowsPut(target.id, callRow)
        }

        // A CALLER-NAMED kernel reflect: the ledger applies (dedup, join, the
        // recorded outcome) but there is still no task row — nothing to cancel
        // in a pure read. Python `Agent._launch`, the named branch.
        if kernelReflect {
            return .reflectFast(
                ReflectFast(target: target, payload: item.payload, execId: execId))
        }

        return .dispatch(
            Dispatch(
                target: target, payload: item.payload, verb: verb,
                timeout: timeout, execId: execId, taskId: taskId,
                binary: item.binary, rawReply: item.rawReply))
    }

    /// `timeout` validation (C30/C38): `item.rawTimeout`, when forwarded by a
    /// caller relaying an untyped value, SUPERSEDES the typed `item.timeout`.
    /// Python: `if timeout is not None and (isinstance(timeout, bool) or not
    /// isinstance(timeout, (int, float)) or timeout <= 0): refuse`.
    ///
    /// The deadline keeps the value AS GIVEN for the refusal texts: python
    /// formats the raw `d.timeout`, so an int `5` reads `5s`, never `5.0s`.
    private func resolveTimeout(_ item: Item) -> LaunchField<Deadline?> {
        if let raw = item.rawTimeout, raw != .null {
            let value: Double?
            let text: String
            switch raw {
            case .integer(let n):
                value = Double(n)
                text = String(n)
            case .double(let d):
                value = d
                text = pyFloatRepr(d)
            default:
                value = nil
                text = ""
            }
            guard let v = value, v.isFinite, v > 0 else {
                return .refused(
                    Refusal.badGot("send", "timeout", "a positive number of seconds", got: raw))
            }
            return .ok(Deadline(seconds: v, text: text))
        }
        if let t = item.timeout {
            guard t.isFinite, t > 0 else {
                return .refused(
                    Refusal.badGot(
                        "send", "timeout", "a positive number of seconds", got: .double(t)))
            }
            return .ok(Deadline(seconds: t, text: pyFloatRepr(t)))
        }
        return .ok(nil)
    }

    /// `retries` validation (C30/C38): `item.rawRetries` SUPERSEDES the typed
    /// `item.retries` (which is always valid by construction — a `UInt32`
    /// cannot be negative or a bool). Python: `retries = item.get("retries",
    /// 0); if isinstance(retries, bool) or not isinstance(retries, int) or
    /// retries < 0: refuse`.
    private func resolveRetries(_ item: Item) -> LaunchField<UInt32> {
        guard let raw = item.rawRetries, raw != .null else {
            return .ok(item.retries ?? 0)
        }
        guard case .integer(let n) = raw, n >= 0 else {
            return .refused(Refusal.badGot("send", "retries", "a non-negative int", got: raw))
        }
        return .ok(UInt32(truncatingIfNeeded: n))
    }

    /// `id` validation (C30/C38): `item.rawId`, when forwarded by a caller
    /// relaying an untyped value (a model's `<exec_id>` argument, a wire
    /// frame's `id`), SUPERSEDES the typed `item.id`. An int NAMES an
    /// execution to JOIN — refused if none exists under it; a non-empty
    /// string NAMES one to open-or-join; anything else (bool included — a
    /// python bool is an int subclass, but this kernel's `JSON` keeps the
    /// cases distinct so no special test is needed) is `bad_arg`.
    ///
    /// Returns `(joinId, name, callerId)`: `joinId` is set when the caller
    /// named an EXISTING execution; `name` is set only for the string case
    /// (the dedup handle); `callerId` is the raw value AS GIVEN, carried
    /// through for [`ExecutionLedger/open`]'s C31 rebind text.
    private func resolveExecId(_ item: Item) -> LaunchField<(Int64?, String?, JSON)> {
        let raw: JSON? = item.rawId ?? item.id.map { JSON.string($0) }
        guard let raw, raw != .null else { return .ok((nil, nil, .null)) }
        switch raw {
        case .integer(let n):
            guard ledger.has(n) else {
                return .refused(
                    Refusal.make(
                        Refusal.badArg,
                        "send: id \(n) names no known execution — ids are minted by the "
                            + "kernel and can be joined, never created",
                        extra: [("arg", .string("id"))]))
            }
            return .ok((n, nil, raw))
        case .string(let s) where !s.isEmpty:
            return .ok((ledger.resolveName(s), s, raw))
        case .string:
            return .refused(Refusal.bad("send", "id", "a non-empty name"))
        default:
            return .refused(
                Refusal.badGot(
                    "send", "id", "a name (non-empty string) or a message id (int)", got: raw))
        }
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
        case .joining(let execId, let timeout, let box):
            return await joinExecution(execId, timeout: timeout, box: box)
        case .dispatch(let d):
            var nextPid = d.taskId
            var pid: TaskId? = nil
            var reply: JSON = .null
            var status: Status = .completed
            // THE BINARY REPLY BODY, carried out of the batch (C37). It is the
            // carrier for the `bytes` value python's reply dict holds natively,
            // so a leg read/write is an ordinary execution — call row, ledger
            // id, `ps` row, timeout, settle row — instead of a `sendWithBinary`
            // that minted none of them.
            var body: Blob? = nil
            while true {
                let n = ledger.attempt(d.execId)
                // THE ROW COUNTS REAL RUNS. The call row was written with
                // `attempts: 1`; every re-dispatch updates it in place (while it
                // is still in RAM), so the store says what the work cost.
                if n > 1 { rows.stamp(d.execId, ["attempts": .integer(Int64(n))]) }
                let box = TaskIdBox()
                let (r, s, b) = await runDispatchOutcome(
                    target: d.target, payload: d.payload, verb: d.verb,
                    taskOut: box, timeout: d.timeout, taskId: nextPid, binary: d.binary,
                    rawReply: d.rawReply)
                reply = r
                status = s
                body = b
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

            // NOT for a remote target: its kernel already composed the reflect
            // with ITS flags over ITS tree — a reflect of `<guid>|kernel` is that
            // tree, and there is no local agent to render it from.
            if d.verb == "reflect" && status == .completed && !d.target.isRemote {
                // A ROSTER NODE THAT RAISED FAILS THE WHOLE REFLECT: python's
                // exception propagates out of `_roster` into the item's own
                // settle, so the reflect answers that body with `failed`.
                if let raised = await applyReflectFlags(d.target, d.payload, &reply) {
                    reply = raised
                    status = .failed
                }
            }
            // NO FANOUT HERE. Python fans out at DISPATCH START, before the verb
            // is resolved (`_dispatch`), so a watcher sees the traffic while the
            // handler is still running and a retry fans out once per attempt.
            // Publishing after the settle reversed the measured order — rust and
            // swift echoed `job_started` before `start`, and `status(done)` before
            // the send that caused it. `runDispatch` now owns it.
            return recordSettle(
                execId: d.execId, target: d.target.id, pid: pid, status: status,
                reply: reply, body: body)

        case .reflectFast(let f):
            // The identity read itself — python `target._reflect_identity()`
            // wrapped in `_apply_reflect_flags`, run as its own task there and
            // inline here: no dispatch, no task row, no fanout.
            var reply = reflectIdentity(f.target)
            var status: Status = .completed
            if let raised = await applyReflectFlags(f.target, f.payload, &reply) {
                reply = raised
                status = .failed
            }
            // UNNAMED: nothing was opened, so nothing settles — python returns
            // `outcome(None, COMPLETED, reply)` straight out of `_settle`.
            guard let execId = f.execId else {
                return Outcome(pid: nil, status: status, result: reply)
            }
            // NAMED: the row exists, so it is stamped and settled like any other
            // execution — the pid IS the execution id python records.
            return recordSettle(
                execId: execId, target: f.target.id, pid: TaskId(execId),
                status: status == .completed && pyTruthy(reply["error"]) ? .errored : status,
                reply: reply, body: nil)
        }
    }

    /// Settle one execution's ledger row and write what the kernel KEEPS of it:
    /// the outcome stamp on the live call row (while it is still in RAM), the
    /// `settle` row on the root, and — for a dead ending — the `on_deadletter`
    /// announcement. Python `Agent._record` + `_stamp_call`.
    private func recordSettle(
        execId: Int64, target: AgentId, pid: TaskId?, status: Status, reply: JSON, body: Blob?
    ) -> Outcome {
        let named = ledger.isNamed(execId)
        let out = ledger.settle(
            execId, Outcome(pid: pid, status: status, result: reply, body: body))
        // The live call row learns how it ended — python's `{status, error?,
        // reason?}` stamp: an error's first 200 CODE POINTS, `reason` copied RAW.
        // The call row may already be flushed (and gone from RAM) by now.
        var stamp: Row = ["status": .string(status.rawValue)]
        let err = reply["error"]
        if pyTruthy(err) {
            stamp["error"] = .string(
                String(String.UnicodeScalarView(pyStr(err).unicodeScalars.prefix(200))))
            let reason = reply["reason"]
            if pyTruthy(reason) { stamp["reason"] = reason }
        }
        rows.stamp(execId, ["outcome": .object(stamp)])
        if let r = root {
            // THE SETTLE IS ITS OWN ROW — a row is append-only, so the ending is
            // an append, never an edit of the call row. It keeps what
            // `storedOutcome` keeps: a NAMED call's whole result, a dead letter's
            // error text, `{status, bytes}` otherwise. ON THE ROOT: a settle is
            // KERNEL bookkeeping; `settled` names the target.
            let settleRow: Row = [
                "kind": .string("settle"), "parent": .integer(execId),
                "settled": .string(target.value),
                "outcome": .object(
                    storedOutcome(
                        status: status.rawValue,
                        result: Kernel.storeValue(reply, body: body),
                        named: named)),
                "attempts": .integer(Int64(out.attempts)),
            ]
            _ = try? rowsPut(r.id, settleRow)
        }
        // A DEAD ENDING IS ANNOUNCED — nothing retries it on its own: someone has
        // to look.
        if DEAD_STATUSES.contains(status.rawValue) {
            announceDeadletter(execId, status: status, attempts: out.attempts)
        }
        return out
    }

    /// Wait for an execution someone else owns.
    ///
    /// The joiner's `timeout` bounds only its OWN wait — on expiry it settles
    /// `timed_out` and the execution KEEPS RUNNING, because a joiner does not own
    /// it and killing another caller's work would be a surprise.
    func joinExecution(_ execId: Int64, timeout: Deadline?, box: OutcomeBox) async -> Outcome {
        let deadline = timeout.map { Date().addingTimeInterval($0.seconds) }
        while true {
            // THE FUTURE THE OWNER SETTLES, held since launch: the execution may
            // already have been flushed and forgotten by the time we look.
            if let o = box.outcome { return o }
            if let d = deadline, Date() >= d {
                // Python `_agent.py`: `pid` is the JOINED execution's id (not
                // nil — the caller can still `cancel`/`ps` it), the text names
                // it via `{d.pid!r}` (a bare int repr, no quotes) and carries
                // `task_id=d.pid`. The execution ITSELF keeps running — a
                // joiner does not own it.
                return Outcome(
                    pid: TaskId(execId), status: .timedOut,
                    result: Refusal.make(
                        Refusal.timeout,
                        "send: joined execution \(execId) did not settle within this "
                            + "caller's \(timeout!.text)s wait; the execution itself "
                            + "keeps running",
                        extra: [("task_id", .integer(execId))]))
            }
            try? await Task.sleep(nanoseconds: 2_000_000)
        }
    }
}

/// One `launch()` field check's outcome: the validated value, or the
/// refusal to settle the item with. `JSON`, not `Error` — a refusal is DATA
/// (the wire reply), not a swift error — so this stands in for `Result`,
/// which requires an `Error`-conforming failure type.
enum LaunchField<T> {
    case ok(T)
    case refused(JSON)
}

/// A launched batch item.
enum Launched {
    /// Nothing entered the task table — `pid` stays nil.
    case settled(Outcome)
    /// The kernel-reflect fast path: a pure identity read that never reaches
    /// the task table (python `_Dispatched(pid=None)`).
    case reflectFast(ReflectFast)
    /// Already in flight under this id: wait for ITS outcome, run nothing.
    case joining(Int64, Deadline?, OutcomeBox)
    /// Passed the gate and claimed its row.
    case dispatch(Dispatch)
}

/// A `reflect` on the `kernel` address: answered from the substrate identity,
/// with no dispatch and no task row. `execId` is nil for an UNNAMED item —
/// python does not even mint an id for one — and set when the caller NAMED the
/// execution, whose ledger row is already open.
struct ReflectFast {
    let target: Agent
    let payload: JSON
    let execId: Int64?
}

/// One item on its way to a handler.
struct Dispatch {
    let target: Agent
    let payload: JSON
    let verb: String
    let timeout: Deadline?
    /// The ledger key AND the task-table handle: one int from the one counter.
    /// A caller's name resolves to it; a retry re-registers under the same id.
    let execId: Int64
    let taskId: TaskId
    /// A RAW request body at its payload path: present ⇒ dispatch through the
    /// BINARY channel.
    let binary: Blob?
    /// The binary channel with no request body — the reply's chunk comes back
    /// raw (`Item.rawReply`).
    let rawReply: Bool
}

/// An item's own deadline: the seconds the kernel waits, and the value as the
/// caller gave it — python formats the RAW `timeout` (`f"{d.timeout}s"`), so an
/// int `5` reads `5s` and a float `5.0` reads `5.0s`.
struct Deadline: Sendable {
    let seconds: Double
    let text: String
}

/// One-shot "this item has started": the batch turnstile's signal. `fire` is
/// idempotent (a retry fans out again; only the first counts) and `wait`
/// returns at once when it already fired.
public final class StartSignal: @unchecked Sendable {
    private let lock = NSLock()
    private var fired = false
    private var waiter: CheckedContinuation<Void, Never>?

    public init() {}

    public func fire() {
        let w: CheckedContinuation<Void, Never>? = lock.withLock {
            guard !fired else { return nil }
            fired = true
            defer { waiter = nil }
            return waiter
        }
        w?.resume()
    }

    public func wait() async {
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            let now: Bool = lock.withLock {
                if fired { return true }
                waiter = c
                return false
            }
            if now { c.resume() }
        }
    }
}
