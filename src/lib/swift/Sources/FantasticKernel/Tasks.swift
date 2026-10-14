// The TASK TABLE — `ps` + `cancel`, the kernel's process table.
//
// Mirrors Python's `kernel/_kernel.py` (canonical) and Rust's
// `fantastic-kernel/src/tasks.rs`.
//
// The unit is a **task**: ONE in-flight dispatch, not an agent. Its id
// (`"<agent_id>:<n>"`) is a pid — `n` is a per-agent counter, monotonic for the
// process lifetime, so an id is never reused and a stale id cancels nothing
// rather than killing an innocent task.
//
// Tasks form a **tree** through the send chain: a `send` issued INSIDE a handler
// is a CHILD of that handler's task. The current id rides the `currentTask`
// task-local exactly as the sender rides `currentSender` — and because an
// unstructured `Task {}` INHERITS the enclosing task-locals, the scope is
// re-established INSIDE the spawned dispatch (see `Kernel.runDispatch`) so the
// new dispatch stamps its OWN id rather than inheriting its caller's. That tree
// is what makes `cancel` a process-GROUP kill rather than a single abort.
//
// ## Cancellation is two-phase (SIGTERM → SIGKILL)
//
// Cancelling a Swift `Task` frees NO real resource — it flips a flag the work
// may or may not read. An OS subprocess, a PTY, an HTTP stream held for that
// task must be released by its bundle first. So `Kernel.cancelTask`:
//
//   1. `on_cancel` (`AgentBundle.onCancel`) — awaited for the WHOLE subtree
//      while every task is still alive, so a bundle can release what it owns and
//      no handler observes a half-killed group.
//   2. abort — leaves-up, synchronously (no `await` in that loop): a parent can
//      never resume between its child's death and its own.
//
// ## How a task is stopped — and where Swift differs from Rust
//
// Rust DROPS the dispatch future (a `select!` against a stop signal): the frame
// dies at its next yield, taking every guard it holds with it. Swift's
// cancellation is COOPERATIVE — nothing can force-unwind a running frame. So a
// stop here does two things at once:
//
//   - resolves the caller's `DispatchBox` immediately with `nil` (= stopped), so
//     the blocked `send` returns `{"cancelled": true}` AT ONCE, without waiting
//     for the killed handler to notice; and
//   - cancels the dispatch's Swift `Task`, so its cancellation-aware awaits
//     (`Task.sleep`, `AsyncStream` iteration, an HTTP body) throw/finish and the
//     handler unwinds through its OWN cleanup — releasing the locks and guards
//     Rust would have dropped with the frame.
//
// A killed handler that ignores cancellation keeps running for a while. It
// cannot resurrect itself: from the moment of the kill the kernel is INERT to it
// — every `send` it issues returns `{"cancelled": true}` without registering a
// row or running a handler (the `currentKilled` guard at the top of
// `runDispatch`, the exact point where Rust would have dropped the future). So
// the observable contract is identical in both runtimes: no zombie rows in `ps`,
// no second reply, no work spawned after the kill.

import FantasticJSON
import Foundation

/// A task id — `"<agent_id>:<n>"`. A pid.
/// A task id IS a message id: one int64 from the kernel's one counter, so
/// `ps`, `cancel`, the ledger, a reply tag and the message log name the same
/// number. It was `"<agent>:<8 hex>"` from a salted per-agent sequence until
/// 2026-09-12 — python reversed that trade (a number that names one thing
/// everywhere is worth more than one that hides its order), rust followed.
public struct TaskId: Sendable, Hashable, CustomStringConvertible {
    public let value: Int64
    public init(_ value: Int64) { self.value = value }
    public var description: String { String(value) }
}

/// "My own task is inside a subtree the kernel is killing." Set by
/// `Kernel.cancelTask` on every marked row BEFORE any task is stopped, so a
/// handler woken by its child's death does not resurrect itself: its next
/// kernel call is refused instead of registering fresh work.
public final class KilledFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var killed = false

    public var isKilled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return killed
    }

    func mark() {
        lock.lock()
        killed = true
        lock.unlock()
    }
}

