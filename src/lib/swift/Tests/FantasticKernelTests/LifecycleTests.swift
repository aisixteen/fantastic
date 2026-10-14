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

import FantasticKernelState

@testable import FantasticKernel
import FantasticRamStore

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
    /// The STATIC table `shutdown_agents` reads — python's module `VERBS`.
    let verbs = ["reflect", "shutdown", "slow", "work"]

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
            // The shutdown pass may still use its tools: a nested send passes
            // the drain.
            let r = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[
                0]
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

/// Python's `quiet_probe.tools`: `VERBS = {reflect, work}`. It ACCEPTS
/// `shutdown` as a lifecycle no-op but does not DECLARE it, so python never
/// tells it — and never reads it to find out. Records every verb delivered.
private struct QuietProbe: AgentBundle {
    let name = "quiet_probe"
    let log: Log
    let verbs = ["reflect", "work"]

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        log.append("\(agentId.value):\(payload["type"].asString ?? "")")
        switch payload["type"].asString ?? "" {
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": .string("quiet probe"),
                "verbs": .object(["reflect": .string("identity"), "work": .string("a verb")]),
            ])
        default:
            return .null
        }
    }
}

/// Every call row the kernel holds, oldest first — the ledger a reader sees.
private func allRows(_ kernel: Kernel) -> [Row] {
    kernel.rows.all()
}

/// A bundle whose `boot` does what the payload's `boot_mode` meta asks: RAISE,
/// answer `{error}`, or succeed. The create_agent line branches on the OUTCOME
/// STATUS, so all three must be reachable.
private struct BootProbe: AgentBundle {
    let name = "bootprobe"
    struct Boom: Error, CustomStringConvertible {
        var description: String { "Boom: the handler raised" }
    }
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard payload["type"].asString == "boot" else { return .null }
        switch kernel.agent(agentId)?.metaValue(forKey: "boot_mode")?.asString {
        case "raise": throw Boom()
        case "error": return .object(["error": .string("a refusal, not a bug")])
        default: return .null
        }
    }
}

/// Python's `_stub_bundle`: a handler that answers nothing, carrying whatever
/// hooks the test wants to observe. `shutdownHook == nil` is a bundle with no
/// shutdown teardown of its own.
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
        try await shutdownHook?(agentId, kernel)
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
                    Item(
                        peer,
                        .object(["type": .string("note"), "text": .string("from the program")]))
                ])
            }
            return .object(["job_id": .integer(job.value), "status": .string("running")])
        case let other:
            return .object(["error": .string("unknown '\(other)'"), "reason": .string("unhandled")])
        }
    }
}

// MARK: - harness

