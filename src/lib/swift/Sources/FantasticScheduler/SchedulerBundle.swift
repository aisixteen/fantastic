// Recurring-task scheduler as an agent.
//
// Mirrors the canonical Python `scheduler` (and Rust `fantastic-scheduler`)
// verb-for-verb. State persists THROUGH the loader (`load_blob`/`persist_blob`)
// — nothing to wire; this bundle owns NO disk surface of its own and never
// touches FileManager. Sidecars are store-relative (`agents/<id>/…`, next to
// the agent's own agent.yaml — one store, no `.fantastic/.fantastic/…` nest):
//
//   - `schedules.yaml` — `[{id, target, payload, interval_seconds, next_run,
//                          paused, run_count, created_at, timeout?}, …]`
//   - `schedule_events.yaml`  — append-only, one event per fire, ring-trimmed to HISTORY_MAX.
//
// `boot` never needs a store; a WRITE failfasts (`scheduler.<verb>: …`) only
// when the loader has no store — reads are empty until then. A tick collects
// every DUE schedule, bumps `run_count`/`next_run`, persists FIRST (a crash
// inside the fire window cannot re-fire on reboot), then fires them all — ONE
// `kernel.send([...])` batch, the items concurrent, so one hung target never
// blocks the others. `timeout` is the schedule's own per-fire bound in seconds; absent =
// wait forever (the kernel never defaults it). One `{type:"schedule_fired", …}`
// per outcome goes to the scheduler's OWN inbox only — the target receives the
// payload, which is the tick.
//
// A SCHEDULE FIRES AS THE SCHEDULER ITSELF: every fire goes out under the
// scheduler's own id, so the gate judges the scheduler's `can_call` and the
// target's `callable_by` for the scheduler. `list`, `history`, `tick_now`,
// `unschedule` and `pause`/`resume` see every schedule.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "scheduler.tools"

/// Maximum history entries kept per scheduler (ring-trim threshold). Matches
/// Python's `HISTORY_MAX`.
public let HISTORY_MAX = 500

/// A one-permit wake-up for a fire loop: `poke` wakes the waiter (or, with none
/// waiting, makes the next `wait` return at once); `wait(upTo:)` returns on a poke
/// or when the time is up — `nil` waits for a poke only, never on a timer.
final class Wake: @unchecked Sendable {
    private let lock = NSLock()
    private var pending = false
    private var waiter: CheckedContinuation<Void, Never>?

    func poke() {
        let c: CheckedContinuation<Void, Never>? = lock.withLock {
            if let w = waiter {
                waiter = nil
                return w
            }
            pending = true
            return nil
        }
        c?.resume()
    }

    private func poked() async {
        await withTaskCancellationHandler {
            await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
                let now: Bool = lock.withLock {
                    if pending {
                        pending = false
                        return true
                    }
                    if Task.isCancelled { return true }
                    waiter = c
                    return false
                }
                if now { c.resume() }
            }
        } onCancel: {
            let c: CheckedContinuation<Void, Never>? = lock.withLock {
                defer { waiter = nil }
                return waiter
            }
            c?.resume()
        }
    }

    func wait(upTo seconds: Double?) async {
        guard let seconds else {
            await poked()
            return
        }
        await withTaskGroup(of: Void.self) { group in
            group.addTask { await self.poked() }
            group.addTask {
                try? await Task.sleep(nanoseconds: UInt64(max(0, seconds) * 1_000_000_000))
            }
            await group.next()
            group.cancelAll()
        }
    }
}