/// One-shot reply slot for a dispatch. Resolved EITHER by the dispatch task
/// (its reply) OR by `TaskHandle.stop()` (`nil` = the kernel killed it) —
/// whichever lands first wins, so a killed dispatch that later finishes anyway
/// (Swift cannot force-unwind it) can never deliver a second reply.
final class DispatchBox: @unchecked Sendable {
    private let lock = NSLock()
    private var isResolved = false
    private var stored: JSON?
    private var waiter: CheckedContinuation<JSON?, Never>?

    /// Deliver the outcome. `nil` means "stopped". Idempotent.
    func resolve(_ reply: JSON?) {
        lock.lock()
        if isResolved {
            lock.unlock()
            return
        }
        isResolved = true
        stored = reply
        let w = waiter
        waiter = nil
        lock.unlock()
        w?.resume(returning: reply)
    }

    /// Await the outcome — the reply, or `nil` if the task was stopped.
    func wait() async -> JSON? {
        await withCheckedContinuation { (c: CheckedContinuation<JSON?, Never>) in
            lock.lock()
            if isResolved {
                let v = stored
                lock.unlock()
                c.resume(returning: v)
            } else {
                waiter = c
                lock.unlock()
            }
        }
    }
}

/// The stop handle of one registered task: the killed flag its own descendants
/// read, plus whatever must be torn down to stop it (the Swift `Task`, and —
/// for a dispatch — the box its caller is blocked on).
///
/// Created BEFORE the task is spawned so the row is complete (and killable) the
/// instant it enters the table: a `stop()` that lands before `attach` still
/// takes effect the moment the task shows up.
public final class TaskHandle: @unchecked Sendable {
    private let lock = NSLock()
    private var stopped = false
    private var task: Task<Void, Never>?
    private var box: DispatchBox?

    /// The flag scoped into the task as `KernelTaskLocals.currentKilled`.
    let killed = KilledFlag()

    init() {}

    /// Phase ① — mark the task as dying. Its own `send`s are refused from here
    /// on (see `Kernel.runDispatch`), so a task the kernel is killing cannot
    /// register new work while the kill walks the subtree.
    func mark() {
        killed.mark()
    }

    /// Bind the running task (and, for a dispatch, the box its caller awaits).
    /// If `stop()` already fired, honour it right now — closing the window
    /// between `register` and `Task {}` actually starting.
    func attach(task: Task<Void, Never>, box: DispatchBox?) {
        lock.lock()
        if stopped {
            lock.unlock()
            box?.resolve(nil)
            task.cancel()
            return
        }
        self.task = task
        self.box = box
        lock.unlock()
    }

    /// Phase ② — stop the task. Resolves the blocked caller AT ONCE (`nil` =
    /// stopped ⇒ `{"cancelled": true}`) and cancels the Swift task so the
    /// handler unwinds through its own cleanup. Idempotent: stopping a finished
    /// task is a no-op (its box is already resolved, its task already done).
    func stop() {
        lock.lock()
        stopped = true
        let t = task
        let b = box
        task = nil
        box = nil
        lock.unlock()
        b?.resolve(nil)
        t?.cancel()
    }
}

/// One row of the table. `handle` is the stop handle and never leaves the
/// kernel — `ps` serves the five public fields only.
private struct TaskEntry {
    let agent: AgentId
    var parent: TaskId?
    let verb: String
    let startedAt: Double
    /// Registration order — `ps` reports oldest first, deterministically (the
    /// `startedAt` clock is too coarse to sort by).
    let order: UInt64
    let handle: TaskHandle
    /// A loop that is SUPPOSED to outlive a graceful agent shutdown — a
    /// persister tailing the row store, a scheduler tick. `shutdown_agents`
    /// neither waits for it nor cancels it; it dies with the process. Python
    /// `_keepalive`.
    let keepalive: Bool
    /// A `spawnTask` FORK — a service loop (scheduler tick, bridge read loop,
    /// ws drain, job pump, pty reader), not a dispatch. It is work that
    /// outlives its dispatch BY CONSTRUCTION, so it never settles:
    /// `shutdownAgents` cancels it instead of waiting for it. Python
    /// `_forked`.
    let forked: Bool
}