private func makeKernel(
    _ bundles: [(String, any AgentBundle)], rootHandler: String? = nil
) -> Kernel {
    let registry = BundleRegistry()
    for (module, b) in bundles { registry.register(module, b) }
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: AgentId(ROOT_ID), handlerModule: rootHandler, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

private func create(
    _ kernel: Kernel, _ id: String, _ module: String, parent: String = "kernel_state",
    canCall: JSON = .null, callableBy: JSON = .null,
    meta: OrderedDictionary<String, JSON> = [:]
) async -> AgentId {
    var payload: OrderedDictionary<String, JSON> = [
        "type": .string("create_agent"),
        "parent_id": .string(parent),
        "handler_module": .string(module),
        "id": .string(id),
    ]
    if case .object = canCall { payload["can_call"] = canCall }
    if case .object = callableBy { payload["callable_by"] = callableBy }
    for (k, v) in meta { payload[k] = v }
    let r = await kernel.send([Item(AgentId("kernel_state"), .object(payload))])[0].result
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
        #expect(
            out.status == .failed && out.result["reason"].asString == "draining",
            "\(out.result.serialize())")
        #expect(
            out.result["error"].asString
                == "send: the kernel is draining for shutdown — no new work is accepted; in-flight work may finish"
        )
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
        let a = await create(
            kernel, "sp_a", "shut_probe.tools", canCall: .object(["$parent": .string("*")]))
        let b = await create(
            kernel, "sp_b", "shut_probe.tools", canCall: .object(["$parent": .string("*")]))
        kernel.quietTimeout = 0.3
        let slow = Task { await send(kernel, b.value, "slow") }
        try await waitUntil("the slow handler to start") { log.has("running:sp_b") }
        // A keepalive loop (a persister) survives the quiet phase. THE KERNEL
        // CREATES THE TASK — only it can mint the id the fork runs under.
        let keep = kernel.spawnTask(
            agent: AgentId("kernel_state"), verb: "message_pump", keepalive: true
        ) {
            try? await Task.sleep(nanoseconds: 30_000_000_000)
        }
        let out = await send(kernel, "kernel", "shutdown_agents").result
        #expect(out["drained"].asBool == true, "\(out.serialize())")
        #expect(
            (out["shut"].asArray ?? []).compactMap { $0.asString }.sorted() == [a.value, b.value],
            "\(out.serialize())")
        #expect(Set(log.all()) == [a.value, b.value], "every advertiser was told")
        #expect(
            !(out["cancelled"].asArray ?? []).isEmpty,
            "the slow task was cancelled after the quiet bound")
        #expect(kernel.tasks.contains(keep), "the keepalive loop is not pending work")
        let settled = await slow.value
        #expect(settled.status == .cancelled, "\(settled.result.serialize())")
        // The kernel keeps serving reads; a new root is refused.
        let r = await send(kernel, a.value, "work")
        #expect(r.result["reason"].asString == "draining")
        let again = await send(kernel, "kernel", "shutdown_agents").result
        #expect(
            again["drained"].asBool == true && again["cancelled"] == .array([]),
            "\(again.serialize())")
        _ = await kernel.cancelTask(keep)
        kernel.setDraining(false)
    }

    /// A `spawnTask` FORK is a SERVICE LOOP — scheduler tick, bridge read loop,
    /// ws drain, job pump, pty reader — and never settles by construction, so
    /// the quiet phase must not wait for one: python's `settling()` drops
    /// `_forked` entries. It is still CANCELLED, and a real in-flight
    /// DISPATCH still holds the quiet phase.
    @Test func aForkDoesNotHoldTheQuietPhaseButIsStillCancelled() async throws {
        let log = Log()
        let kernel = makeKernel([("shut_probe.tools", ShutProbe(log: log))])
        _ = await create(
            kernel, "sp_f", "shut_probe.tools", canCall: .object(["$parent": .string("*")]))
        kernel.quietTimeout = 3.0
        let fork = kernel.spawnTask(agent: AgentId("kernel_state"), verb: "ws_drain") {
            try? await Task.sleep(nanoseconds: 30_000_000_000)
        }
        let t0 = Date()
        let out = await send(kernel, "kernel", "shutdown_agents").result
        let waited = Date().timeIntervalSince(t0)
        #expect(waited < 1.0, "a fork held the quiet phase for \(waited)s")
        #expect(
            (out["cancelled"].asArray ?? []).compactMap { $0.asInt }.contains(fork.value),
            "the fork was not cancelled: \(out.serialize())")
        kernel.setDraining(false)

        // The CONTRAST: an in-flight dispatch is not a fork, so it burns the
        // whole bound and is cancelled at the end of it.
        let k2 = makeKernel([("shut_probe.tools", ShutProbe(log: log))])
        let b = await create(
            k2, "sp_g", "shut_probe.tools", canCall: .object(["$parent": .string("*")]))
        k2.quietTimeout = 0.5
        let slow = Task { await send(k2, b.value, "slow") }
        try await waitUntil("the slow handler to start") { log.has("running:sp_g") }
        let t1 = Date()
        let out2 = await send(k2, "kernel", "shutdown_agents").result
        #expect(
            Date().timeIntervalSince(t1) >= 0.5,
            "a live dispatch did not hold the quiet phase")
        #expect(!(out2["cancelled"].asArray ?? []).isEmpty, "\(out2.serialize())")
        _ = await slow.value
        k2.setDraining(false)
    }

    /// EVERY system verb on a non-root answers the SAME generic
    /// `not_here` — `shutdown_agents`/`kernel_stop` carry no special
    /// case (python `_agent.py:2036-2044` generalises the old kernel_stop-
    /// only refusal to all seven system verbs).
    /// `shutdown_agents` READS THE BUNDLE'S STATIC VERBS TABLE — python's
    /// `advertises("shutdown")` imports the module and looks at `VERBS`,
    /// sending nothing. A tree of N agents therefore costs NO reflect call row,
    /// no task row and no watcher echo.
    @Test func shutdownAgentsReadsTheStaticTableWithoutASingleSend() async throws {
        let quiet = Log()
        let kernel = makeKernel([
            ("shut_probe.tools", ShutProbe(log: Log())),
            ("quiet_probe.tools", QuietProbe(log: quiet)),
        ])
        for i in 0..<5 { _ = await create(kernel, "q\(i)", "quiet_probe.tools") }
        // `ps` on the root: an explicit edge on both ends.
        let a = await create(kernel, "sp_a", "shut_probe.tools")
        wire(kernel, "sp_a", "kernel_state", ["ps"])
        kernel.quietTimeout = 0.3
        let before = allRows(kernel).count
        let out = await send(kernel, "kernel", "shutdown_agents").result
        #expect(
            (out["shut"].asArray ?? []).compactMap { $0.asString } == [a.value],
            "\(out.serialize())")
        // `boot` is the create line's, not the shutdown's.
        let delivered = quiet.all().filter { !$0.hasSuffix(":boot") }
        #expect(delivered.isEmpty, "no verb reached a non-advertiser: \(delivered)")
        let rows = allRows(kernel)
        let seenVerbs = rows.compactMap { $0["verb"]?.asString }
        #expect(!seenVerbs.contains("reflect"), "not one reflect row: \(seenVerbs)")
        // The ONLY rows the verb writes are its own dispatch and the nested `ps`
        // the advertiser's handler makes: the `shutdown` itself is the kernel's
        // own call into that handler, not a send (`runHandler`).
        let wrote = rows.suffix(rows.count - before).compactMap { $0["verb"]?.asString }
        #expect(wrote.sorted() == ["ps", "shutdown_agents"], "\(wrote)")
        kernel.setDraining(false)
    }

    @Test func shutdownAgentsIsRootOnly() async throws {
        let log = Log()
        let kernel = makeKernel([("shut_probe.tools", ShutProbe(log: log))])
        let a = await create(kernel, "sp_c", "shut_probe.tools")
        let r = await send(kernel, a.value, "shutdown_agents").result
        #expect(
            r["error"].asString == "shutdown_agents is a kernel verb — sp_c does not answer it",
            "\(r.serialize())")
        #expect(r["reason"].asString == "not_here")
        #expect(
            r["hint"].asString
                == "address the kernel root ('kernel'); name the agent you mean in "
                + "`parent_id` / `id`")
        #expect(r["verb"].asString == "shutdown_agents")
        let k = await send(kernel, a.value, "kernel_stop").result
        #expect(
            k["error"].asString == "kernel_stop is a kernel verb — sp_c does not answer it",
            "\(k.serialize())")
        #expect(k["reason"].asString == "not_here")
        #expect(k["verb"].asString == "kernel_stop")
    }

    @Test func kernelStopAcksThenRequestsTheStop() async throws {
        let kernel = makeKernel([])
        let r = await send(kernel, "kernel", "kernel_stop").result
        #expect(
            r == .object(["type": .string("kernel_stop"), "ok": .bool(true)]),
            "\(r.serialize())")
        // The trigger is deferred so the ack flushes first; the waiter wakes.
        let woke = Task {
            await kernel.shutdownRequested()
            return true
        }
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

    @Test func kernelShutdownLandsTheLastExecutionRowsBeforeTheWalk() async throws {
        // THE FINAL FLUSH RUNS BEFORE THE WALK, while the memory store is alive:
        // by the time the walk reaches an agent, every execution row minted
        // before the stop is in the store — RAM holds only the final flush's own
        // call and settle (already recorded by its flush row). Python
        // `Kernel.shutdown`.
        let log = Log()
        let kernel = makeKernel(
            [
                (ROOT_HANDLER_MODULE, KernelStateBundle()),
                ("ram_store.tools", RamStoreBundle()),
                (
                    "walk.tools",
                    HookProbe(
                        name: "walk", log: log,
                        shutdownHook: { _, k in
                            log.append("walk")
                            let unflushed = k.rows.all().filter {
                                $0["kind"]?.asString == "call"
                                    && $0["agent"]?.asString != "memory"
                            }
                            log.notePump(gone: unflushed.isEmpty)
                        }, deleteHook: nil)
                ),
            ], rootHandler: ROOT_HANDLER_MODULE)
        _ = await create(kernel, "w", "walk.tools")
        _ = await create(
            kernel, "memory", "ram_store.tools")
        kernel.update(AgentId(ROOT_ID), ["memory_store_id": .string("memory")])
        let started = await kernel.start(bootAll: false)
        #expect(started["started"].asBool == true, "\(started.serialize())")
        await kernel.shutdown()
        #expect(log.all() == ["walk"], "\(log.all())")
        #expect(log.pumpGone() == true, "the final flush ran before the walk")
        #expect(kernel.draining && kernel.shutdownComplete)
    }

    /// The create_agent boot line branches on the OUTCOME STATUS: a boot
    /// that RAISED (`failed`) prints python's stdout line; one that answered
    /// `{error}` (`errored`) or succeeded prints nothing. Swift discarded the
    /// boot reply entirely, so a raised child boot was invisible.
    @Test func testRaisedChildBootPrintsPythonLine() async throws {
        let kernel = makeKernel([("bootprobe.tools", BootProbe())])
        let raised = await RuntimeLog.captureOut {
            _ = await create(
                kernel, "boom", "bootprobe.tools", meta: ["boot_mode": .string("raise")])
        }
        // `{boot['result']}` in python's f-string is `str(<dict>)` — the WHOLE
        // reply body, python-repr'd, not just its `error` text.
        #expect(
            raised == [
                "  [create_agent] boom boot failed: "
                    + "{'error': 'Boom: the handler raised', 'raised': True}"
            ], "\(raised)")
        // An `errored` boot is a refusal the create does not undo and does not
        // narrate; a clean one says nothing either.
        let errored = await RuntimeLog.captureOut {
            _ = await create(
                kernel, "refuses", "bootprobe.tools", meta: ["boot_mode": .string("error")])
            _ = await create(kernel, "fine", "bootprobe.tools")
        }
        #expect(errored.isEmpty, "\(errored)")
    }

    @Test func theRootLoaderBootMintsTheGuidAfterItsGrants() async throws {
        // The guid is minted ONCE, by the root loader's `boot`, AFTER the
        // record writer subscribes — so the `updated` event it publishes is what
        // carries the new record to disk. Meta order is grants, then guid.
        let kernel = makeKernel(
            [(ROOT_HANDLER_MODULE, KernelStateBundle())], rootHandler: ROOT_HANDLER_MODULE)
        #expect(kernel.root?.metaValue(forKey: "guid") == nil, "a virgin root has none")
        _ = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("boot")]))])[0]
        let guid = try #require(kernel.root?.metaValue(forKey: "guid")?.asString)
        #expect(guid.count == 32, "\(guid)")
        #expect(
            Array(kernel.root!.meta.keys) == ["can_call", "callable_by", "guid"],
            "\(Array(kernel.root!.meta.keys))")
        // Idempotent: a second boot neither re-mints nor re-installs.
        _ = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("boot")]))])[0]
        #expect(kernel.root?.metaValue(forKey: "guid")?.asString == guid)
    }
}