public final class SchedulerBundle: AgentBundle, @unchecked Sendable {
    public let name = "scheduler"

    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs) }
    public var emitsDoc: JSON? { .object(Self.emits) }

    /// python's module-level `VERBS` — ONE table: reflect renders it and
    /// `advertises` reads it.
    /// Served by `docs` (tier detailed). Python `EMITS`.
    static let emits: OrderedDictionary<String, JSON> = [
        "schedule_fired": .string(
            "{type:'schedule_fired', schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} — emitted on the scheduler's OWN inbox after every fire (never the target's — the payload send is the tick)"
        )
    ]

    static let verbDocs: OrderedDictionary<String, JSON> = [
        "reflect": .string("Identity + tick state. No args."),
        "boot": .string("Idempotent. Starts the tick loop; persistence is THROUGH the loader."),
        "schedule": .string("args: target:str (the agent each fire calls), payload:{type:<verb>, ...} (the call itself: any verb `target` answers, with its args; {type:'send', text:'...'} wakes an AI with that text), interval_seconds:int (default 60), fire_timeout:number? (seconds per fire; absent = wait forever — not `timeout`, which is a call's own routing field). Every fire runs AS THE SCHEDULER — it reaches what the scheduler's can_call grants. Returns {schedule_id, schedule}. Persisted THROUGH the loader; failfast if no store wired."),
        "unschedule": .string("args: schedule_id:str (req). Returns {removed:bool, schedule_id}."),
        "list": .string("No args. Returns {schedules: [<schedule record>, ...]} — every schedule."),
        "pause": .string("args: schedule_id:str?. With id pauses one schedule; without, pauses the whole scheduler."),
        "resume": .string("args: schedule_id:str?. With id resumes one schedule; without, resumes the whole scheduler."),
        "tick_now": .string("args: schedule_id:str (req). Bumps run_count, persists, then fires that schedule immediately, as the scheduler. Returns {fired:true, schedule_id, result, error} — the fire's outcome as `history` records it: the target's reply, or why the fire failed."),
        "history": .string("args: limit:int? (1..500, default 100), schedule_id:str?. Returns {history:[event,...], count} — every schedule's fires."),
    ]

    public init() {}

    private let lock = NSLock()
    /// Live tick tasks keyed by scheduler agent id. `boot` populates;
    /// `onShutdown` / `onDelete` cancel.
    /// Each booted scheduler's TICK LOOP, by its row in the kernel task table.
    /// Not a bare `Task`: the loop outlives the `boot` dispatch and fires a send
    /// per due schedule for the life of the agent, so it belongs in `ps` and
    /// within reach of `cancel` — and its fires are then parented to work that
    /// is still alive, never to a dispatch that has already ended.
    private var tasks: [String: TaskId] = [:]
    /// python's module-global `_cache`: the RAM copy of each scheduler's
    /// schedules, keyed by agent id.
    ///
    /// It is updated BEFORE the persist, and NOTHING evicts it — not `delete_agent`
    /// (python's `_cache` outlives the record), and not a failed write. A tick that
    /// bumped `next_run` and could not save must not re-fire the same tick on the
    /// next sweep; `schedule`/`tick_now` are the two that DO roll back, because a
    /// phantom schedule (or run) nobody can see on disk is worse than the refusal.
    /// Bad JSON on disk reads as an empty list, exactly as `json.JSONDecodeError`
    /// does there.
    private var cache: [String: [JSON]] = [:]
    /// Per scheduler: what wakes its fire loop (a change), and the state tap that
    /// pokes it when the scheduler's own record is updated. Python `_wake`/`_unsub`.
    private var wakes: [String: Wake] = [:]
    private var subs: [String: SubscriberToken] = [:]

    private func wakeOf(_ agentId: AgentId) -> Wake {
        lock.withLock {
            if let w = wakes[agentId.value] { return w }
            let w = Wake()
            wakes[agentId.value] = w
            return w
        }
    }

    /// Something the fire loop waits on changed: it recomputes when to wake.
    private func poke(_ agentId: AgentId) {
        let w = lock.withLock { wakes[agentId.value] }
        w?.poke()
    }

    /// python's `readme.md`, verbatim.
    public var readme: String? {
        """
        # scheduler — recurring tasks
        Verbs: schedule, unschedule, list, pause, resume, tick_now, history. Persistence (schedules.yaml, schedule_events.yaml) is automatic THROUGH the loader (`kernel_state`) — nothing to wire; a write failfasts only when no store is wired at the root.

        **A schedule fires AS THE SCHEDULER.** Every fire goes out under the scheduler's
        own id, so the gate judges the scheduler's `can_call` and the target's
        `callable_by` for the scheduler: wire the scheduler → target edge. `list`,
        `history`, `tick_now`, `unschedule` and `pause`/`resume {schedule_id}` see every
        schedule.

        **Cadence.** Each schedule has its own `interval_seconds`; the loop wakes at the earliest due
        fire and never on a fixed tick, so schedules at 5 s and at 10 min coexist in one scheduler. To
        change a cadence, `unschedule` and `schedule` again. A schedule can fire any verb — e.g. an
        `exec_bridge` `start` of a one-shot check job, for a source that does not push (a
        job's own output has `monitor` on its `exec_bridge`).

        """
    }

    // ── dispatch ────────────────────────────────────────────────

    public func handle(
        agentId: AgentId, payload: JSON, kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        switch verb {
        case "reflect": return reflectReply(agentId, kernel: kernel)
        case "boot": return await bootReply(agentId, kernel: kernel)
        case "schedule": return await scheduleReply(agentId, payload: payload, kernel: kernel)
        case "unschedule": return await unscheduleReply(agentId, payload: payload, kernel: kernel)
        case "list": return await listReply(agentId, kernel: kernel)
        case "pause": return await pauseReply(agentId, payload: payload, kernel: kernel)
        case "resume": return await resumeReply(agentId, payload: payload, kernel: kernel)
        case "tick_now": return await tickNowReply(agentId, payload: payload, kernel: kernel)
        case "history": return await historyReply(agentId, payload: payload, kernel: kernel)
        default:
            // python `{t!r}`: an absent `type` reads `None`, a number reads bare.
            // `shutdown` is NOT a verb here — python's VERBS table has no such key,
            // so it lands in exactly this branch.
            return Refusal.make(
                Refusal.unhandled, "scheduler: unknown type \(pyRepr(payload["type"]))",
                extra: [("verb", payload["type"])])
        }
    }

    // ── argument typing ─────────────────────────────────────────

    /// ABSENT ⇒ `def`. Present, an integer, ≥ `minimum` ⇒ that value. ANYTHING
    /// ELSE (a bool, a string, a float, an explicit null, a value under the
    /// bound) ⇒ a `bad_arg` refusal.
    ///
    /// A present-but-wrong number is a TYPO, and `asInt ?? 60` hid it:
    /// `interval_seconds: "60"` silently became an hourly schedule the caller
    /// believed was per-minute, and nothing said so.
    /// `refusal == nil` ⇒ `value` is usable.
    private func intArg(
        _ payload: JSON, _ key: String, _ def: Int64, _ verb: String, _ minimum: Int64
    ) -> (value: Int64, refusal: JSON?) {
        guard let raw = payload.asObject?[key] else { return (def, nil) }
        if case .integer(let n) = raw, n >= minimum { return (n, nil) }
        let want = minimum == 0 ? "a non-negative int" : "an int >= \(minimum)"
        return (0, Refusal.bad(verb, key, want))
    }

    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
        _ = await stopTick(agentId, kernel: kernel)
    }

    /// The tick loop AND the schedule cache die with the agent: a scheduler
    /// recreated under the same id never lists or fires its predecessor's
    /// schedules (python `on_delete`).
    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        lock.withLock { cache[agentId.value] = nil }
        _ = await stopTick(agentId, kernel: kernel)
        let token: SubscriberToken? = lock.withLock {
            wakes[agentId.value] = nil
            return subs.removeValue(forKey: agentId.value)
        }
        if let token { kernel.unsubscribe(token) }
    }

    // ── persistence (THROUGH the loader) ────────────────────────

    // THE LOADER OWNS THE PATH. This bundle names only a FILENAME; the sidecar
    // lands beside the agent's own agent.yaml in whatever store was discovered.
    private static let schedulesBlob = "schedules.yaml"
    private static let historyBlob = "schedule_events.yaml"

    /// Python's shape for a failed write inside a verb:
    /// `{error: "scheduler.<verb>: <loader error>", reason?}`.
    private func persistErr(_ verb: String, _ e: JSON) -> JSON {
        var out: JSON = .object([
            "error": .string("scheduler.\(verb): \(e["error"].asString ?? "")")
        ])
        if let reason = e["reason"].asString { out["reason"] = .string(reason) }
        return out
    }

    /// Read one of THIS agent's own sidecars through the kernel. The loader owns
    /// the path; this bundle names only a filename. No store / missing ⇒ nil.
    private func fileRead(_ agentId: AgentId, _ kernel: Kernel, _ name: String) async -> String? {
        // The kernel's own persistence function — the namespace is the agent
        // running this (the scheduler, in its handler or its tick task).
        let r = await kernel.loadOwnBlob(name: name)
        return r["content"].asString
    }

    /// Persist one of THIS agent's own sidecars through the kernel. Returns the
    /// loader's `{error, reason?}` on a denied/no-store write, nil on success.
    private func fileWrite(
        _ agentId: AgentId, _ kernel: Kernel, _ name: String, _ content: String
    ) async -> JSON? {
        let r = await kernel.persistOwnBlob(name: name, content: content)
        guard let err = r["error"].asString else { return nil }
        var e: JSON = .object(["error": .string(err)])
        if let reason = r["reason"].asString { e["reason"] = .string(reason) }
        return e
    }

    private func cached(_ id: AgentId) -> [JSON]? { lock.withLock { cache[id.value] } }

    /// The RAM copy IS the state (python mutates `_cache` in place). Setting it is
    /// a separate step from persisting, so a failed write can still be kept.
    private func setCache(_ id: AgentId, _ schedules: [JSON]) {
        lock.withLock { cache[id.value] = schedules }
    }

    private func loadSchedules(_ id: AgentId, _ kernel: Kernel) async -> [JSON] {
        if let hit = cached(id) { return hit }
        let raw = await fileRead(id, kernel, Self.schedulesBlob)
        // Unreadable YAML (or a document that is not a list) is an EMPTY list,
        // not a crash — python's `_cache[sid] = doc if isinstance(doc, list) else []`.
        let parsed = raw.flatMap { $0.isEmpty ? nil : (try? YamlIO.load($0))?.asArray } ?? []
        // FIRST FILL WINS: a verb that loaded and wrote while this read was in
        // flight already holds the cache — the read must not replace it with the
        // older list.
        return lock.withLock {
            if let held = cache[id.value] { return held }
            cache[id.value] = parsed
            return parsed
        }
    }

    /// RAM FIRST, then disk — and the caller decides what a write failure means.
    @discardableResult
    private func saveSchedules(_ id: AgentId, _ kernel: Kernel, _ schedules: [JSON]) async
        -> JSON?
    {
        setCache(id, schedules)
        return await fileWrite(
            id, kernel, Self.schedulesBlob,
            YamlIO.dump(.array(schedules)))
    }

    /// Returns the persist error, if any — python's `_append_history` hands it
    /// back so `_fire_many` can name it on stderr.
    @discardableResult
    private func appendHistory(_ id: AgentId, _ kernel: Kernel, _ event: JSON) async -> JSON? {
        let prev = await fileRead(id, kernel, Self.historyBlob) ?? ""
        // One `---` document per event (python `messages_yaml.row_to_doc`).
        var combined = prev + YamlIO.rowToDoc(event.asObject ?? [:])
        // Ring-trim if oversize: keep the newest HISTORY_MAX documents.
        let events = Self.parseEvents(combined)
        if events.count > 2 * HISTORY_MAX {
            combined = events.suffix(HISTORY_MAX).map(YamlIO.rowToDoc).joined()
        }
        return await fileWrite(id, kernel, Self.historyBlob, combined)
    }

    private func readHistory(_ id: AgentId, _ kernel: Kernel, _ limit: Int) async -> [JSON] {
        guard let raw = await fileRead(id, kernel, Self.historyBlob), !raw.isEmpty else { return [] }
        return Self.parseEvents(raw).suffix(limit).map { .object($0) }
    }

    /// The event stream's documents that are mappings. A malformed stream reads
    /// empty (reads are lenient, as they were for the JSON-lines file). Python
    /// `_parse_events`.
    private static func parseEvents(_ raw: String) -> [OrderedDictionary<String, JSON>] {
        guard let docs = try? YamlIO.loadAll(raw) else { return [] }
        return docs.compactMap { $0.asObject }
    }

    // ── tick loop ───────────────────────────────────────────────

    /// Fire what is due, then sleep until the earliest next fire — or, with
    /// nothing to wait for, until a change (`poke`). NO IDLE WAKEUPS:
    /// never on a fixed tick. Python `_tick_loop`.
    private func tickLoop(_ agentId: AgentId, _ kernel: Kernel) async {
        let wake = wakeOf(agentId)
        while !Task.isCancelled {
            guard kernel.agent(agentId) != nil else { return }  // deleted → exit
            var delay: Double?
            // python truthiness on the record's `paused`: `0`, `""` and `[]` are not
            // paused, and any non-empty value is.
            if !pyTruthy(kernel.agent(agentId)?.metaValue(forKey: "paused") ?? .null) {
                let now = nowSecs()
                var schedules = await loadSchedules(agentId, kernel)
                var due: [JSON] = []
                for i in schedules.indices {
                    if pyTruthy(schedules[i]["paused"]) { continue }
                    let nextRun = schedules[i]["next_run"].asDouble ?? 0
                    if now < nextRun {
                        delay = min(delay ?? .infinity, nextRun - now)
                        continue
                    }
                    // Persist BEFORE the fire: run_count/next_run are bumped and
                    // saved first, so a crash inside the fire window does not
                    // re-fire the same tick on reboot (at-most-once restart
                    // semantics).
                    let count = schedules[i]["run_count"].asInt ?? 0
                    let interval = schedules[i]["interval_seconds"].asDouble ?? 60
                    schedules[i]["run_count"] = .integer(count + 1)
                    schedules[i]["next_run"] = .double(now + interval)
                    due.append(schedules[i])
                }
                if !due.isEmpty {
                    // THE BUMP IS KEPT even when the write fails: RAM is updated
                    // first, so a persist fault cannot make the next look re-fire
                    // the same tick. The fault is NAMED — python
                    // `scheduler/tools.py`.
                    if let err = await saveSchedules(agentId, kernel, schedules) {
                        RuntimeLog.warn(
                            "scheduler \(agentId.value): persist failed: \(pyStr(err["error"]))")
                    }
                    await fireMany(agentId, due, kernel)
                    continue
                }
            }
            if Task.isCancelled { return }
            await wake.wait(upTo: delay)
        }
    }

    private func fireItem(_ sch: JSON) -> Item {
        let payload = sch.asObject?["payload"] ?? .object([:])
        return Item(
            AgentId(sch["target"].asString ?? ""), payload, timeout: sch["fire_timeout"].asDouble)
    }

    /// Fire every due schedule AS THE SCHEDULER — ONE batch send, the items
    /// concurrent, no head-of-line blocking — then record one `schedule_fired`
    /// per outcome, and return those events in `due` order.
    @discardableResult
    private func fireMany(_ agentId: AgentId, _ due: [JSON], _ kernel: Kernel) async -> [JSON] {
        let ts = nowSecs()
        let items = due.map { self.fireItem($0) }
        // The scheduler's own handler or tick task — its sends are its own.
        let outs = await kernel.send(items)
        let outcomes: [Outcome?] = due.indices.map { $0 < outs.count ? outs[$0] : nil }
        let durationMs = Int64((nowSecs() - ts) * 1000)
        var events: [JSON] = []
        for (i, sch) in due.enumerated() {
            let target = sch["target"].asString ?? ""
            let payload = sch.asObject?["payload"] ?? .object([:])
            var error: JSON = .null
            var result: JSON = .null
            if let out = outcomes[i] {
                if out.status == .completed {
                    // COMPLETED means completed. A reply that happens to carry an
                    // `error` key is still the target's ANSWER — python puts it in
                    // `result` and leaves `error` null; hoisting it reported a
                    // failed fire for a verb that had answered.
                    result = out.result
                } else {
                    // python: `res.get("error") if isinstance(res, dict) else res`,
                    // rendered with `str()` — so a cancelled fire with no detail
                    // reads `cancelled: None`.
                    let detail: JSON =
                        out.result.asObject != nil ? out.result["error"] : out.result
                    error = .string("\(out.status.rawValue): \(pyStr(detail))")
                }
            }
            let event: JSON = .object([
                "type": .string("schedule_fired"),
                "schedule_id": .string(sch["id"].asString ?? ""),
                "scheduler_id": .string(agentId.value),
                "target": .string(target),
                "payload": payload,
                "result": result,
                "error": error,
                "ts": .double(ts),
                "duration_ms": .integer(durationMs),
            ])
            if let err = await appendHistory(agentId, kernel, event) {
                RuntimeLog.warn(
                    "scheduler \(agentId.value): history persist failed: \(pyStr(err["error"]))")
            }
            // ONE delivery per tick (py-canonical): no target-echo of
            // `schedule_fired`, which would double every delivery to a live program's
            // on_message — the payload send IS the tick; telemetry rides the
            // scheduler's own inbox and schedule_events.yaml.
            await kernel.emit(agentId, event)
            events.append(event)
        }
        return events
    }

    private func nowSecs() -> Double { Date().timeIntervalSince1970 }

    private func mintId() -> String {
        String(format: "sch_%06x", Int(UInt32.random(in: 0...0xFF_FFFF)))
    }

    // ── verbs ───────────────────────────────────────────────────

    private func reflectReply(_ agentId: AgentId, kernel: Kernel) -> JSON {
        let running: Bool = {
            lock.lock()
            defer { lock.unlock() }
            // LIVENESS COMES FROM THE TABLE, which holds only running work.
            guard let tid = tasks[agentId.value] else { return false }
            return kernel.tasks.contains(tid)
        }()
        let agent = kernel.agent(agentId)
        return .object([
            "id": .string(agentId.value),
            "sentence": .string("Recurring-task scheduler."),
            // python `bool(rec.get("paused"))` — truthiness in, a BOOL out.
            "paused": .bool(pyTruthy(agent?.metaValue(forKey: "paused") ?? .null)),
            "running": .bool(running),
        ])
    }

    /// Start + register the tick task unless a LIVE one is already registered.
    /// Synchronous so the NSLock never spans an `await`.
    ///
    /// python's condition is `id not in _tick_tasks or _tick_tasks[id].done()` — a
    /// loop whose task has ENDED (cancelled, or the agent was deleted and
    /// recreated) is restarted. Holding the stale id and doing nothing left a
    /// scheduler that reported `{running:true}` and never ticked again.
    private func startTickIfNotLive(_ agentId: AgentId, _ kernel: Kernel) {
        let wake = wakeOf(agentId)
        lock.lock()
        defer { lock.unlock() }
        if subs[agentId.value] == nil {
            // The scheduler's own record changed (`paused`, by a verb or an
            // update_agent): the loop recomputes when to wake.
            let me = agentId.value
            subs[me] = kernel.subscribe { evt, _ in
                if evt["agent_id"].asString == me && evt["kind"].asString == "updated" {
                    wake.poke()
                }
            }
        }
        if let tid = tasks[agentId.value], kernel.tasks.contains(tid) {
            wake.poke()  // a boot of a running loop makes it look again
            return
        }
        tasks[agentId.value] = kernel.spawnTask(agent: agentId, verb: "tick_loop") {
            [weak self] in
            guard let self else { return }
            await self.tickLoop(agentId, kernel)
        }
    }

    private func bootReply(_ agentId: AgentId, kernel: Kernel) async -> JSON {
        // Boot never needs a store (python): the tick loop starts; a WRITE
        // surfaces the loader's error when none is wired.
        startTickIfNotLive(agentId, kernel)
        // ALWAYS the same reply. `already_booted` was a key python never emits, so
        // a caller branching on the reply saw two shapes for one idempotent verb.
        return .object(["running": .bool(true)])
    }

    /// Cancel the tick loop through the TABLE. Idempotent.
    private func stopTick(_ agentId: AgentId, kernel: Kernel) async -> JSON {
        let removed: TaskId? = {
            lock.lock()
            defer { lock.unlock() }
            return tasks.removeValue(forKey: agentId.value)
        }()
        if let removed {
            // Through the TABLE — the same path an operator's `cancel` takes.
            _ = await kernel.cancelTask(removed)
            return .object(["stopped": .bool(true), "id": .string(agentId.value)])
        }
        return .object([
            "stopped": .bool(false), "id": .string(agentId.value),
            "reason": .string("not running"),
        ])
    }

    private func scheduleReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        // PYTHON'S ORDER, and it is observable: payload shape → interval_seconds
        // → target → payload.type → timeout. A call with two faults must name the
        // same one python names.
        var schedPayload = payload.asObject?["payload"] ?? .object([:])
        if schedPayload.isNull { schedPayload = .object([:]) }
        guard schedPayload.asObject != nil else {
            // A list/string here can never carry a `type`; say so as an ARG
            // error rather than as the downstream "payload.type required".
            return Refusal.bad("schedule", "payload", "an object with type")
        }
        let (interval, intervalBad) = intArg(payload, "interval_seconds", 60, "schedule", 1)
        if let intervalBad { return intervalBad }
        // `payload.get("target", "")` under truthiness — and a TRUTHY non-string is
        // stored verbatim, the way python stores whatever it was handed.
        let target = payload.asObject?["target"] ?? .string("")
        if !pyTruthy(target) {
            return .object(["error": .string("schedule: target required")])
        }
        if !pyTruthy(schedPayload["type"]) {
            return .object(["error": .string("schedule: payload.type required")])
        }
        // Per-fire bound, seconds. Optional: absent = wait forever (never defaulted).
        // STORED AS SENT — an int `5` stays `5`; widening it to `5.0` changed the
        // persisted bytes and every reply that echoes the schedule.
        var timeout: JSON = .null
        if let raw = payload.asObject?["fire_timeout"], !raw.isNull {
            let positive: Bool
            switch raw {
            case .integer(let n): positive = n > 0
            case .double(let d): positive = d > 0
            default: positive = false
            }
            guard positive else {
                return Refusal.bad("schedule", "fire_timeout", "a positive number of seconds")
            }
            timeout = raw
        }
        let now = nowSecs()
        var sch: JSON = .object([
            "id": .string(mintId()),
            "target": target,
            "payload": schedPayload,
            "interval_seconds": .integer(interval),
            "created_at": .double(now),
            "next_run": .double(now + Double(interval)),
            "run_count": .integer(0),
            "paused": .bool(false),
        ])
        if !timeout.isNull { sch["fire_timeout"] = timeout }
        var schedules = await loadSchedules(agentId, kernel)
        schedules.append(sch)
        if let err = await saveSchedules(agentId, kernel, schedules) {
            // ROLL THE APPEND BACK: a failed persist leaves no phantom schedule in
            // RAM either (python re-filters `_cache` by id).
            setCache(agentId, schedules.filter { $0["id"] != sch["id"] })
            return persistErr("schedule", err)
        }
        poke(agentId)
        return .object(["schedule_id": sch["id"], "schedule": sch])
    }

    private func unscheduleReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        // TRUTHINESS, and the id is echoed RAW — python compares and returns
        // whatever it was given.
        let sid = payload["schedule_id"]
        guard pyTruthy(sid) else {
            return .object(["error": .string("unschedule: schedule_id required")])
        }
        var schedules = await loadSchedules(agentId, kernel)
        let before = schedules.count
        schedules.removeAll { $0["id"] == sid }
        let removed = schedules.count < before
        // RAM FIRST, unconditionally (python assigns `_cache[id]` before it looks at
        // `removed`), so a failed write still forgets the schedule.
        setCache(agentId, schedules)
        if removed {
            if let err = await saveSchedules(agentId, kernel, schedules) {
                return persistErr("unschedule", err)
            }
            poke(agentId)
        }
        return .object(["removed": .bool(removed), "schedule_id": sid])
    }

    private func listReply(_ agentId: AgentId, kernel: Kernel) async -> JSON {
        .object([
            "schedules": .array(await loadSchedules(agentId, kernel)),
            // HOW TO GET THIS AGAIN — python `_list`'s `origin`.
            "origin": .string("\(agentId.value) list — re-send; changes as schedules change"),
        ])
    }

    private func pauseReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        await flipPaused(agentId, payload: payload, kernel: kernel, paused: true)
    }

    private func resumeReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        await flipPaused(agentId, payload: payload, kernel: kernel, paused: false)
    }

    /// pause/resume: with a schedule_id flips one schedule; without, flips the
    /// whole scheduler's `paused` meta (persisted through the provider).
    private func flipPaused(_ agentId: AgentId, payload: JSON, kernel: Kernel, paused: Bool) async
        -> JSON
    {
        let key = paused ? "paused" : "resumed"
        let sid = payload["schedule_id"]
        if pyTruthy(sid) {
            var schedules = await loadSchedules(agentId, kernel)
            var touched = 0
            for i in schedules.indices where schedules[i]["id"] == sid {
                schedules[i]["paused"] = .bool(paused)
                touched += 1
            }
            // RAM FIRST: a failed write leaves the flag flipped in memory, so the
            // very next tick already honours it.
            if touched > 0, let err = await saveSchedules(agentId, kernel, schedules) {
                return persistErr(paused ? "pause" : "resume", err)
            }
            if touched > 0 { poke(agentId) }
            // FLAG FIRST — python's dict literal is `{"paused": …, "schedule_id": …}`.
            var out: JSON = .object([:])
            out[key] = .bool(touched > 0)
            out["schedule_id"] = sid
            return out
        }
        if kernel.agent(agentId) != nil {
            // Through the kernel update path (publishes `updated`) — never a
            // direct persist, which the loader's writer never learns about.
            kernel.update(agentId, ["paused": .bool(paused)])
        }
        var out: JSON = .object([:])
        out[key] = .bool(true)
        out["scheduler_id"] = .string(agentId.value)
        return out
    }

    private func tickNowReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        let sid = payload["schedule_id"]
        guard pyTruthy(sid) else {
            return .object(["error": .string("tick_now: schedule_id required")])
        }
        var schedules = await loadSchedules(agentId, kernel)
        guard let i = schedules.firstIndex(where: { $0["id"] == sid }) else {
            return .object(["error": .string("schedule \(pyRepr(sid)) not found")])
        }
        // Same order as the tick: bump → persist → fire.
        let count = schedules[i]["run_count"].asInt ?? 0
        schedules[i]["run_count"] = .integer(count + 1)
        if let err = await saveSchedules(agentId, kernel, schedules) {
            // ROLLED BACK in RAM too (python's `s["run_count"] -= 1`): a run nothing
            // recorded must not show up in the next `list`.
            schedules[i]["run_count"] = .integer(count)
            setCache(agentId, schedules)
            return persistErr("tick_now", err)
        }
        // THE OUTCOME, not just the fact. `{fired:true}` alone would hide a target that
        // refuses every fire (a payload naming a verb the target lacks).
        let ev = await fireMany(agentId, [schedules[i]], kernel).first ?? .null
        return .object([
            "fired": .bool(true), "schedule_id": sid, "result": ev["result"], "error": ev["error"],
        ])
    }

    private func historyReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        let (rawLimit, limitBad) = intArg(payload, "limit", 100, "history", 1)
        if let limitBad { return limitBad }
        let limit = min(500, Int(rawLimit))
        // The newest `limit` fires.
        var entries = Array(await readHistory(agentId, kernel, 2 * HISTORY_MAX).suffix(limit))
        let sid = payload["schedule_id"]
        if pyTruthy(sid) {
            entries = entries.filter { $0["schedule_id"] == sid }
        }
        return .object([
            "history": .array(entries), "count": .integer(Int64(entries.count)),
            "origin": .string("\(agentId.value) history — re-send; grows as schedules fire"),
        ])
    }
}