/// The live table of in-flight work. Lives on the `Kernel`; `ps` reads it,
/// `cancel` acts on it.
///
/// A plain `NSLock` — every critical section is a few dictionary operations with
/// no `await` inside (the `onCancel` hooks in `Kernel.cancelTask` run BETWEEN
/// locked sections, never under the lock).
/// A one-slot carrier for the PID a dispatch mints, so a caller can read it at send
/// time. Swift has no inout across an `async` boundary, so the out-param is a box.
public final class TaskIdBox: @unchecked Sendable {
    public var value: TaskId?
    public init() {}
}

/// What one dispatch's spawned task hands back beside its reply (C37): the
/// BINARY REPLY BODY, and whether the handler RAISED. Written inside the task
/// before it resolves its `DispatchBox` and read only after that box has been
/// awaited, so the box's own lock orders the two.
///
/// `body == nil` after a dispatch means the verb returned NO chunk; `.some(b)`
/// means it returned one at its slot of the reply, `b.bytes` possibly empty.
final class DispatchExtras: @unchecked Sendable {
    var body: Blob?
    var raised = false
    init() {}
}

/// How ONE dispatch ended — python's three exits from `await d.task`: a reply,
/// a raise (the `except Exception` branch) or a cancelled task. The status is
/// read from HERE, never from the reply's own keys.
struct DispatchEnd {
    var reply: JSON
    var body: Blob? = nil
    var raised = false
    var stopped = false
}

/// Set once, by the deadline branch of a timed dispatch, BEFORE it kills: the
/// one fact that makes a stopped task `timed_out` rather than `cancelled`.
final class FiredFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var fired = false
    init() {}
    func set() { lock.withLock { fired = true } }
    var isSet: Bool { lock.withLock { fired } }
}

public final class TaskTable: @unchecked Sendable {
    private let lock = NSLock()
    private var tasks: [TaskId: TaskEntry] = [:]
    private var order: UInt64 = 0

    public init() {}

    /// Seconds since the epoch — the `started_at` clock (Python's `time.time()`).
    private func nowSecs() -> Double { Date().timeIntervalSince1970 }

    // NO MINTER HERE. `mint` used to hand out `"<agent>:<8 hex>"` from a salted
    // per-agent counter, hashed so a visible sequence could not be read as an
    // ordering. Python reversed that trade: ONE int64 counter allocates every id
    // in the kernel, so `ps`, `cancel`, the ledger, a reply tag and the message
    // store all name the same number. The allocator is `kernel.mintId()`;
    // the caller mints, and this table only registers.

    /// Put one in-flight task in the table.
    ///
    /// THE PARENT IS STORED VERBATIM. There used to be a `parent` → `nil` drop
    /// here for the case where `currentTask` named a task that had already
    /// ended, defended as init-adoption. Python removed exactly that
    /// (`_kernel.py::register_task`): it was the fingerprint of a bug, not a
    /// rule — converting a dead parent to `nil` puts the work OUTSIDE every
    /// subtree, where `cancel` on its caller can never reach it, and
    /// `forget`'s re-parenting is what should have handled it. It also made
    /// every REST-originated `ps` row claim `parent: null` where python names
    /// the web boot pid (A11.31: the boot dispatch has already settled by the
    /// time a request lands, so the pid it hands down is dangling by design).
    func register(
        id: TaskId, agent: AgentId, verb: String, parent: TaskId?, handle: TaskHandle,
        keepalive: Bool = false, forked: Bool = false
    ) {
        lock.lock()
        defer { lock.unlock() }
        order += 1
        tasks[id] = TaskEntry(
            agent: agent, parent: parent, verb: verb,
            startedAt: nowSecs(), order: order, handle: handle, keepalive: keepalive,
            forked: forked)
    }

    /// `id` and every ancestor of it — the chain CARRYING a dispatch. Over a
    /// wire the leg's `inbound_call` is the root of the operator's call, so
    /// `shutdownAgents` spares the whole chain, not only its own task:
    /// cancelling the carrier cancels the verb's own reply. Python's `mine` set.
    public func carriers(_ id: TaskId?) -> Set<TaskId> {
        lock.lock()
        defer { lock.unlock() }
        var chain: Set<TaskId> = []
        var cur = id
        while let t = cur, !chain.contains(t), let e = tasks[t] {
            chain.insert(t)
            cur = e.parent
        }
        return chain
    }

