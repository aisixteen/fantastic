// KERNEL LIFECYCLE — drain, `shutdown_agents`, `Kernel.shutdown()`, a killed
// task's sends, a fork's parentage.
//
// Twins of python `tests/kernel/test_message_persistence.py` (the drain and
// `shutdown_agents` half), `tests/kernel/test_graceful_shutdown.py` (the
// depth-first hook walk), `tests/kernel/test_timeout_cancel_parity.py` (a
// dying task registers no new work) and `tests/kernel/test_fork_parentage.py`
// (a fork's work is parented to the fork, not the dispatch that made it).

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

// MARK: - shared recorder

/// What the test bundles record. One lock, several logs.
private final class Log: @unchecked Sendable {
    private let lock = NSLock()
    private var entries: [String] = []
    private var flags: Set<String> = []
    private var pumpGoneAtHook: Bool? = nil

    func append(_ s: String) {
        lock.lock()
        entries.append(s)
        lock.unlock()
    }

    func all() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return entries
    }

    func flag(_ s: String) {
        lock.lock()
        flags.insert(s)
        lock.unlock()
    }

    func has(_ s: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return flags.contains(s)
    }

    func notePump(gone: Bool) {
        lock.lock()
        pumpGoneAtHook = gone
        lock.unlock()
    }

    func pumpGone() -> Bool? {
        lock.lock()
        defer { lock.unlock() }
        return pumpGoneAtHook
    }
}

// MARK: - a test bundle that ADVERTISES `shutdown` and can hold a task in flight

/// Python's `shut_probe.tools`: `VERBS = {reflect, shutdown, slow, work}`.
private struct ShutProbe: AgentBundle {
    let name = "shut_probe"
    let log: Log

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot":
            return .null
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": .string("shutdown probe"),
                "verbs": .object(["shutdown": .string("wraps up")]),
            ])
        case "shutdown":
            log.append(agentId.value)
            // The shutdown turn may still use its tools: a nested send passes
            // the drain.
            let r = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[0]
            return .object(["wrapped": .bool(true), "tools_ok": .bool(r.status == .completed)])
        case "slow":
            log.flag("running:\(agentId.value)")
            try? await Task.sleep(nanoseconds: 30_000_000_000)
            return .object(["finished": .bool(true)])
        case "work":
            return .object(["ok": .bool(true)])
        case let other:
            return .object(["error": .string("unknown '\(other)'"), "reason": .string("unhandled")])
        }
    }
}

/// Python's `_stub_bundle`: a handler that answers nothing, carrying whatever
/// hooks the test wants to observe. `onShutdown == nil` leaves the protocol
/// default in place — which IS `onDelete`, python's fallback.
private struct HookProbe: AgentBundle {
    let name: String
    let log: Log
    let shutdownHook: (@Sendable (AgentId, Kernel) async throws -> Void)?
    let deleteHook: (@Sendable (AgentId, Kernel) async throws -> Void)?

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": .string("hook probe"),
                "verbs": .object([:]),
            ])
        default:
            return .null
        }
    }

    func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
        guard let shutdownHook else {
            try await onDelete(agentId: agentId, kernel: kernel)
            return
        }
        try await shutdownHook(agentId, kernel)
    }

    func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        try await deleteHook?(agentId, kernel)
    }
}

/// Python's `mailbox_probe.tools` + the forking agent of `test_fork_parentage`.
private struct ForkProbe: AgentBundle {
    let name = "fork_probe"
    let log: Log

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown":
            return .null
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": .string("fork probe"),
                "verbs": .object([:]),
            ])
        case "note":
            log.append("note:\(payload["text"].asString ?? "")")
            return .object(["noted": .bool(true)])
        // The `start` of a job: fork the program, return at once. The program
        // sends to a peer AFTER this dispatch has settled.
        case "fork":
            let peer = AgentId(payload["peer"].asString ?? "")
            let job = kernel.spawnTask(agent: agentId, verb: "job") {
                try? await Task.sleep(nanoseconds: 50_000_000)
                _ = await kernel.send([
                    Item(peer, .object(["type": .string("note"), "text": .string("from the program")]))
                ])
            }
            return .object(["job_id": .integer(job.value), "status": .string("running")])
        case let other:
            return .object(["error": .string("unknown '\(other)'"), "reason": .string("unhandled")])
        }
    }
}

