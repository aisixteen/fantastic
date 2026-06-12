// Recurring-task scheduler as an agent.
//
// Mirrors the canonical Python `scheduler` (and Rust `fantastic-scheduler`)
// verb-for-verb. State persists THROUGH a `file_bridge` AGENT (the gated fs
// edge), referenced by `file_bridge_id` on this agent's record — this bundle
// owns NO disk surface of its own and never touches FileManager. Sidecars are
// store-relative (`agents/<id>/…`, wired to the `.fantastic` store, next to the
// agent's own agent.json — one store, no `.fantastic/.fantastic/…` double-nest):
//
//   - `schedules.json` — `[{id, target, payload, interval_seconds, next_run,
//                          paused, run_count, created_at}, …]`
//   - `history.jsonl`  — append-only, one event per fire, ring-trimmed to HISTORY_MAX.
//
// `boot` / `schedule` FAILFAST until `file_bridge_id` is set. After every fire
// the scheduler emits `{type:"schedule_fired", …}` to its OWN inbox AND the
// target's inbox.

import FantasticJSON
import FantasticKernel
import Foundation

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

        State persists THROUGH a file_bridge (set `file_bridge_id`; wire it to the
        `.fantastic` store): `agents/<id>/schedules.json` + `history.jsonl`.

        Verbs: reflect · boot · shutdown · schedule {target, payload, interval_seconds} ·
        unschedule {schedule_id} · list · pause/resume {schedule_id?} · tick_now {schedule_id} ·
        history {limit?, schedule_id?}. Each schedule fires `kernel.send(target, payload)` on its
        interval and emits `schedule_fired` to the scheduler's inbox AND the target's.
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

    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
        _ = shutdownReply(agentId)
    }

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        _ = shutdownReply(agentId)
    }

    // ── persistence (THROUGH a file_bridge provider) ────────────

    private func fileBridgeId(_ agentId: AgentId, _ kernel: Kernel) -> String? {
        kernel.agent(agentId)?.metaValue(forKey: "file_bridge_id")?.asString
    }

    private func schedulesPath(_ id: AgentId) -> String { "agents/\(id.value)/schedules.json" }
    private func historyPath(_ id: AgentId) -> String { "agents/\(id.value)/history.jsonl" }

    /// Read a file THROUGH the wired provider. Unwired / missing ⇒ nil.
    private func fileRead(_ id: AgentId, _ kernel: Kernel, _ path: String) async -> String? {
        guard let fid = fileBridgeId(id, kernel) else { return nil }
        let r = await kernel.send(
            AgentId(fid), .object(["type": .string("read"), "path": .string(path)]))
        return r["content"].asString
    }

    /// Write a file THROUGH the provider. Returns an error string, or nil on success.
    @discardableResult
    private func fileWrite(_ id: AgentId, _ kernel: Kernel, _ path: String, _ content: String)
        async -> String?
    {
        guard let fid = fileBridgeId(id, kernel) else { return "file_bridge_id unset" }
        let w = await kernel.send(
            AgentId(fid),
            .object([
                "type": .string("write"), "path": .string(path), "content": .string(content),
            ]))
        if let err = w["error"].asString { return err }
        return nil
    }

    private func loadSchedules(_ id: AgentId, _ kernel: Kernel) async -> [JSON] {
        guard let raw = await fileRead(id, kernel, schedulesPath(id)),
            let parsed = try? JSON.parse(raw), let arr = parsed.asArray
        else { return [] }
        return arr
    }

    @discardableResult
    private func saveSchedules(_ id: AgentId, _ kernel: Kernel, _ schedules: [JSON]) async
        -> String?
    {
        await fileWrite(
            id, kernel, schedulesPath(id), JSON.array(schedules).serializePretty(indent: 2))
    }

    private func appendHistory(_ id: AgentId, _ kernel: Kernel, _ event: JSON) async {
        let prev = await fileRead(id, kernel, historyPath(id)) ?? ""
        var combined = prev + event.serialize() + "\n"
        // Ring-trim past 2× MAX.
        let lines = combined.split(separator: "\n", omittingEmptySubsequences: false)
            .map(String.init)
        // `split` on a trailing "\n" yields a final "" element — drop it for counting.
        let real = lines.last == "" ? Array(lines.dropLast()) : lines
        if real.count > 2 * HISTORY_MAX {
            combined = real.suffix(HISTORY_MAX).joined(separator: "\n") + "\n"
        }
        await fileWrite(id, kernel, historyPath(id), combined)
    }

    private func readHistory(_ id: AgentId, _ kernel: Kernel, _ limit: Int) async -> [JSON] {
        guard let raw = await fileRead(id, kernel, historyPath(id)) else { return [] }
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
            var anyFired = false
            for i in schedules.indices {
                if schedules[i]["paused"].asBool == true { continue }
                let nextRun = schedules[i]["next_run"].asDouble ?? 0
                if now < nextRun { continue }
                await fireSchedule(agentId, schedules[i], kernel)
                let count = schedules[i]["run_count"].asInt ?? 0
                let interval = schedules[i]["interval_seconds"].asDouble ?? 60
                schedules[i]["run_count"] = .integer(count + 1)
                schedules[i]["next_run"] = .double(nowSecs() + interval)
                anyFired = true
            }
            if anyFired { await saveSchedules(agentId, kernel, schedules) }
        }
    }

    private func fireSchedule(_ agentId: AgentId, _ sch: JSON, _ kernel: Kernel) async {
        let target = sch["target"].asString ?? ""
        let payload = sch["payload"]
        let schedId = sch["id"].asString ?? ""
        let ts = nowSecs()
        var error: JSON = .null
        var result: JSON = .null
        if target.isEmpty {
            error = .string("empty target")
        } else {
            let reply = await kernel.send(AgentId(target), payload)
            if let err = reply["error"].asString { error = .string(err) } else { result = reply }
        }
        let event: JSON = .object([
            "type": .string("schedule_fired"),
            "schedule_id": .string(schedId),
            "scheduler_id": .string(agentId.value),
            "target": .string(target),
            "payload": payload,
            "result": result,
            "error": error,
            "ts": .double(ts),
            "duration_ms": .integer(Int64((nowSecs() - ts) * 1000)),
        ])
        await appendHistory(agentId, kernel, event)
        await kernel.emit(agentId, event)
        if !target.isEmpty && target != agentId.value {
            await kernel.emit(AgentId(target), event)
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
            "file_bridge_id": fileBridgeId(agentId, kernel).map { JSON.string($0) } ?? .null,
            "running": .bool(running),
            "verbs": .object([
                "reflect": .string("Identity + tick state + file_bridge_id. No args."),
                "boot": .string("Idempotent. Starts the tick loop. Requires file_bridge_id."),
                "shutdown": .string("Idempotent. Cancels the tick loop."),
                "schedule": .string(
                    "args: target:str, payload:dict, interval_seconds:int (default 60)."),
                "unschedule": .string("args: schedule_id:str."),
                "list": .string("No args. Returns {schedules:[...]}."),
                "pause": .string("args: schedule_id:str?. Pauses one or all."),
                "resume": .string("args: schedule_id:str?. Resumes one or all."),
                "tick_now": .string("args: schedule_id:str. Fires immediately."),
                "history": .string("args: limit:int?, schedule_id:str?."),
            ]),
            "emits": .object([
                "schedule_fired": .string(
                    "{type, schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} broadcast to scheduler's inbox AND target's inbox on every fire"
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
        if fileBridgeId(agentId, kernel) == nil {
            return .object(["error": .string("scheduler: file_bridge_id required")])
        }
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
        if fileBridgeId(agentId, kernel) == nil {
            return .object(["error": .string("scheduler: file_bridge_id required")])
        }
        let target = payload["target"].asString ?? ""
        if target.isEmpty {
            return .object(["error": .string("schedule: target required")])
        }
        let schedPayload = payload.asObject?["payload"] ?? .object([:])
        if (schedPayload["type"].asString ?? "").isEmpty {
            return .object(["error": .string("schedule: payload.type required")])
        }
        let interval = max(1, payload["interval_seconds"].asInt ?? 60)
        let now = nowSecs()
        let sch: JSON = .object([
            "id": .string(mintId()),
            "target": .string(target),
            "payload": schedPayload,
            "interval_seconds": .integer(interval),
            "created_at": .double(now),
            "next_run": .double(now + Double(interval)),
            "run_count": .integer(0),
            "paused": .bool(false),
        ])
        var schedules = await loadSchedules(agentId, kernel)
        schedules.append(sch)
        if let err = await saveSchedules(agentId, kernel, schedules) {
            return .object(["error": .string("schedule: persist failed: \(err)")])
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
                return .object(["error": .string("unschedule: persist failed: \(err)")])
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
            if touched > 0 { await saveSchedules(agentId, kernel, schedules) }
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
        for i in schedules.indices where schedules[i]["id"].asString == sid {
            await fireSchedule(agentId, schedules[i], kernel)
            let count = schedules[i]["run_count"].asInt ?? 0
            schedules[i]["run_count"] = .integer(count + 1)
            if let err = await saveSchedules(agentId, kernel, schedules) {
                return .object(["error": .string("tick_now: persist failed: \(err)")])
            }
            return .object(["fired": .bool(true), "schedule_id": .string(sid)])
        }
        return .object(["error": .string("schedule '\(sid)' not found")])
    }

    private func historyReply(_ agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
        let limit = min(500, max(1, payload["limit"].asInt.map(Int.init) ?? 100))
        var entries = await readHistory(agentId, kernel, limit)
        if let sid = payload["schedule_id"].asString {
            entries = entries.filter { $0["schedule_id"].asString == sid }
        }
        return .object(["history": .array(entries), "count": .integer(Int64(entries.count))])
    }
}