    /// Every live task that is NOT a keepalive loop and not in `except` — what
    /// `shutdown_agents` waits to drain, then cancels. Python `pending()`.
    public func pending(except: Set<TaskId>) -> [TaskId] {
        lock.lock()
        defer { lock.unlock() }
        return tasks.filter { !$0.value.keepalive && !except.contains($0.key) }.map { $0.key }
    }

    /// The pending DISPATCHES — work that can finish on its own. A `spawnTask`
    /// FORK (scheduler tick, bridge read loop, ws drain, job pump, pty reader)
    /// is a service loop that never settles by construction, so waiting on one
    /// burned the WHOLE quiet timeout on every daemon that ran one. Forks are
    /// CANCELLED by `shutdownAgents`, never awaited. Python `settling()`.
    public func settling(except: Set<TaskId>) -> [TaskId] {
        lock.lock()
        defer { lock.unlock() }
        return tasks.filter { !$0.value.keepalive && !$0.value.forked && !except.contains($0.key) }
            .map { $0.key }
    }

    /// `true` iff `id` is a live ROOT task — the only tasks `shutdown_agents`
    /// cancels, since a subtree dies with its root.
    public func isRoot(_ id: TaskId) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return tasks[id].map { $0.parent == nil } ?? false
    }

    /// Drop one row. Surviving children are RE-PARENTED to the dead task's
    /// parent — init-adoption: work forked out of a dispatch outlives it and
    /// must stay in the caller's process group, else cancelling the caller could
    /// never reach it. No-op if absent.
    func forget(_ id: TaskId) {
        lock.lock()
        defer { lock.unlock() }
        removeAndReparent(id)
    }

    /// `true` iff `id` names a live task.
    public func contains(_ id: TaskId) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return tasks[id] != nil
    }

    /// While draining, may a send made under task `cur` (inside the current
    /// Swift task) start? Yes inside a live DISPATCH and inside a keepalive fork
    /// (a persister that must outlive the quiet phase); no inside any other fork
    /// — a ws call, a scheduler tick, a bridge read loop starts new work (python
    /// `Kernel.drain_admits`; audit 2026-10-14).
    public func drainAdmits(_ cur: TaskId?) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        let e = cur.flatMap { tasks[$0] } ?? KernelTaskLocals.ownTask.flatMap { tasks[$0] }
        guard let e else { return false }
        return !e.forked || e.keepalive
    }

    /// The public table — the `ps` reply rows `{id, agent, parent, verb,
    /// started_at}`, oldest first. The stop handle stays behind.
    public func rows() -> [JSON] {
        lock.lock()
        defer { lock.unlock() }
        return
            tasks
            .sorted { $0.value.order < $1.value.order }
            .map { id, e in
                .object([
                    "id": .integer(id.value),
                    "agent": .string(e.agent.value),
                    "parent": e.parent.map { JSON.integer($0.value) } ?? .null,
                    "verb": .string(e.verb),
                    "started_at": .double(e.startedAt),
                ])
            }
    }

    /// The agent that owns `id`, if it is still live.
    func agentOf(_ id: TaskId) -> AgentId? {
        lock.lock()
        defer { lock.unlock() }
        return tasks[id]?.agent
    }

    /// The verb of `id` when it is a KERNEL SERVICE TASK — a keepalive loop the
    /// ROOT owns (`root`): the execution flusher, the record persister. `cancel`
    /// never kills one (audit KV-5, owner 2026-10-03): killing it stopped ledger
    /// durability and record persistence until the process restarted, and
    /// nothing restarted it. It stays listed in `ps`; the kernel's own stop ends
    /// it. Python `Kernel.is_service_task`.
    func serviceVerb(_ id: TaskId, root: AgentId) -> String? {
        lock.lock()
        defer { lock.unlock() }
        guard let e = tasks[id], Self.isService(e, root: root) else { return nil }
        return e.verb
    }

    /// Is `e` a kernel service task — a keepalive loop the root owns?
    private static func isService(_ e: TaskEntry, root: AgentId?) -> Bool {
        e.keepalive && e.agent == root
    }

    /// Phase ① of `cancel`: `id` + every descendant, POST-ORDER (leaves first —
    /// the kill order of a process group), with every one of them MARKED killed
    /// before the caller runs a single hook or stops a single task. `nil` if
    /// `id` isn't in the table. A kernel service task below `id` (a flusher
    /// started by a `kernel_start` some agent's dispatch sent) is not part of
    /// the group: it and its own work are left out.
    func beginCancel(_ id: TaskId, root: AgentId?) -> [TaskId]? {
        lock.lock()
        defer { lock.unlock() }
        guard tasks[id] != nil else { return nil }
        var kids: [TaskId: [TaskId]] = [:]
        for (tid, e) in tasks {
            if let p = e.parent, !Self.isService(e, root: root) {
                kids[p, default: []].append(tid)
            }
        }
        // Deterministic order among siblings (oldest first).
        for (p, v) in kids {
            kids[p] = v.sorted { (tasks[$0]?.order ?? 0) < (tasks[$1]?.order ?? 0) }
        }
        var out: [TaskId] = []
        walk(id, kids, &out)
        for tid in out {
            tasks[tid]?.handle.mark()
        }
        return out
    }

    /// Phase ② of `cancel`: stop the whole subtree leaves-up and drop its rows —
    /// SYNCHRONOUS, no `await`, so a parent cannot resume between its child's
    /// death and its own. Returns the ids actually killed (the table only ever
    /// holds LIVE work).
    func abortSubtree(_ ids: [TaskId]) -> [Int64] {
        lock.lock()
        var handles: [(TaskId, TaskHandle)] = []
        for tid in ids {
            if let entry = removeAndReparent(tid) {
                handles.append((tid, entry.handle))
            }
        }
        lock.unlock()
        // Stop OUTSIDE the lock (a `stop` resumes the caller's continuation),
        // but the rows are already gone — the table is consistent the whole way.
        for (_, handle) in handles {
            handle.stop()
        }
        return handles.map { $0.0.value }
    }

    /// Post-order DFS of the task tree.
    private func walk(_ id: TaskId, _ kids: [TaskId: [TaskId]], _ out: inout [TaskId]) {
        for c in kids[id] ?? [] {
            walk(c, kids, &out)
        }
        out.append(id)
    }

    /// Remove one row, re-parenting its surviving children to its parent.
    /// Caller holds the lock.
    @discardableResult
    private func removeAndReparent(_ id: TaskId) -> TaskEntry? {
        guard let entry = tasks.removeValue(forKey: id) else { return nil }
        for (tid, e) in tasks where e.parent == id {
            tasks[tid]?.parent = entry.parent
        }
        return entry
    }
}

