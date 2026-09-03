// The TASK TABLE — `ps` + `cancel`, the kernel's process table.
//
// Mirrors Python's `tests/test_task_table.py` (the canonical contract) and
// Rust's `fantastic-kernel/src/tasks/tests.rs`.
//
// The unit is a TASK (ONE in-flight dispatch), not an agent: `"<agent>:<n>"` is
// a pid, and tasks form a TREE through the send chain (a `send` issued inside a
// handler is a CHILD of that handler's task). `ps` reads the live table;
// `cancel` kills a task AND ITS SUBTREE, like killing a process group.
//
// Both are SYSTEM verbs — baked into the substrate, answered natively by every
// agent, in no bundle's verb map. So an AGENT can cancel another agent's work
// (`agentCancelsAnotherAgentsTask`) — that is the point: cancellation is a
// property of the one primitive, not a UI feature bolted onto one bundle.

import FantasticJSON
import Foundation
import OrderedCollections
import Testing

@testable import FantasticKernel

// MARK: - a test bundle: a handler that STAYS in flight + the onCancel hook

/// What the test bundle records. `held` is a stand-in for a REAL resource (an OS
/// subprocess, a PTY): the slow handler takes it and only `onCancel` gives it
/// back — cancelling the task would leak it, which is the whole point of the
/// hook.
private final class Shared: @unchecked Sendable {
    private let lock = NSLock()
    /// (agent, task_id, task_still_in_the_table_when_the_hook_ran)
    private var hooks: [(String, String, Bool)] = []
    /// Resources currently held, keyed by the task holding them.
    private var held: Set<String> = []

    func hookLog() -> [(String, String, Bool)] {
        lock.lock()
        defer { lock.unlock() }
        return hooks
    }

    func heldIds() -> Set<String> {
        lock.lock()
        defer { lock.unlock() }
        return held
    }

    func take(_ taskId: String) {
        lock.lock()
        held.insert(taskId)
        lock.unlock()
    }

    func recordHook(agent: String, task: String, alive: Bool) {
        lock.lock()
        hooks.append((agent, task, alive))
        held.remove(task)
        lock.unlock()
    }
}

private struct SlowBundle: AgentBundle {
    let name = "slow"
    let sh: Shared

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown":
            return .null
        case "reflect":
            return .object([
                "id": .string(agentId.value), "sentence": .string("slow test agent"),
                "verbs": .object([:]),
            ])
        // In flight until someone kills us (or the test times it out). Takes the
        // "resource" for the duration — released ONLY by `onCancel`.
        case "slow":
            if let t = KernelTaskLocals.currentTask { sh.take(t.value) }
            try? await Task.sleep(nanoseconds: 30_000_000_000)
            return .object(["finished": .bool(true)])  // never reached in these tests
        // A nested send — the callee's dispatch becomes a CHILD task of this one.
        case "chain":
            let next = AgentId(payload["next"].asString ?? "")
            return await kernel.send([Item(next, .object(["type": .string("slow")]))])[0].result
        // Work that OUTLIVES this dispatch: registered in the table, so `ps` sees
        // it and `cancel` can still reach it once the dispatch is gone.
        case "fork":
            let sh = self.sh
            let tid = kernel.spawnTask(agent: agentId, verb: "job") {
                if let t = KernelTaskLocals.currentTask { sh.take(t.value) }
                try? await Task.sleep(nanoseconds: 30_000_000_000)
            }
            return .object(["task_id": .string(tid.value)])
        // Fork long-lived work on a PEER, then stay in flight. The fork outlives
        // the `fork` dispatch and is re-parented to THIS task.
        case "chain_fork":
            let next = AgentId(payload["next"].asString ?? "")
            let r = await kernel.send([Item(next, .object(["type": .string("fork")]))])[0].result
            try? await Task.sleep(nanoseconds: 30_000_000_000)
            return r
        // An AGENT cancelling another agent's work: discover via `ps`, then
        // `cancel`. Plain verbs — no privileged surface, no bundle coupling.
        case "kill":
            let victim = payload["victim"].asString ?? ""
            let table = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[0].result
            let found = (table["tasks"].asArray ?? []).first {
                $0["agent"].asString == victim && $0["verb"].asString == "slow"
            }
            guard let taskId = found?["id"].asString else {
                return .object(["error": .string("no victim task")])
            }
            return await kernel.send([Item(AgentId("kernel"), .object(["type": .string("cancel"), "task_id": .string(taskId)]))])[0].result
        case let other:
            return .object(["error": .string("unknown verb \(other)")])
        }
    }

    /// The optional bundle hook — the SIGTERM half. Records that it ran BEFORE
    /// the abort (the task is still in the table here), which is the whole
    /// contract: a bundle gets to release real resources while its handler still
    /// exists.
    func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
        sh.recordHook(
            agent: agentId.value, task: taskId.value,
            alive: kernel.tasks.contains(taskId))
    }
}

