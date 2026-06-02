// Recurring task scheduler.
//
// Mirrors Rust's `fantastic-scheduler::SchedulerBundle`. Each
// schedule entry fires `kernel.send(targetId, payload)` on its
// interval. Backed by DispatchSourceTimer; one timer per active
// schedule.

import FantasticJSON
import FantasticKernel
import Foundation

public let HANDLER_MODULE = "scheduler.tools"

public final class SchedulerBundle: AgentBundle, @unchecked Sendable {
    public let name = "scheduler"

    public init() {}

    private let lock = NSLock()
    private var timers: [String: DispatchSourceTimer] = [:]

    public var readme: String? {
        """
        scheduler — recurring tasks as an agent.
        Verbs: schedule, cancel, list. Each fires `kernel.send(target, payload)` on its interval.
        """
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        switch verb {
        case "reflect":
            return [
                "id": .string(agentId.value),
                "sentence": .string("Scheduler — fires recurring kernel.send."),
                "kind": .string("scheduler"),
                "active": .integer(Int64(activeCount())),
                "verbs": [
                    "schedule":
                        "args: name, interval_ms, target, payload. Adds a recurring send.",
                    "cancel": "args: name. Stops the schedule.",
                ] as JSON,
            ] as JSON
        case "boot":
            return .object(["ok": .bool(true)])
        case "shutdown":
            cancelAll()
            return .object(["ok": .bool(true)])
        case "schedule":
            return scheduleVerb(payload: payload, kernel: kernel)
        case "cancel":
            return cancelVerb(payload: payload)
        case "list":
            return listVerb()
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
    }

    public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
        cancelAll()
    }

    private func scheduleVerb(payload: JSON, kernel: Kernel) -> JSON {
        guard let name = payload["name"].asString,
            let intervalMs = payload["interval_ms"].asInt,
            let targetStr = payload["target"].asString
        else {
            return .object([
                "error": .string("schedule requires name, interval_ms, target")
            ])
        }
        let target = AgentId(targetStr)
        let body = payload["payload"]
        let timer = DispatchSource.makeTimerSource(queue: .global())
        timer.schedule(
            deadline: .now() + .milliseconds(Int(intervalMs)),
            repeating: .milliseconds(Int(intervalMs)))
        timer.setEventHandler { [weak kernel] in
            guard let kernel = kernel else { return }
            Task {
                _ = await kernel.send(target, body)
            }
        }
        timer.resume()
        lock.lock()
        timers[name]?.cancel()
        timers[name] = timer
        lock.unlock()
        return .object([
            "ok": .bool(true),
            "name": .string(name),
        ])
    }

    private func cancelVerb(payload: JSON) -> JSON {
        guard let name = payload["name"].asString else {
            return .object(["error": .string("cancel requires name")])
        }
        lock.lock()
        let removed = timers.removeValue(forKey: name)
        lock.unlock()
        removed?.cancel()
        return .object([
            "ok": .bool(true),
            "cancelled": .bool(removed != nil),
        ])
    }

    private func listVerb() -> JSON {
        lock.lock()
        let names = Array(timers.keys).sorted()
        lock.unlock()
        return .object([
            "schedules": .array(names.map { .string($0) })
        ])
    }

    private func cancelAll() {
        lock.lock()
        let snapshot = timers
        timers.removeAll()
        lock.unlock()
        for (_, t) in snapshot { t.cancel() }
    }

    private func activeCount() -> Int {
        lock.lock()
        defer { lock.unlock() }
        return timers.count
    }
}