extension Kernel {
    /// Register BACKGROUND work a bundle forks out of a dispatch (an LLM
    /// generation, an OS subprocess pump, a PTY reader) — work that OUTLIVES the
    /// dispatch that started it. Bundles call this so their real work is visible
    /// to `ps` and killable by `cancel`; without it the dispatch returns, its row
    /// drops, and the resource is unreachable (that is the orphan bug).
    ///
    /// Parent = the dispatch that forked it, so the fork stays inside the
    /// caller's process group even after that dispatch returns (its row is
    /// re-parented to init on removal). Inside the spawned task the new task id
    /// is scoped, so anything the work `send`s nests under IT.
    ///
    /// Returns the new task id.
    @discardableResult
    public func spawnTask(
        agent: AgentId,
        verb: String,
        keepalive: Bool = false,
        work: @escaping @Sendable () async -> Void
    ) -> TaskId {
        // THE COUNTER, not a table-local sequence: a fork is a message, and its
        // id comes from the one allocator like every other id in the kernel.
        // The `task` row names the fork for the trace, parented to the dispatch
        // that forked it.
        let parent = KernelTaskLocals.currentTask
        var row: Row = ["kind": .string("task"), "verb": .string(verb)]
        row["parent"] = parent.map { JSON.integer($0.value) } ?? .null
        // `put` refuses ONE thing — a caller-supplied id — and this row carries
        // none, so the fork's id IS its row id, the way python's `spawn_task`
        // returns what `rows_put` minted. Minting a second id on a failure that
        // cannot happen would name a task no row describes.
        let taskId = TaskId(try! rowsPut(agent, row))
        let handle = TaskHandle()
        tasks.register(
            id: taskId, agent: agent, verb: verb,
            parent: parent, handle: handle, keepalive: keepalive, forked: true)

        let table = tasks
        let t = Task {
            defer { table.forget(taskId) }
            // A `Task {}` INHERITS the forking dispatch's task-locals — stamp
            // this task's OWN id + killed flag so its `send`s nest under IT and
            // its own kill (not its forker's) is what refuses its work.
            await KernelTaskLocals.$currentTask.withValue(taskId) {
                await KernelTaskLocals.$ownTask.withValue(taskId) {
                    await KernelTaskLocals.$currentKilled.withValue(handle.killed) {
                        await work()
                    }
                }
            }
        }
        handle.attach(task: t, box: nil)
        return taskId
    }

