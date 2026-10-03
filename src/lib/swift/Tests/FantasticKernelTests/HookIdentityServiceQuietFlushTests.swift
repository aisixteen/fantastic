// Owner decisions 2026-10-03 (audit 2, set 2) on the task table and the flusher —
// twin of python `tests/kernel/test_hook_identity_service_tasks_quiet_flush.py`
// and rust `tasks/hook_identity_tests.rs` + `fantastic-ram-store/tests/quiet_flush.rs`.
//
// - G-3: every `onCancel` hook — the `cancel` verb's path and an item's own
//   deadline alike — and every `onDelete` hook runs AS THE AGENT ITSELF (its own
//   id is the sender), never as the operator or the root.
// - KV-5: `cancel` refuses the kernel's own service tasks (the execution
//   flusher, the record persister — keepalive loops the root owns); `ps` still
//   lists them, and a group kill leaves them out.
// - CLI-B: the kernel's own ledger-flush send publishes NOTHING on the state
//   stream (its rows stay recorded by the flush row) — a long-poll waiter is not
//   woken; an id block reserved inside the flush stays quiet too — the root
//   record holds the bound, the next published update carries it.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel
@testable import FantasticKernelState
@testable import FantasticRamStore

private final class HookLog: @unchecked Sendable {
    private let lock = NSLock()
    private var _entries: [String] = []
    var entries: [String] { lock.withLock { _entries } }
    func add(_ s: String) { lock.withLock { _entries.append(s) } }
}

private struct IdBundle: AgentBundle {
    let name = "hook_identity_probe"
    let log: HookLog

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown":
            return nil
        case "slow":
            try? await Task.sleep(nanoseconds: 30_000_000_000)
            return .object(["finished": .bool(true)])
        // a root-owned keepalive loop forked INSIDE this dispatch: it is in this
        // task's process group by parentage
        case "fork_service":
            let root = kernel.root!.id
            kernel.spawnTask(agent: root, verb: "svc_probe", keepalive: true) {
                try? await Task.sleep(nanoseconds: 30_000_000_000)
            }
            try? await Task.sleep(nanoseconds: 30_000_000_000)
            return .object(["finished": .bool(true)])
        case let other:
            return .object(["error": .string("unknown verb \(other)")])
        }
    }

    func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        log.add("cancel \(agentId.value) as \(KernelTaskLocals.currentSender?.value ?? "operator")")
        // an emit on its OWN inbox — ai_core's final `status done` to a browser
        await kernel.emit(agentId, doneStatus)
    }

    func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        log.add("delete \(agentId.value) as \(KernelTaskLocals.currentSender?.value ?? "operator")")
    }
}

private let doneStatus: JSON = .object([
    "type": .string("status"), "phase": .string("done"), "reason": .string("cancelled"),
])

private func makeKernel(_ log: HookLog) -> Kernel {
    let r = BundleRegistry()
    r.register("kernel_state.tools", KernelStateBundle())
    r.register("ram_store.tools", RamStoreBundle())
    r.register("hook_identity_probe.tools", IdBundle(log: log))
    let k = Kernel(storage: .inMemory, bundles: r)
    let root = Agent(id: AgentId("kernel_state"), handlerModule: "kernel_state.tools", parentId: nil)
    k.register(root)
    k.setRoot(root)
    return k
}

@discardableResult
private func call(_ k: Kernel, _ target: String, _ payload: JSON, timeout: Double? = nil) async
    -> Outcome
{
    await k.send([Item(AgentId(target), payload, timeout: timeout)])[0]
}

private func create(_ k: Kernel, _ id: String, parent: String) async {
    let r = await call(
        k, "kernel",
        .object([
            "type": .string("create_agent"), "parent_id": .string(parent), "id": .string(id),
            "handler_module": .string("hook_identity_probe.tools"),
        ])
    ).result
    #expect(r["id"].asString == id, "\(r.serialize())")
}

/// `gc` under `mid` under the root — NOT a direct child, so the root's baseline
/// `$children` does not reach it.
private func grandchild(_ k: Kernel) async -> AgentId {
    await create(k, "mid", parent: "kernel_state")
    await create(k, "gc", parent: "mid")
    return AgentId("gc")
}