// MARK: - harness

private func makeKernel(_ bundles: [(String, any AgentBundle)]) -> Kernel {
    let registry = BundleRegistry()
    for (module, b) in bundles { registry.register(module, b) }
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

private func create(
    _ kernel: Kernel, _ id: String, _ module: String, parent: String = "core",
    canCall: JSON = .null, callableBy: JSON = .null
) async -> AgentId {
    var payload: OrderedDictionary<String, JSON> = [
        "type": .string("create_agent"),
        "parent_id": .string(parent),
        "handler_module": .string(module),
        "id": .string(id),
    ]
    if case .object = canCall { payload["can_call"] = canCall }
    if case .object = callableBy { payload["callable_by"] = callableBy }
    let r = await kernel.send([Item(AgentId("core"), .object(payload))])[0].result
    #expect(r["id"].asString == id, "create_agent failed: \(r.serialize())")
    return AgentId(id)
}

private func send(_ kernel: Kernel, _ target: String, _ verb: String) async -> Outcome {
    await kernel.send([Item(AgentId(target), .object(["type": .string(verb)]))])[0]
}

private func waitUntil(_ what: String, _ cond: () async -> Bool) async throws {
    for _ in 0..<300 {
        if await cond() { return }
        try await Task.sleep(nanoseconds: 10_000_000)
    }
    Issue.record("timed out waiting for \(what)")
}

// MARK: - drain + shutdown_agents (test_message_persistence.py)

@Suite("Lifecycle: drain + shutdown_agents", .serialized)
struct DrainTests {
    @Test func drainingRefusesNewRootsButNotLiveWorkOrShutdownTraffic() async throws {
        let log = Log()
        let kernel = makeKernel([("shut_probe.tools", ShutProbe(log: log))])
        let a = await create(kernel, "sp", "shut_probe.tools")
        kernel.setDraining(true)
        defer { kernel.setDraining(false) }
        let out = await send(kernel, a.value, "work")
        #expect(out.status == .failed && out.result["reason"].asString == "draining", "\(out.result.serialize())")
        #expect(
            out.result["error"].asString
                == "send: the kernel is draining for shutdown — no new work is accepted; in-flight work may finish")
        #expect(out.pid == nil, "a refused root costs no id")
        let inside = await KernelTaskLocals.withShutdown { await send(kernel, a.value, "work") }
        #expect(inside.status == .completed, "\(inside.result.serialize())")
        // The root's control surface stays open while draining.
        let ps = await send(kernel, "kernel", "ps")
        #expect(ps.status == .completed)
    }

    @Test func shutdownAgentsTellsAdvertisersWaitsThenCancelsTheRest() async throws {
        let log = Log()
        let kernel = makeKernel([("shut_probe.tools", ShutProbe(log: log))])
        let a = await create(kernel, "sp_a", "shut_probe.tools", canCall: .object(["$parent": .string("*")]))
        let b = await create(kernel, "sp_b", "shut_probe.tools", canCall: .object(["$parent": .string("*")]))
        kernel.quietTimeout = 0.3
        let slow = Task { await send(kernel, b.value, "slow") }
        try await waitUntil("the slow handler to start") { log.has("running:sp_b") }
        // A keepalive loop (a persister) survives the quiet phase. THE KERNEL
        // CREATES THE TASK — only it can mint the id the fork runs under.
        let keep = kernel.spawnTask(agent: AgentId("core"), verb: "message_pump", keepalive: true) {
            try? await Task.sleep(nanoseconds: 30_000_000_000)
        }
        let out = await send(kernel, "kernel", "shutdown_agents").result
        #expect(out["drained"].asBool == true, "\(out.serialize())")
        #expect((out["shut"].asArray ?? []).compactMap { $0.asString }.sorted() == [a.value, b.value], "\(out.serialize())")
        #expect(Set(log.all()) == [a.value, b.value], "every advertiser was told")
        #expect(!(out["cancelled"].asArray ?? []).isEmpty, "the slow task was cancelled after the quiet bound")
        #expect(kernel.tasks.contains(keep), "the keepalive loop is not pending work")
        let settled = await slow.value
        #expect(settled.status == .cancelled, "\(settled.result.serialize())")
        // The kernel keeps serving reads; a new root is refused.
        let r = await send(kernel, a.value, "work")
        #expect(r.result["reason"].asString == "draining")
        let again = await send(kernel, "kernel", "shutdown_agents").result
        #expect(again["drained"].asBool == true && again["cancelled"] == .array([]), "\(again.serialize())")
        _ = await kernel.cancelTask(keep)
        kernel.setDraining(false)
    }

    @Test func shutdownAgentsIsRootOnly() async throws {
        let log = Log()
        let kernel = makeKernel([("shut_probe.tools", ShutProbe(log: log))])
        let a = await create(kernel, "sp_c", "shut_probe.tools")
        let r = await send(kernel, a.value, "shutdown_agents").result
        #expect(
            r["error"].asString
                == "shutdown_agents: root control surface only; address the kernel root (alias 'kernel')",
            "\(r.serialize())")
        let k = await send(kernel, a.value, "shutdown_kernel").result
        #expect(
            k["error"].asString
                == "shutdown_kernel: root control surface only; address the kernel root (alias 'kernel')",
            "\(k.serialize())")
    }

    @Test func shutdownKernelAcksThenRequestsTheStop() async throws {
        let kernel = makeKernel([])
        let r = await send(kernel, "kernel", "shutdown_kernel").result
        #expect(r == .object(["type": .string("shutdown_kernel"), "ok": .bool(true)]), "\(r.serialize())")
        // The trigger is deferred so the ack flushes first; the waiter wakes.
        let woke = Task { await kernel.shutdownRequested(); return true }
        let winner = await withTaskGroup(of: Bool.self) { group in
            group.addTask { await woke.value }
            group.addTask {
                try? await Task.sleep(nanoseconds: 2_000_000_000)
                return false
            }
            let first = await group.next() ?? false
            group.cancelAll()
            return first
        }
        #expect(winner, "shutdownRequested did not fire within 2 s of the ack")
        // Idempotent: a late waiter returns at once.
        await kernel.shutdownRequested()
    }

    @Test func kernelShutdownRunsTheFlushHookBeforeTheWalk() async throws {
        let log = Log()
        let kernel = makeKernel([
            ("walk.tools",
             HookProbe(
                name: "walk", log: log,
                shutdownHook: { _, k in
                    log.append("walk")
                    // The pump was landed + forgotten BEFORE the walk reached us.
                    log.notePump(gone: k.messagePump == nil)
                }, deleteHook: nil)),
        ])
        _ = await create(kernel, "w", "walk.tools")
        kernel.installMessagePump(after: kernel.rows.nextId - 1)
        #expect(kernel.messagePump != nil)
        await kernel.shutdown()
        #expect(log.all() == ["walk"], "\(log.all())")
        #expect(log.pumpGone() == true, "the flush hook ran before the walk")
        #expect(kernel.draining && kernel.shutdownComplete)
    }
}