// MARK: - the depth-first hook walk (test_graceful_shutdown.py)

@Suite("Lifecycle: Kernel.shutdown walk", .serialized)
struct GracefulShutdownTests {
    private func recorder(_ log: Log, _ tag: String)
        -> @Sendable (AgentId, Kernel) async throws -> Void
    {
        { id, _ in log.append("\(tag):\(id.value)") }
    }

    /// Parent's hook fires AFTER all descendants' hooks — same order contract
    /// as cascade-delete.
    @Test func shutdownWalksDepthFirst() async throws {
        let log = Log()
        let kernel = makeKernel([
            (
                "test_shutdown_p",
                HookProbe(name: "p", log: log, shutdownHook: recorder(log, "p"), deleteHook: nil)
            ),
            (
                "test_shutdown_c1",
                HookProbe(name: "c1", log: log, shutdownHook: recorder(log, "c1"), deleteHook: nil)
            ),
            (
                "test_shutdown_c2",
                HookProbe(name: "c2", log: log, shutdownHook: recorder(log, "c2"), deleteHook: nil)
            ),
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
            (
                "test_shutdown_keep",
                HookProbe(name: "keep", log: log, shutdownHook: recorder(log, "k"), deleteHook: nil)
            )
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

    /// Shutdown calls `onShutdown` and only that. A
    /// bundle with only `onDelete` has nothing to free at shutdown, and delete's
    /// teardown is not run in its place.
    @Test func shutdownNeverFallsBackToOnDelete() async throws {
        let log = Log()
        let kernel = makeKernel([
            (
                "test_shutdown_only_od",
                HookProbe(
                    name: "od", log: log, shutdownHook: nil, deleteHook: recorder(log, "on_delete"))
            )
        ])
        _ = await create(kernel, "OD", "test_shutdown_only_od")
        await kernel.shutdown()
        #expect(log.all() == [])
    }

    /// If a bundle defines BOTH hooks, shutdown calls `onShutdown`; `onDelete`
    /// stays reserved for cascade-delete.
    @Test func shutdownPrefersOnShutdownOverOnDelete() async throws {
        let log = Log()
        let kernel = makeKernel([
            (
                "test_shutdown_both",
                HookProbe(
                    name: "both", log: log, shutdownHook: recorder(log, "on_shutdown"),
                    deleteHook: recorder(log, "on_delete"))
            )
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
            (
                "test_shutdown_idem",
                HookProbe(name: "idem", log: log, shutdownHook: recorder(log, "i"), deleteHook: nil)
            )
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
            (
                "test_shutdown_good",
                HookProbe(
                    name: "good", log: log, shutdownHook: recorder(log, "ok"), deleteHook: nil)
            ),
            (
                "test_shutdown_bad",
                HookProbe(
                    name: "bad", log: log,
                    shutdownHook: { id, _ in
                        log.append("raised:\(id.value)")
                        throw Boom()
                    }, deleteHook: nil)
            ),
        ])
        _ = await create(kernel, "A", "test_shutdown_good")
        _ = await create(kernel, "B", "test_shutdown_bad")
        _ = await create(kernel, "C", "test_shutdown_good")
        await kernel.shutdown()
        let fired = log.all()
        #expect(
            fired.contains("ok:A") && fired.contains("raised:B") && fired.contains("ok:C"),
            "\(fired)")
    }

    /// `flush_pending` raising prints python's stderr line — bare
    /// `str(e)`, no agent id (python calls the hook on the root alone;
    /// swift's own broader "every agent is offered it" design keeps the
    /// text id-less all the same, matching python's literal spelling).
    ///
    /// Documented, not reproduced: python's recursive
    /// `await child.shutdown()` catch (`  [shutdown] <cid> raised: <e>`) has
    /// no swift counterpart — `shutdownWalk` is non-throwing, so nothing
    /// escapes the recursive call to catch further up.
    @Test func flushPendingFailurePrintsPythonLine() async throws {
        struct Boom: Error, CustomStringConvertible {
            var description: String { "disk full" }
        }
        struct FlushRaises: AgentBundle {
            let name = "flushraises"
            func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
                .null
            }
            func flushPending(agentId: AgentId, kernel: Kernel) async throws { throw Boom() }
        }
        let kernel = makeKernel(
            [("test_flush_raises", FlushRaises())], rootHandler: "test_flush_raises")
        let lines = await RuntimeLog.capture {
            await kernel.shutdown()
        }
        #expect(
            lines.contains { $0.contains("[shutdown] flush_pending raised: disk full") }, "\(lines)"
        )
    }
}