private func taskOf(_ k: Kernel, _ agent: String, _ verb: String) async throws -> Int64 {
    for _ in 0..<300 {
        if let row = k.tasks.rows().first(where: {
            $0["agent"].asString == agent && $0["verb"].asString == verb
        }), let id = row["id"].asInt {
            return id
        }
        try await Task.sleep(nanoseconds: 10_000_000)
    }
    Issue.record("timed out waiting for \(agent)'s \(verb) task")
    throw CancellationError()
}

private func drained(_ inbox: Inbox) -> [JSON] {
    var out: [JSON] = []
    while let item = inbox.tryTake() { out.append(item.payload) }
    return out
}

/// A ram_store `memory` under the root, named by the root; the kernel started.
private func started(_ k: Kernel) async {
    let r = await call(
        k, "kernel",
        .object([
            "type": .string("create_agent"), "parent_id": .string("kernel_state"),
            "id": .string("memory"), "handler_module": .string("ram_store.tools"),
        ])
    ).result
    #expect(r["id"].asString == "memory", "\(r.serialize())")
    k.update(AgentId("kernel_state"), ["memory_store_id": .string("memory")])
    let out = await k.start(bootAll: false)
    #expect(out["started"].asBool == true, "\(out.serialize())")
}

/// A state tap that records every event and flags a long-poll style waiter.
private final class Tap: @unchecked Sendable {
    private let lock = NSLock()
    private var _seen: [JSON] = []
    private var _woken = false
    var seen: [JSON] { lock.withLock { _seen } }
    var woken: Bool { lock.withLock { _woken } }
    func hit(_ e: JSON) { lock.withLock { _seen.append(e); _woken = true } }
    func reset() { lock.withLock { _seen = []; _woken = false } }
}

private func serviceRefusal(_ id: Int64, _ verb: String) -> JSON {
    .object([
        "error": .string("cancel: task \(id) (\(verb)) is a kernel service task — not cancellable"),
        "reason": .string("bad_arg"),
        "hint": .string("it runs for the life of the kernel and stays listed in ps; kernel_stop ends it"),
        "arg": .string("task_id"),
    ])
}

@Suite("hook identity, service tasks, the silent flush (audit 2 set 2)")
struct HookIdentityServiceQuietFlushTests {

    // ── G-3 ──

    @Test func onCancelRunsAsTheAgentOnTheCancelVerbPath() async throws {
        let log = HookLog()
        let k = makeKernel(log)
        let gc = await grandchild(k)
        let inbox = k.ensureInbox(gc)
        let pending = Task { await call(k, "gc", .object(["type": .string("slow")])) }
        let pid = try await taskOf(k, "gc", "slow")
        let out = await call(
            k, "kernel", .object(["type": .string("cancel"), "task_id": .integer(pid)])
        ).result
        #expect(out == .object(["cancelled": .bool(true), "killed": .array([.integer(pid)])]))
        #expect(await pending.value.status == .cancelled)
        #expect(log.entries == ["cancel gc as gc"])
        // the own-inbox emit is delivered — it used to be gated root → gc and dropped
        #expect(drained(inbox).contains(doneStatus))
    }

    @Test func onCancelRunsAsTheAgentOnTheDeadlinePath() async throws {
        let log = HookLog()
        let k = makeKernel(log)
        let gc = await grandchild(k)
        let inbox = k.ensureInbox(gc)
        let out = await call(k, "gc", .object(["type": .string("slow")]), timeout: 0.2)
        #expect(out.status == .timedOut, "\(out.result.serialize())")
        #expect(log.entries == ["cancel gc as gc"])
        #expect(drained(inbox).contains(doneStatus))
    }

    @Test func onDeleteRunsAsTheAgentItself() async throws {
        let log = HookLog()
        let k = makeKernel(log)
        _ = await grandchild(k)
        let out = await call(
            k, "kernel", .object(["type": .string("delete_agent"), "id": .string("mid")])
        ).result
        #expect(out["deleted"].asBool == true, "\(out.serialize())")
        // depth-first, each hook as its own agent — not the root that ran the verb
        #expect(log.entries == ["delete gc as gc", "delete mid as mid"])
    }

