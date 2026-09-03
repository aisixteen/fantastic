// Recurring-task scheduler as an agent.
//
// Mirrors the canonical Python `scheduler` (and Rust `fantastic-scheduler`)
// verb-for-verb. State persists THROUGH the loader (`load_blob`/`persist_blob`)
// — nothing to wire; this bundle owns NO disk surface of its own and never
// touches FileManager. Sidecars are store-relative (`agents/<id>/…`, next to
// the agent's own agent.json — one store, no `.fantastic/.fantastic/…` nest):
//
//   - `schedules.json` — `[{id, target, payload, interval_seconds, next_run,
//                          paused, run_count, created_at, timeout?}, …]`
//   - `history.jsonl`  — append-only, one event per fire, ring-trimmed to HISTORY_MAX.
//
// `boot` never needs a store; a WRITE failfasts (`scheduler.<verb>: …`) only
// when the loader has no store — reads are empty until then. A tick collects
// every DUE schedule, bumps `run_count`/`next_run`, persists FIRST (a crash
// inside the fire window cannot re-fire on reboot), then fires them all as ONE
// `kernel.send([...])` batch — concurrent, so one hung target never blocks the
// others. `timeout` is the schedule's own per-fire bound in seconds; absent =
// wait forever (the kernel never defaults it). One `{type:"schedule_fired", …}`
// per outcome goes to the scheduler's OWN inbox only — the target receives the
// payload, which is the tick.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "scheduler.tools"

/// Maximum history entries kept per scheduler (ring-trim threshold). Matches
/// Python's `HISTORY_MAX`.
public let HISTORY_MAX = 500

public final class SchedulerBundle: AgentBundle, @unchecked Sendable {
    public let name = "scheduler"

    public init() {}

    private let lock = NSLock()
    /// Live tick tasks keyed by scheduler agent id. `boot` populates; `shutdown`
    /// / `onDelete` cancel.
    private var tasks: [String: Task<Void, Never>] = [:]