/// The cascade and the shutdown walk SAY what failed and go on, as python's
/// `Agent.on_delete` (stdout) and `Agent.shutdown` (stderr) print it.
@Suite("Lifecycle: hook failures are said")
struct HookFailureLineTests {
    @Test func aRaisingOnDeletePrintsTheCascadeLine() async throws {
        struct Boom: Error, CustomStringConvertible { var description: String { "socket gone" } }
        struct RaisingDelete: AgentBundle {
            let name = "raisingdelete"
            func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
                .null
            }
            func onDelete(agentId: AgentId, kernel: Kernel) async throws { throw Boom() }
        }
        let kernel = makeKernel([("raisingdelete.tools", RaisingDelete())])
        _ = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "raisingdelete.tools", "id": "rd",
                ])
        ])[0]
        let lines = await RuntimeLog.captureOut {
            let r = await kernel.send([
                Item("kernel_state", ["type": "delete_agent", "id": "rd"])
            ])[0]
            #expect(r.result["deleted"].asBool == true, "\(r.result)")
        }
        #expect(
            lines.contains { $0.contains("  [cascade] rd bundle on_delete raised: socket gone") },
            "\(lines)")
    }

    /// An agent created at runtime with a module this runtime does not have is
    /// python's failed import at shutdown: one stderr line, and the walk goes on.
    @Test func anUnimportableModulePrintsTheShutdownImportLine() async throws {
        let kernel = makeKernel()
        let made = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "absent_bundle.tools", "id": "absent",
                ])
        ])[0]
        #expect(made.result["id"].asString == "absent", "\(made.result)")
        let lines = await RuntimeLog.capture { await kernel.shutdown() }
        #expect(
            lines.contains(
                "  [shutdown] absent import 'absent_bundle.tools' raised: "
                    + "No module named 'absent_bundle'\n"), "\(lines)")
    }

    /// Python's `ModuleNotFoundError` text for a name outside its catalog
    /// (`import_handler`): nothing is imported, the
    /// TOP-LEVEL package is named — under an installed package too, and for
    /// the bare package; only a registered name resolves (`nil`).
    @Test func moduleNotFoundIsPythonsCatalogText() {
        let registry = BundleRegistry()
        registry.register("file_bridge.tools", EchoBundle())
        registry.registerUnlisted("probe.tools", EchoBundle())
        #expect(registry.moduleNotFound("absent_bundle.tools") == "No module named 'absent_bundle'")
        #expect(registry.moduleNotFound("absent_bundle") == "No module named 'absent_bundle'")
        #expect(registry.moduleNotFound("file_bridge.nope") == "No module named 'file_bridge'")
        #expect(
            registry.moduleNotFound("file_bridge.tools.sub") == "No module named 'file_bridge'")
        #expect(registry.moduleNotFound("file_bridge") == "No module named 'file_bridge'")
        #expect(registry.moduleNotFound("file_bridge.tools") == nil)
        #expect(registry.moduleNotFound("probe.tools") == nil)
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
        let probe = kernel.spawnTask(agent: AgentId("kernel_state"), verb: "probe") {
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
        _ = kernel.spawnTask(agent: AgentId("kernel_state"), verb: "probe") {
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
        let mailbox = await create(
            kernel, "mailbox", "fork_probe.tools", callableBy: .object(["runner": .string("*")]))
        let runner = await create(
            kernel, "runner", "fork_probe.tools", canCall: .object(["mailbox": .string("*")]))
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
        let rows = kernel.rows.all().filter { $0["agent"]?.asString == mailbox.value }.filter {
            $0["kind"]?.asString == "call" && $0["verb"]?.asString == "note"
        }
        #expect(!rows.isEmpty, "no row for the program's send")
        let parent = rows.first?["parent"]?.asInt
        // The dispatch that forked the job is LONG GONE...
        #expect(!kernel.tasks.contains(startPid!), "the fork dispatch is still in flight")
        // ...so it must not be what the program's work hangs from.
        #expect(
            parent != startPid?.value,
            "the program's send is parented to the settled `fork` dispatch")
        #expect(parent != nil, "the program's send became a ROOT: unreachable by cancel")
        #expect(
            parent == jobId,
            "parent \(String(describing: parent)) != job \(String(describing: jobId))")
    }
}