// MARK: - the depth-first hook walk (test_graceful_shutdown.py)

@Suite("Lifecycle: Kernel.shutdown walk", .serialized)
struct GracefulShutdownTests {
    private func recorder(_ log: Log, _ tag: String) -> @Sendable (AgentId, Kernel) async throws -> Void {
        { id, _ in log.append("\(tag):\(id.value)") }
    }

    /// Parent's hook fires AFTER all descendants' hooks — same order contract
    /// as cascade-delete.
    @Test func shutdownWalksDepthFirst() async throws {
        let log = Log()
        let kernel = makeKernel([
            ("test_shutdown_p", HookProbe(name: "p", log: log, shutdownHook: recorder(log, "p"), deleteHook: nil)),
            ("test_shutdown_c1", HookProbe(name: "c1", log: log, shutdownHook: recorder(log, "c1"), deleteHook: nil)),
            ("test_shutdown_c2", HookProbe(name: "c2", log: log, shutdownHook: recorder(log, "c2"), deleteHook: nil)),
        ])
        _ = await create(kernel, "P", "test_shutdown_p")
        _ = await create(kernel, "C1", "test_shutdown_c1", parent: "P")
        _ = await create(kernel, "C2", "test_shutdown_c2", parent: "P")
        await kernel.shutdown()
        let order = log.all()
        let p = order.firstIndex(of: "p:P") ?? -1
        #expect((order.firstIndex(of: "c1:C1") ?? 99) < p, "\(order)")
        #expect((order.firstIndex(of: "c2:C2") ?? 99) < p, "\(order)")
    }