// MARK: - harness

private func makeSlowKernel(_ sh: Shared) -> Kernel {
    let registry = BundleRegistry()
    registry.register("slow.tools", SlowBundle(sh: sh))
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

private func makeAgent(
    _ kernel: Kernel, _ id: String, canCall: JSON = .null, callableBy: JSON = .null
) async -> AgentId {
    var payload: OrderedDictionary<String, JSON> = [
        "type": .string("create_agent"),
        "parent_id": .string("core"),
        "handler_module": .string("slow.tools"),
        "id": .string(id),
    ]
    // Two-sided deny-all: a `chain` send to a SIBLING needs BOTH the caller's
    // `can_call` (egress) and the callee's `callable_by` (ingress).
    if case .object = canCall { payload["can_call"] = canCall }
    if case .object = callableBy { payload["callable_by"] = callableBy }
    let r = await kernel.send([Item(AgentId("core"), .object(payload))])[0].result
    #expect(r["id"].asString == id, "create_agent failed: \(r.serialize())")
    return AgentId(id)
}

/// The `ps` reply rows — through the VERB, like any caller.
private func rows(_ kernel: Kernel) async -> [JSON] {
    let r = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[0].result
    return r["tasks"].asArray ?? []
}

private func rowFor(_ kernel: Kernel, _ agent: AgentId, _ verb: String) async -> JSON? {
    await rows(kernel).first {
        $0["agent"].asString == agent.value && $0["verb"].asString == verb
    }
}

private func cancel(_ kernel: Kernel, _ taskId: String) async -> JSON {
    await kernel.send([Item(AgentId("kernel"), .object(["type": .string("cancel"), "task_id": .string(taskId)]))])[0].result
}

/// Poll until `cond` holds, or fail the test.
private func waitUntil(_ what: String, _ cond: () async -> Bool) async throws {
    for _ in 0..<300 {
        if await cond() { return }
        try await Task.sleep(nanoseconds: 10_000_000)
    }
    Issue.record("timed out waiting for \(what)")
}

/// Wait until `agent`'s `verb` dispatch shows up in the table, then return its id.
private func waitForTask(_ kernel: Kernel, _ agent: AgentId, _ verb: String) async throws -> String
{
    for _ in 0..<300 {
        if let row = await rowFor(kernel, agent, verb), let id = row["id"].asString {
            return id
        }
        try await Task.sleep(nanoseconds: 10_000_000)
    }
    Issue.record("timed out waiting for \(agent)'s \(verb) task")
    throw CancellationError()
}

/// Fire a `send` that is expected to STAY in flight; returns a handle whose
/// `value` is the eventual reply.
private func spawnCaller(_ kernel: Kernel, _ target: AgentId, _ payload: JSON) -> Task<JSON, Never>
{
    Task { await kernel.send([Item(target, payload)])[0].result }
}

/// Await a blocked caller's reply, failing the test rather than hanging. The
/// timeout races INSIDE the group so the winner cancels the loser — a happy-path
/// reply costs nothing.
private func replyOf(_ caller: Task<JSON, Never>) async -> JSON {
    let outcome: JSON? = await withTaskGroup(of: JSON?.self) { group in
        group.addTask { await caller.value }
        group.addTask {
            try? await Task.sleep(nanoseconds: 5_000_000_000)
            return nil  // timed out — the caller never resolved
        }
        let first = await group.next() ?? nil
        group.cancelAll()
        return first
    }
    guard let reply = outcome else {
        Issue.record("the blocked caller never resolved")
        return .null
    }
    return reply
}

// MARK: - ps: the live table + the send TREE

@Suite("Task table — ps")
struct TaskTablePsTests {

    @Test func psShowsTheTreeOfANestedSend() async throws {
        // A `send` issued INSIDE a handler is a CHILD of that handler's task.
        // `ps` shows both rows with the parent link, so a caller sees who spawned
        // what.
        let sh = Shared()
        let kernel = makeSlowKernel(sh)
        let a = await makeAgent(kernel, "slow_a", canCall: ["slow_b": "*"])
        let b = await makeAgent(kernel, "slow_b", callableBy: ["slow_a": "*"])

        let caller = spawnCaller(
            kernel, a, .object(["type": .string("chain"), "next": .string("slow_b")]))
        defer { caller.cancel() }
        _ = try await waitForTask(kernel, b, "slow")

        let parent = try #require(await rowFor(kernel, a, "chain"))
        let child = try #require(await rowFor(kernel, b, "slow"))

        // Task id IS a pid: "<agent_id>:<8 hex>". The token is OPAQUE on purpose — a
        // visible counter reads as a SEQUENCE, and messaging is async (a send may return
        // once, several times, or never). Measured 2026-08-20 on python: a model inferred
        // ordering from the number and re-polled forever instead of matching on the id.
        for (row, agent) in [(parent, a), (child, b)] {
            let id = try #require(row["id"].asString)
            let parts = id.split(separator: ":")
            #expect(parts.count == 2)
            #expect(String(parts[0]) == agent.value)
            let token = String(parts[1])
            #expect(token.count == 8)
            #expect(token.allSatisfy { $0.isHexDigit && !$0.isUppercase })
        }
        // The tree: b's dispatch is a child of a's dispatch. a's caller is the
        // test task — not a dispatch — so a's task is a root (init-owned).
        #expect(child["parent"] == parent["id"])
        #expect(parent["parent"] == .null)
        // The five public fields, and nothing else (the stop handle stays behind).
        let keys = Set((parent.asObject ?? [:]).keys)
        #expect(keys == ["id", "agent", "parent", "verb", "started_at"])
        #expect(parent["started_at"].asDouble != nil)  // a float clock

        // `ps` shows its own row too (it IS a dispatch) — the table is honest.
        let sawItself = await rows(kernel).contains(where: { $0["verb"].asString == "ps" })
        #expect(sawItself)
    }
}

// MARK: - cancel: the subtree dies, the caller learns

@Suite("Task table — cancel")
struct TaskTableCancelTests {

    @Test func cancelKillsTheWholeSubtree() async throws {
        // Cancelling A's task kills A AND the B dispatch A is blocked on —
        // children first, like killing a process group. The blocked caller's
        // `send` RESOLVES to {"cancelled": true}; it never throws.
        let sh = Shared()
        let kernel = makeSlowKernel(sh)
        let a = await makeAgent(kernel, "slow_a", canCall: ["slow_b": "*"])
        let b = await makeAgent(kernel, "slow_b", callableBy: ["slow_a": "*"])

        let caller = spawnCaller(
            kernel, a, .object(["type": .string("chain"), "next": .string("slow_b")]))
        defer { caller.cancel() }
        let bTask = try await waitForTask(kernel, b, "slow")
        let aTask = try await waitForTask(kernel, a, "chain")

        let r = await cancel(kernel, aTask)
        #expect(r["cancelled"].asBool == true)
        // Leaves-up: the child is killed before the parent that awaits it.
        #expect(r["killed"] == .array([.string(bTask), .string(aTask)]))

        // The subtree is GONE from the table.
        let live = Set((await rows(kernel)).compactMap { $0["id"].asString })
        #expect(!live.contains(aTask))
        #expect(!live.contains(bTask))

        // The blocked caller gets the cancellation as a REPLY, not a throw.
        let reply = await replyOf(caller)
        #expect(reply == .object(["cancelled": .bool(true)]))

        // The onCancel hook ran for BOTH agents, each with its own task id, and
        // in both cases BEFORE the abort (the task was still live in the table).
        let log = sh.hookLog()
        #expect(log.map { "\($0.0)/\($0.1)" } == ["\(b.value)/\(bTask)", "\(a.value)/\(aTask)"])
        #expect(log.allSatisfy { $0.2 }, "hooks ran pre-abort")
    }

    @Test func cancelUnknownOrFinishedTaskIsHonest() async throws {
        // An unknown id — and an id that already finished — cancel NOTHING, and
        // say so: {"cancelled": false, "killed": []}. No pretending, no crash.
        let sh = Shared()
        let kernel = makeSlowKernel(sh)
        let a = await makeAgent(kernel, "slow_a", canCall: ["slow_b": "*"])

        let unknown = await cancel(kernel, "no_such_agent:7")
        #expect(unknown == .object(["cancelled": .bool(false), "killed": .array([])]))

        // A task id that WAS live and is now finished (we just cancelled it).
        let caller = spawnCaller(kernel, a, .object(["type": .string("slow")]))
        defer { caller.cancel() }
        let taskId = try await waitForTask(kernel, a, "slow")
        let first = await cancel(kernel, taskId)
        #expect(first["cancelled"].asBool == true)
        let reply = await replyOf(caller)
        #expect(reply == .object(["cancelled": .bool(true)]))

        let again = await cancel(kernel, taskId)
        #expect(again == .object(["cancelled": .bool(false), "killed": .array([])]))

        // A malformed call fails LOUD, and CODED — python's `missing_arg` with
        // the arg it wanted, not a prose-only `{error}` a caller had to
        // string-match to branch on.
        let bad = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("cancel")]))])[0].result
        #expect(bad["error"].asString == "cancel: task_id required")
        #expect(bad["reason"].asString == "missing_arg")
        #expect(bad["arg"].asString == "task_id")
        #expect(bad["cancelled"] == .null)
        let empty = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("cancel"), "task_id": .string("")]))])[0].result
        #expect(empty["error"].asString == "cancel: task_id required")
        #expect(empty["reason"].asString == "missing_arg")
    }

    // MARK: - the emergent one: an AGENT cancels another agent's work

    @Test func agentCancelsAnotherAgentsTask() async throws {
        // `cancel` is a PLAIN VERB, so an agent wields it: the killer agent's own
        // handler discovers the victim's task through `ps` and sends `cancel`. No
        // UI, no privileged caller — the capability is in the substrate.
        let sh = Shared()
        let kernel = makeSlowKernel(sh)
        let victim = await makeAgent(kernel, "slow_victim")
        let killer = await makeAgent(kernel, "slow_killer")

        let caller = spawnCaller(kernel, victim, .object(["type": .string("slow")]))
        defer { caller.cancel() }
        let victimTask = try await waitForTask(kernel, victim, "slow")

        let r = await kernel.send([Item(killer, .object(["type": .string("kill"), "victim": .string(victim.value)]))])[0].result
        #expect(r["cancelled"].asBool == true)
        #expect(r["killed"] == .array([.string(victimTask)]))

        let live = Set((await rows(kernel)).compactMap { $0["id"].asString })
        #expect(!live.contains(victimTask))
        let reply = await replyOf(caller)
        #expect(reply == .object(["cancelled": .bool(true)]))
    }

    // MARK: - onCancel: the reliability hook

    @Test func onCancelHookRunsAndReleasesItsResource() async throws {
        // Cancelling a Swift Task frees NO real resource. The slow handler TAKES
        // one and never gives it back on its own — only the hook does. Cancel must
        // therefore leave nothing held (this is the orphaned-subprocess class of
        // bug).
        let sh = Shared()
        let kernel = makeSlowKernel(sh)
        let a = await makeAgent(kernel, "slow_a", canCall: ["slow_b": "*"])

        let caller = spawnCaller(kernel, a, .object(["type": .string("slow")]))
        defer { caller.cancel() }
        let taskId = try await waitForTask(kernel, a, "slow")
        try await waitUntil("the resource to be held") { sh.heldIds().contains(taskId) }

        let r = await cancel(kernel, taskId)
        #expect(r["cancelled"].asBool == true)

        #expect(sh.heldIds().isEmpty, "onCancel must release the resource: \(sh.heldIds())")
        let log = sh.hookLog()
        #expect(log.count == 1)
        #expect(log.first?.0 == a.value)
        #expect(log.first?.1 == taskId)
        #expect(log.first?.2 == true, "the hook ran BEFORE the abort")
    }

    // MARK: - long-lived work: init-adoption + reachable by cancel

    @Test func forkedWorkOutlivesItsDispatchAndIsAdoptedByItsGrandparent() async throws {
        // Work can OUTLIVE the dispatch that spawned it (a generation, a job, a
        // pump). The bundle registers it with `kernel.spawnTask`, so `ps` sees it;
        // when the dispatch that forked it dies, the job is RE-PARENTED to that
        // dispatch's parent (init-adoption, exactly like Linux) — which is what
        // makes "cancel the turn" also kill the work the turn spawned.
        let sh = Shared()
        let kernel = makeSlowKernel(sh)
        let a = await makeAgent(kernel, "slow_a", canCall: ["slow_b": "*"])
        let b = await makeAgent(kernel, "slow_b", callableBy: ["slow_a": "*"])

        let caller = spawnCaller(
            kernel, a, .object(["type": .string("chain_fork"), "next": .string("slow_b")]))
        defer { caller.cancel() }
        let aTask = try await waitForTask(kernel, a, "chain_fork")
        let job = try await waitForTask(kernel, b, "job")

        // The `fork` dispatch that spawned the job is GONE (it replied at once) —
        // but the job lives on, re-parented to the task that forked it.
        try await waitUntil("the fork dispatch to retire") {
            await rowFor(kernel, b, "fork") == nil
        }
        let jobRow = try #require(await rowFor(kernel, b, "job"))
        #expect(jobRow["parent"].asString == aTask, "init-adoption")

        // Killing the OUTER task reaches the forked job through the adopted link.
        let r = await cancel(kernel, aTask)
        #expect(r["cancelled"].asBool == true)
        #expect(r["killed"] == .array([.string(job), .string(aTask)]))
        #expect(sh.heldIds().isEmpty, "the job's onCancel must release it: \(sh.heldIds())")
        let reply = await replyOf(caller)
        #expect(reply == .object(["cancelled": .bool(true)]))
    }

    // MARK: - reflect advertises the substrate's own verbs

    @Test func rootReflectAdvertisesPsAndCancel() async throws {
        // `ps`/`cancel` and the tree verbs belong to no bundle — they are the
        // SUBSTRATE's own, answered by the root alone. They are advertised on
        // EVERY agent: root-only was tried and reverted, because a model that
        // never reflects the root never learns they exist, guesses, and gives up.
        let sh = Shared()
        let kernel = makeSlowKernel(sh)
        let a = await makeAgent(kernel, "slow_a", canCall: ["slow_b": "*"])

        let r = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("reflect")]))])[0].result
        let kv = r["kernel_verbs"]
        // The `_` line says WHERE to send them and what the named id means —
        // without it "send create_agent to X" reads three ways.
        let preamble = try #require(kv["_"].asString)
        #expect(preamble.contains("kernel"))
        #expect(preamble.contains("OPERAND"))

        // THE CONSEQUENCE PRECEDES THE FIRST EM-DASH, so a default reflect
        // (which truncates there) still carries the fact a caller needs BEFORE
        // firing.
        let cancelDoc = try #require(kv["cancel"].asString)
        #expect(cancelDoc.contains("task_id"))
        #expect(
            cancelDoc.lowercased().contains("subtree"),
            "the cascade must survive truncation: \(cancelDoc)")
        #expect(!cancelDoc.contains("—"), "a default reflect truncates at the dash")

        let createDoc = try #require(kv["create_agent"].asString)
        #expect(
            createDoc.contains("parent_id:str REQUIRED"),
            "placement is the fact a caller needs first: \(createDoc)")
        let updateDoc = try #require(kv["update_agent"].asString)
        #expect(
            updateDoc.contains("REPLACED, NOT MERGED"),
            "replace-not-merge must precede the dash: \(updateDoc)")

        for v in ["create_agent", "update_agent", "delete_agent", "list_agents", "get", "ps"] {
            #expect(kv[v].asString != nil, "missing kernel verb \(v)")
        }

        // EVERY agent carries them, not just the root.
        let child = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
        #expect(
            child["kernel_verbs"]["cancel"].asString != nil,
            "a non-root agent must advertise them too")

        // `detail:"full"` keeps each doc WHOLE.
        let full = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("reflect"), "detail": .string("full")]))])[0].result
        let fullPs = try #require(full["kernel_verbs"]["ps"].asString)
        #expect(fullPs.contains("tasks"), "full detail keeps the shape")
        #expect(fullPs.contains("—"))
    }
}