    // ── KV-5 ──

    @Test func cancelRefusesTheFlusherAndItKeepsFlushing() async throws {
        let k = makeKernel(HookLog())
        await started(k)
        let fid = try await taskOf(k, "kernel_state", "flush_executions")
        let out = await call(
            k, "kernel", .object(["type": .string("cancel"), "task_id": .integer(fid)]))
        #expect(out.status == .errored)
        #expect(out.result == serviceRefusal(fid, "flush_executions"))
        let ps = await call(k, "kernel", .object(["type": .string("ps")])).result
        #expect((ps["tasks"].asArray ?? []).contains { $0["id"].asInt == fid }, "still listed")
        // and still flushing: a new row leaves RAM
        let pid = await call(k, "kernel", .object(["type": .string("list_agents")])).pid!.value
        for _ in 0..<100 where k.rows.contains(pid) {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        #expect(!k.rows.contains(pid), "the flusher kept writing")
    }

    @Test func aGroupKillLeavesAServiceTaskAlive() async throws {
        let k = makeKernel(HookLog())
        await create(k, "forker", parent: "kernel_state")
        let pending = Task { await call(k, "forker", .object(["type": .string("fork_service")])) }
        let pid = try await taskOf(k, "forker", "fork_service")
        let svc = try await taskOf(k, "kernel_state", "svc_probe")
        let row = k.tasks.rows().first { $0["id"].asInt == svc }
        #expect(row?["parent"].asInt == pid, "the service loop is in the group")
        let out = await call(
            k, "kernel", .object(["type": .string("cancel"), "task_id": .integer(pid)])
        ).result
        #expect(out == .object(["cancelled": .bool(true), "killed": .array([.integer(pid)])]))
        #expect(await pending.value.status == .cancelled)
        #expect(k.tasks.contains(TaskId(svc)), "left alive")
    }

    // ── CLI-B ──

    @Test func theLedgerFlushPublishesNothingOnTheStateStream() async throws {
        let k = makeKernel(HookLog())
        await started(k)
        _ = await k.flushExecutions()  // start from an empty RAM
        let tap = Tap()
        let token = k.subscribe { e, _ in tap.hit(e) }
        defer { k.unsubscribe(token) }
        await call(k, "kernel", .object(["type": .string("list_agents")]))  // rows to flush
        tap.reset()
        let out = await k.flushExecutions()
        #expect((out["flushed"].asInt ?? 0) > 0, "\(out.serialize())")
        #expect(tap.seen.isEmpty, "\(tap.seen.map { $0.serialize() })")
        #expect(!tap.woken, "a flush wakes no waiter")
        // the flush is recorded: its own flush row is in the store
        let dump = await call(k, "memory", .object(["type": .string("dump")])).result
        #expect(
            (dump["tables"]["rows"].asArray ?? []).contains {
                $0["row"]["kind"].asString == "flush" && $0["row"]["id"] == out["flush"]
            })
        // traffic outside the flush still publishes
        await call(k, "kernel", .object(["type": .string("list_agents")]))
        #expect(!tap.seen.isEmpty, "the stream is not muted, only the flush")
    }

    @Test func anIdBlockReservedInsideTheFlushStaysQuiet() async throws {
        let k = makeKernel(HookLog())
        await started(k)
        _ = await k.flushExecutions()
        let tap = Tap()
        let token = k.subscribe { e, _ in tap.hit(e) }
        defer { k.unsubscribe(token) }
        await call(k, "kernel", .object(["type": .string("list_agents")]))
        // the flush row's id is the first past the block: the flush mints the
        // one id it needs (it sends nothing) and crosses the bound inside the
        // silent flush
        while k.nextId < k.idReserved { _ = k.mintId() }
        tap.reset()
        let before = k.idReserved
        _ = await k.flushExecutions()
        #expect(k.idReserved > before)
        // quiet on the stream: the reservation publishes no `updated`
        #expect(tap.seen.isEmpty, "\(tap.seen.map { $0.serialize() })")
        // ...the root record holds the bound; the next published update carries it
        #expect(k.root?.metaValue(forKey: "next_id") == .integer(k.idReserved))
    }
}