    /// Shutdown is the OPPOSITE of cascade-delete: hooks run, tree stays.
    @Test func shutdownDoesNotTouchRecordsOrTree() async throws {
        let log = Log()
        let kernel = makeKernel([
            ("test_shutdown_keep", HookProbe(name: "keep", log: log, shutdownHook: recorder(log, "k"), deleteHook: nil))
        ])
        _ = await create(kernel, "K", "test_shutdown_keep")
        _ = await create(kernel, "K_child", "test_shutdown_keep", parent: "K")
        await kernel.shutdown()
        #expect(Set(log.all()) == ["k:K", "k:K_child"])
        #expect(kernel.agent(AgentId("K")) != nil)
        #expect(kernel.agent(AgentId("K_child")) != nil)
        #expect(kernel.root?.childIds().contains(AgentId("K")) == true)
        #expect(kernel.agent(AgentId("K"))?.childIds().contains(AgentId("K_child")) == true)
    }

    /// Bundles that only define `onDelete` get their teardown invoked
    /// unchanged — the protocol default of `onShutdown` IS `onDelete`.
    @Test func shutdownFallsBackToOnDelete() async throws {
        let log = Log()
        let kernel = makeKernel([
            ("test_shutdown_only_od", HookProbe(name: "od", log: log, shutdownHook: nil, deleteHook: recorder(log, "on_delete")))
        ])
        _ = await create(kernel, "OD", "test_shutdown_only_od")
        await kernel.shutdown()
        #expect(log.all() == ["on_delete:OD"])
    }

    /// If a bundle defines BOTH hooks, shutdown calls `onShutdown`; `onDelete`
    /// stays reserved for cascade-delete.
    @Test func shutdownPrefersOnShutdownOverOnDelete() async throws {
        let log = Log()
        let kernel = makeKernel([
            ("test_shutdown_both",
             HookProbe(name: "both", log: log, shutdownHook: recorder(log, "on_shutdown"), deleteHook: recorder(log, "on_delete")))
        ])
        _ = await create(kernel, "B", "test_shutdown_both")
        await kernel.shutdown()
        #expect(log.all() == ["on_shutdown:B"])
    }

    /// Signal handler + a safety net can both reach `shutdown()`; the second
    /// call is a no-op so bundles aren't double-torn-down.
    @Test func shutdownIsIdempotent() async throws {
        let log = Log()
        let kernel = makeKernel([
            ("test_shutdown_idem", HookProbe(name: "idem", log: log, shutdownHook: recorder(log, "i"), deleteHook: nil))
        ])
        _ = await create(kernel, "I", "test_shutdown_idem")
        await kernel.shutdown()
        await kernel.shutdown()
        #expect(log.all() == ["i:I"])
        #expect(kernel.shutdownComplete)
    }

    /// One bundle's teardown can't take down the whole shutdown.
    @Test func shutdownContinuesOnHookException() async throws {
        struct Boom: Error {}
        let log = Log()
        let kernel = makeKernel([
            ("test_shutdown_good", HookProbe(name: "good", log: log, shutdownHook: recorder(log, "ok"), deleteHook: nil)),
            ("test_shutdown_bad",
             HookProbe(
                name: "bad", log: log,
                shutdownHook: { id, _ in
                    log.append("raised:\(id.value)")
                    throw Boom()
                }, deleteHook: nil)),
        ])
        _ = await create(kernel, "A", "test_shutdown_good")
        _ = await create(kernel, "B", "test_shutdown_bad")
        _ = await create(kernel, "C", "test_shutdown_good")
        await kernel.shutdown()
        let fired = log.all()
        #expect(fired.contains("ok:A") && fired.contains("raised:B") && fired.contains("ok:C"), "\(fired)")
    }
}

// MARK: - a dying task registers no new work (test_timeout_cancel_parity.py)