    /// Kill a task AND ITS SUBTREE — children first, like killing a process
    /// group. Two-phase, mirroring SIGTERM → SIGKILL:
    ///
    /// ① `onCancel` — each agent's bundle hook releases the REAL resources it
    ///    holds for that task (an OS subprocess, a PTY, an HTTP stream).
    ///    Cancelling a Swift `Task` frees NONE of those. Nothing is stopped in
    ///    this phase, so no handler can wake mid-walk and observe a half-killed
    ///    group.
    /// ② stop — the tasks are cancelled leaves-up, synchronously.
    ///
    /// Returns `{cancelled, killed:[task_id]}`. An unknown or already-finished
    /// id is not in the table → `{"cancelled": false, "killed": []}`: cancel
    /// fails honestly, it never pretends. A kernel service task
    /// (`TaskTable.serviceVerb`) is refused, and left alive.
    public func cancelTask(_ taskId: TaskId) async -> JSON {
        let rootId = root?.id
        if let rootId, let verb = tasks.serviceVerb(taskId, root: rootId) {
            return Refusal.make(
                Refusal.badArg,
                "cancel: task \(taskId.value) (\(verb)) is a kernel service task — not cancellable",
                hint: "it runs for the life of the kernel and stays listed in ps; kernel_stop ends it",
                extra: [("arg", .string("task_id"))])
        }
        guard let ids = tasks.beginCancel(taskId, root: rootId) else {
            return .object(["cancelled": .bool(false), "killed": .array([])])
        }
        // ① SIGTERM — hooks first, subtree-wide, while everything is still alive.
        for tid in ids {
            guard let agentId = tasks.agentOf(tid),
                let agent = agent(agentId),
                let hm = agent.handlerModule,
                let bundle = bundles.get(hm)
            else { continue }
            // A hook that fails is ignored and the kill still happens: cancel is
            // not refusable — but python still SAYS so (C87), stdout, no
            // `flush=True` on its own print either:
            // `print(f"  [cancel] {self.id} bundle on_cancel raised: {e}")`.
            //
            // AS THE AGENT ITSELF, on every path that kills the task — the
            // `cancel` verb (inside the root's dispatch), an item's own deadline,
            // `shutdownAgents` (audit G-3, owner 2026-10-03): an emit on its own
            // inbox is ungated, every other send is gated as this agent. The verb
            // path used to run it as the ROOT, so a non-top-level agent's
            // own-inbox emit (ai_core's final `status done`) was gated root →
            // agent and dropped; the deadline path ran it as the operator.
            do {
                try await asSelf(agentId) {
                    try await bundle.onCancel(agentId: agentId, taskId: tid, kernel: self)
                }
            } catch {
                RuntimeLog.out(
                    "  [cancel] \(agentId.value) bundle on_cancel raised: \(pyErrorText(error))")
            }
        }
        // ② SIGKILL — leaves-up.
        let killed = tasks.abortSubtree(ids)
        return .object([
            "cancelled": .bool(true),
            "killed": .array(killed.map { .integer($0) }),
        ])
    }
}