    public var readme: String? {
        """
        scheduler — recurring tasks as an agent.

        State persists THROUGH the loader (nothing to wire; a write failfasts only
        when no store is wired): `agents/<id>/schedules.json` + `history.jsonl`.

        Verbs: reflect · boot · shutdown · schedule {target, payload, interval_seconds, timeout?} ·
        unschedule {schedule_id} · list · pause/resume {schedule_id?} · tick_now {schedule_id} ·
        history {limit?, schedule_id?}. A tick bumps run_count/next_run of every DUE schedule,
        persists FIRST, then fires them all as ONE batch `kernel.send([{target, payload, timeout?}, …])`
        (concurrent; `timeout` = seconds per fire, absent = wait forever) and emits one
        `schedule_fired` per outcome to the scheduler's OWN inbox (the target gets the payload).
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
        case "shutdown": return shutdownReply(agentId)
        case "schedule": return await scheduleReply(agentId, payload: payload, kernel: kernel)
        case "unschedule": return await unscheduleReply(agentId, payload: payload, kernel: kernel)
        case "list": return await listReply(agentId, kernel: kernel)
        case "pause": return await pauseReply(agentId, payload: payload, kernel: kernel)
        case "resume": return await resumeReply(agentId, payload: payload, kernel: kernel)
        case "tick_now": return await tickNowReply(agentId, payload: payload, kernel: kernel)
        case "history": return await historyReply(agentId, payload: payload, kernel: kernel)
        default:
            return .object(["error": .string("scheduler: unknown type '\(verb)'")])
        }
    }

    /// Refuse a `tick_sec` that is not a positive number at WRITE time — the
    /// point of the mistake. Silently defaulting it (as the tick loop's
    /// `?? 1.0` did) turns an operator's typo into a behaviour change nobody is
    /// told about.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        guard let raw = meta["tick_sec"] ?? record["tick_sec"] else { return nil }
        guard let v = raw.asDouble, v > 0 else {
            return "scheduler: tick_sec must be a positive number of seconds"
        }
        return nil
    }

    // ── argument typing ─────────────────────────────────────────

    /// ABSENT ⇒ `def`. Present, an integer, ≥ `minimum` ⇒ that value. ANYTHING
    /// ELSE (a bool, a string, a float, an explicit null, a value under the
    /// bound) ⇒ a `bad_arg` refusal.
    ///
    /// A present-but-wrong number is a TYPO, and `asInt ?? 60` hid it:
    /// `interval_seconds: "60"` silently became an hourly schedule the caller
    /// believed was per-minute, and nothing said so.
    /// `refusal == nil` ⇒ `value` is usable. (A tuple, not `Result<_, JSON>`:
    /// the retroactive `JSON: Error` conformance belongs to exactly one module,
    /// and it is `FantasticTextFile`.)
    private func intArg(
        _ payload: JSON, _ key: String, _ def: Int64, _ verb: String, _ minimum: Int64
    ) -> (value: Int64, refusal: JSON?) {
        guard let raw = payload.asObject?[key] else { return (def, nil) }
        if case .integer(let n) = raw, n >= minimum { return (n, nil) }
        let want = minimum == 0 ? "a non-negative int" : "an int >= \(minimum)"
        return (0, Refusal.bad(verb, key, want))
    }

    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
        _ = shutdownReply(agentId)
    }

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        _ = shutdownReply(agentId)
    }

    // ── persistence (THROUGH the loader) ────────────────────────

    // THE LOADER OWNS THE PATH. This bundle names only a FILENAME; the sidecar
    // lands beside the agent's own agent.json in whatever store was discovered.
    private static let schedulesBlob = "schedules.json"
    private static let historyBlob = "history.jsonl"

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
        let r = await KernelTaskLocals.$currentSender.withValue(nil) {
            await kernel.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("load_blob"),
                        "agent_id": .string(agentId.value),
                        "name": .string(name),
                    ]))
            ])[0].result
        }
        return r["content"].asString
    }

    /// Persist one of THIS agent's own sidecars through the kernel. Returns the
    /// loader's `{error, reason?}` on a denied/no-store write, nil on success.
    private func fileWrite(
        _ agentId: AgentId, _ kernel: Kernel, _ name: String, _ content: String
    ) async -> JSON? {
        let r = await KernelTaskLocals.$currentSender.withValue(nil) {
            await kernel.send([
                Item(
                    AgentId("kernel"),
                    .object([
                        "type": .string("persist_blob"),
                        "agent_id": .string(agentId.value),
                        "name": .string(name),
                        "content": .string(content),
                    ]))
            ])[0].result
        }
        guard let err = r["error"].asString else { return nil }
        var e: JSON = .object(["error": .string(err)])
        if let reason = r["reason"].asString { e["reason"] = .string(reason) }
        return e
    }

    private func loadSchedules(_ id: AgentId, _ kernel: Kernel) async -> [JSON] {
        guard let raw = await fileRead(id, kernel, Self.schedulesBlob),
            let parsed = try? JSON.parse(raw), let arr = parsed.asArray
        else { return [] }
        return arr
    }

    @discardableResult
    private func saveSchedules(_ id: AgentId, _ kernel: Kernel, _ schedules: [JSON]) async
        -> JSON?
    {
        await fileWrite(
            id, kernel, Self.schedulesBlob, JSON.array(schedules).serializePretty(indent: 2))
    }

    private func appendHistory(_ id: AgentId, _ kernel: Kernel, _ event: JSON) async {
        let prev = await fileRead(id, kernel, Self.historyBlob) ?? ""
        var combined = prev + event.serialize() + "\n"
        // Ring-trim past 2× MAX.
        let lines = combined.split(separator: "\n", omittingEmptySubsequences: false)
            .map(String.init)
        // `split` on a trailing "\n" yields a final "" element — drop it for counting.
        let real = lines.last == "" ? Array(lines.dropLast()) : lines
        if real.count > 2 * HISTORY_MAX {
            combined = real.suffix(HISTORY_MAX).joined(separator: "\n") + "\n"
        }
        _ = await fileWrite(id, kernel, Self.historyBlob, combined)
    }

    private func readHistory(_ id: AgentId, _ kernel: Kernel, _ limit: Int) async -> [JSON] {
        guard let raw = await fileRead(id, kernel, Self.historyBlob) else { return [] }
        let lines = raw.split(separator: "\n").map(String.init).suffix(limit)
        return lines.compactMap { line -> JSON? in
            let t = line.trimmingCharacters(in: .whitespaces)
            return t.isEmpty ? nil : try? JSON.parse(t)
        }
    }

    // ── tick loop ───────────────────────────────────────────────

    private func tickLoop(_ agentId: AgentId, _ kernel: Kernel) async {
        while !Task.isCancelled {
            let tickSec = max(
                0.1, kernel.agent(agentId)?.metaValue(forKey: "tick_sec")?.asDouble ?? 1.0)
            try? await Task.sleep(nanoseconds: UInt64(tickSec * 1_000_000_000))
            if Task.isCancelled { return }
            guard let agent = kernel.agent(agentId) else { return }  // deleted → exit
            if agent.metaValue(forKey: "paused")?.asBool == true { continue }
            let now = nowSecs()
            var schedules = await loadSchedules(agentId, kernel)
            var due: [JSON] = []
            for i in schedules.indices {
                if schedules[i]["paused"].asBool == true { continue }
                let nextRun = schedules[i]["next_run"].asDouble ?? 0
                if now < nextRun { continue }
                // Persist BEFORE the fire: run_count/next_run are bumped and saved
                // first, so a crash inside the fire window does not re-fire the
                // same tick on reboot (at-most-once restart semantics).
                let count = schedules[i]["run_count"].asInt ?? 0
                let interval = schedules[i]["interval_seconds"].asDouble ?? 60
                schedules[i]["run_count"] = .integer(count + 1)
                schedules[i]["next_run"] = .double(now + interval)
                due.append(schedules[i])
            }
            if due.isEmpty { continue }
            await saveSchedules(agentId, kernel, schedules)
            await fireMany(agentId, due, kernel)
        }
    }

    private func fireItem(_ sch: JSON) -> Item {
        let payload = sch.asObject?["payload"] ?? .object([:])
        return Item(
            AgentId(sch["target"].asString ?? ""), payload, timeout: sch["timeout"].asDouble)
    }

    /// Fire every due schedule as ONE batch send — concurrent, no head-of-line
    /// blocking — then record one `schedule_fired` per outcome.
    private func fireMany(_ agentId: AgentId, _ due: [JSON], _ kernel: Kernel) async {
        let ts = nowSecs()
        let outcomes = await kernel.send(due.map(fireItem))
        let durationMs = Int64((nowSecs() - ts) * 1000)
        for (sch, out) in zip(due, outcomes) {
            let target = sch["target"].asString ?? ""
            let payload = sch.asObject?["payload"] ?? .object([:])
            var error: JSON = .null
            var result: JSON = .null
            if out.status != .completed {
                let detail = out.result["error"].asString ?? out.result.serialize()
                error = .string("\(out.status.rawValue): \(detail)")
            } else if let err = out.result["error"].asString {
                error = .string(err)
            } else {
                result = out.result
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
            await appendHistory(agentId, kernel, event)
            // ONE delivery per tick (py-canonical, 2026-08-07). The target-echo of
            // `schedule_fired` doubled every delivery to a live program's
            // on_message — the payload send IS the tick; telemetry rides the
            // scheduler's own inbox and history.jsonl.
            await kernel.emit(agentId, event)
        }
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
            return tasks[agentId.value] != nil
        }()
        let agent = kernel.agent(agentId)
        return .object([
            "id": .string(agentId.value),
            "sentence": .string("Recurring-task scheduler."),
            "tick_sec": .double(agent?.metaValue(forKey: "tick_sec")?.asDouble ?? 1.0),
            "paused": .bool(agent?.metaValue(forKey: "paused")?.asBool ?? false),
            "running": .bool(running),
            "verbs": .object([
                "reflect": .string("Identity + tick state. No args."),
                "boot": .string(
                    "Idempotent. Starts the tick loop. Persistence is THROUGH the loader; a write failfasts only when no store is wired."
                ),
                "shutdown": .string("Idempotent. Cancels the tick loop."),
                "schedule": .string(
                    "args: target:str, payload:dict, interval_seconds:int (default 60), timeout:number? (seconds per fire; absent = wait forever)."
                ),
                "unschedule": .string("args: schedule_id:str."),
                "list": .string("No args. Returns {schedules:[...]}."),
                "pause": .string("args: schedule_id:str?. Pauses one or all."),
                "resume": .string("args: schedule_id:str?. Resumes one or all."),
                "tick_now": .string(
                    "args: schedule_id:str. Bumps run_count, persists, then fires immediately."),
                "history": .string("args: limit:int?, schedule_id:str?."),
            ]),
            "emits": .object([
                "schedule_fired": .string(
                    "{type, schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} emitted on the scheduler's OWN inbox on every fire (never the target's)"
                )
            ]),
        ])
    }

    /// Atomically start + register the tick task if none is live for `id`.
    /// Returns false if one was already running (idempotent boot). Synchronous
    /// so the NSLock never spans an `await`.
    private func startTaskIfAbsent(_ id: String, _ make: () -> Task<Void, Never>) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if tasks[id] != nil { return false }
        tasks[id] = make()
        return true
    }

    private func bootReply(_ agentId: AgentId, kernel: Kernel) async -> JSON {
        // Boot never needs a store (python): the tick loop starts; a WRITE
        // surfaces the loader's error when none is wired.
        let started = startTaskIfAbsent(agentId.value) {
            Task { [weak self] in
                guard let self else { return }
                await self.tickLoop(agentId, kernel)
            }
        }
        if !started {
            return .object(["running": .bool(true), "already_booted": .bool(true)])
        }
        return .object(["running": .bool(true)])
    }

    private func shutdownReply(_ agentId: AgentId) -> JSON {
        lock.lock()
        let removed = tasks.removeValue(forKey: agentId.value)
        lock.unlock()
        if let removed {
            removed.cancel()
            return .object(["stopped": .bool(true), "id": .string(agentId.value)])
        }
        return .object([
            "stopped": .bool(false), "id": .string(agentId.value),
            "reason": .string("not running"),
        ])
    }

    private func scheduleReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        let target = payload["target"].asString ?? ""
        if target.isEmpty {
            return .object(["error": .string("schedule: target required")])
        }
        var schedPayload = payload.asObject?["payload"] ?? .object([:])
        if schedPayload.isNull { schedPayload = .object([:]) }
        guard schedPayload.asObject != nil else {
            // A list/string here can never carry a `type`; say so as an ARG
            // error rather than as the downstream "payload.type required".
            return Refusal.bad("schedule", "payload", "an object with type")
        }
        if (schedPayload["type"].asString ?? "").isEmpty {
            return .object(["error": .string("schedule: payload.type required")])
        }
        let (interval, intervalBad) = intArg(payload, "interval_seconds", 60, "schedule", 1)
        if let intervalBad { return intervalBad }
        // Per-fire bound, seconds. Optional: absent = wait forever (never defaulted).
        var timeout: Double? = nil
        if let raw = payload.asObject?["timeout"], !raw.isNull {
            guard let t = raw.asDouble, t > 0 else {
                return Refusal.bad("schedule", "timeout", "a positive number of seconds")
            }
            timeout = t
        }
        let now = nowSecs()
        var sch: JSON = .object([
            "id": .string(mintId()),
            "target": .string(target),
            "payload": schedPayload,
            "interval_seconds": .integer(interval),
            "created_at": .double(now),
            "next_run": .double(now + Double(interval)),
            "run_count": .integer(0),
            "paused": .bool(false),
        ])
        if let timeout { sch["timeout"] = .double(timeout) }
        var schedules = await loadSchedules(agentId, kernel)
        schedules.append(sch)
        if let err = await saveSchedules(agentId, kernel, schedules) {
            // Nothing is cached in RAM: a failed persist leaves no phantom schedule.
            return persistErr("schedule", err)
        }
        return .object(["schedule_id": sch["id"], "schedule": sch])
    }

    private func unscheduleReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        guard let sid = payload["schedule_id"].asString else {
            return .object(["error": .string("unschedule: schedule_id required")])
        }
        var schedules = await loadSchedules(agentId, kernel)
        let before = schedules.count
        schedules.removeAll { $0["id"].asString == sid }
        let removed = schedules.count < before
        if removed {
            if let err = await saveSchedules(agentId, kernel, schedules) {
                return persistErr("unschedule", err)
            }
        }
        return .object(["removed": .bool(removed), "schedule_id": .string(sid)])
    }

    private func listReply(_ agentId: AgentId, kernel: Kernel) async -> JSON {
        .object(["schedules": .array(await loadSchedules(agentId, kernel))])
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
        if let sid = payload["schedule_id"].asString {
            var schedules = await loadSchedules(agentId, kernel)
            var touched = 0
            for i in schedules.indices where schedules[i]["id"].asString == sid {
                schedules[i]["paused"] = .bool(paused)
                touched += 1
            }
            if touched > 0, let err = await saveSchedules(agentId, kernel, schedules) {
                return persistErr(paused ? "pause" : "resume", err)
            }
            var out: JSON = .object(["schedule_id": .string(sid)])
            out[key] = .bool(touched > 0)
            return out
        }
        if let agent = kernel.agent(agentId) {
            _ = agent.updateMeta(["paused": .bool(paused)])
            await kernel.persistRecord(agent)
        }
        var out: JSON = .object(["scheduler_id": .string(agentId.value)])
        out[key] = .bool(true)
        return out
    }

    private func tickNowReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        guard let sid = payload["schedule_id"].asString else {
            return .object(["error": .string("tick_now: schedule_id required")])
        }
        var schedules = await loadSchedules(agentId, kernel)
        guard let i = schedules.firstIndex(where: { $0["id"].asString == sid }) else {
            return .object(["error": .string("schedule '\(sid)' not found")])
        }
        // Same order as the tick: bump → persist → fire.
        let count = schedules[i]["run_count"].asInt ?? 0
        schedules[i]["run_count"] = .integer(count + 1)
        if let err = await saveSchedules(agentId, kernel, schedules) {
            return persistErr("tick_now", err)
        }
        await fireMany(agentId, [schedules[i]], kernel)
        return .object(["fired": .bool(true), "schedule_id": .string(sid)])
    }

    private func historyReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        let (rawLimit, limitBad) = intArg(payload, "limit", 100, "history", 1)
        if let limitBad { return limitBad }
        let limit = min(500, Int(rawLimit))
        var entries = await readHistory(agentId, kernel, limit)
        if let sid = payload["schedule_id"].asString {
            entries = entries.filter { $0["schedule_id"].asString == sid }
        }
        return .object(["history": .array(entries), "count": .integer(Int64(entries.count))])
    }
}