@Suite("Lifecycle: a killed task's sends", .serialized)
struct KilledTaskSendTests {
    /// Drive the guard directly: mark the current task as `cancelTask` does,
    /// then send. The item must settle `cancelled` and reach no handler.
    @Test func aSendFromAKilledTaskDoesNotDispatch() async throws {
        let log = Log()
        let kernel = makeKernel([("fork_probe.tools", ForkProbe(log: log))])
        let victim = await create(kernel, "tc_victim", "fork_probe.tools")
        let box = OutcomeBox()
        let probe = kernel.spawnTask(agent: AgentId("core"), verb: "probe") {
            // A live, REGISTERED task whose handle we mark exactly as
            // `cancelTask` does before it aborts anything.
            KernelTaskLocals.currentKilled?.mark()
            let out = await kernel.send([
                Item(victim, .object(["type": .string("note"), "text": .string("x")]))
            ])[0]
            box.set(out)
        }
        try await waitUntil("the probe to settle") { box.get() != nil }
        let out = box.get()!
        #expect(out.status == .cancelled, "\(out.result.serialize())")
        #expect(out.result == .object(["cancelled": .bool(true)]))
        #expect(out.pid == nil)
        #expect(log.all().isEmpty, "the handler must never run for a killed sender")
        #expect(!kernel.tasks.contains(probe))
    }

    /// The guard must not fire for an ordinary in-flight task.
    @Test func aLiveTaskStillSendsNormally() async throws {
        let log = Log()
        let kernel = makeKernel([("fork_probe.tools", ForkProbe(log: log))])
        let victim = await create(kernel, "tc_live", "fork_probe.tools")
        let box = OutcomeBox()
        _ = kernel.spawnTask(agent: AgentId("core"), verb: "probe") {
            let out = await kernel.send([
                Item(victim, .object(["type": .string("note"), "text": .string("y")]))
            ])[0]
            box.set(out)
        }
        try await waitUntil("the probe to settle") { box.get() != nil }
        let out = box.get()!
        #expect(out.status != .cancelled, "\(out.result.serialize())")
        #expect(log.all() == ["note:y"])
    }
}

/// One-slot carrier for an outcome produced inside a spawned task.
private final class OutcomeBox: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Outcome?
    func set(_ o: Outcome) {
        lock.lock()
        value = o
        lock.unlock()
    }
    func get() -> Outcome? {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

// MARK: - a fork's work is parented to the fork (test_fork_parentage.py)

@Suite("Lifecycle: fork parentage", .serialized)
struct ForkParentageTests {
    /// `fork` dispatches, spawns the job, returns. That dispatch is FINISHED —
    /// its row is gone. The job's later send must hang from the JOB's task,
    /// never from the settled `fork` pid, and never become a root.
    @Test func aProgramsSendIsParentedToTheJobNotADeadDispatch() async throws {
        let log = Log()
        let kernel = makeKernel([("fork_probe.tools", ForkProbe(log: log))])
        let mailbox = await create(kernel, "mailbox", "fork_probe.tools", callableBy: .object(["runner": .string("*")]))
        let runner = await create(kernel, "runner", "fork_probe.tools", canCall: .object(["mailbox": .string("*")]))
        let start = await kernel.send([
            Item(runner, .object(["type": .string("fork"), "peer": .string(mailbox.value)]))
        ])[0]
        let startPid = start.pid
        #expect(startPid != nil)
        let jobId = start.result["job_id"].asInt
        #expect(jobId != nil, "\(start.result.serialize())")
        try await waitUntil("the program to send") { log.all() == ["note:from the program"] }
        try await Task.sleep(nanoseconds: 100_000_000)  // let the row settle

        // THE ROW THE PROGRAM'S SEND WROTE.
        let rows = kernel.rows.forAgent(mailbox).filter {
            $0["kind"]?.asString == "call" && $0["verb"]?.asString == "note"
        }
        #expect(!rows.isEmpty, "no row for the program's send")
        let parent = rows.first?["parent"]?.asInt
        // The dispatch that forked the job is LONG GONE...
        #expect(!kernel.tasks.contains(startPid!), "the fork dispatch is still in flight")
        // ...so it must not be what the program's work hangs from.
        #expect(parent != startPid?.value, "the program's send is parented to the settled `fork` dispatch")
        #expect(parent != nil, "the program's send became a ROOT: unreachable by cancel")
        #expect(parent == jobId, "parent \(String(describing: parent)) != job \(String(describing: jobId))")
    }
}
