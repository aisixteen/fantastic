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
public struct TaskId: Sendable, Hashable, CustomStringConvertible {
    public let value: String
    public init(_ value: String) { self.value = value }
    public var description: String { value }
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

public final class TaskTable: @unchecked Sendable {
    private let lock = NSLock()
    private var tasks: [TaskId: TaskEntry] = [:]
    /// Per-agent task counter (the `n` in `"<agent_id>:<n>"`).
    private var seq: [AgentId: UInt64] = [:]
    /// Per-table, never persisted — makes the minted token opaque across runs.
    private let salt = UInt64.random(in: .min ... .max)
    private var order: UInt64 = 0

    public init() {}

    /// Seconds since the epoch — the `started_at` clock (Python's `time.time()`).
    private func nowSecs() -> Double { Date().timeIntervalSince1970 }

    /// Next task id for `agentId` — `"<agent_id>:<8 lowercase hex>"`.
    ///
    /// OPAQUE on purpose. A visible counter reads as a SEQUENCE, and messaging here is
    /// fully async: a send may return once, several times, or never, and replies arrive
    /// out of order. Measured 2026-08-20 (python): with `<agent>:<n>` a model inferred
    /// ordering from the number, decided "the reply queue is running ahead of me", and
    /// re-polled forever instead of matching on the id. The monotonic `seq` is KEPT — it
    /// is what guarantees non-reuse — but hashed out of view, and a collision with a live
    /// row re-rolls rather than aliasing two dispatches onto one id.
    ///
    /// The salt is per-table and never persisted, so ids carry no meaning outside this
    /// process. Matching the SHAPE is what parity needs; the hash is an implementation
    /// detail (python uses keyed blake2b).
    func mint(_ agentId: AgentId) -> TaskId {
        lock.lock()
        defer { lock.unlock() }
        while true {
            let n = (seq[agentId] ?? 0) + 1
            seq[agentId] = n
            var h = Hasher()
            h.combine(salt)
            h.combine(agentId.value)
            h.combine(n)
            let token = String(format: "%08x", UInt32(truncatingIfNeeded: h.finalize()))
            let id = TaskId("\(agentId.value):\(token)")
            if tasks[id] == nil { return id }
        }
    }

    /// Put one in-flight task in the table.
    ///
    /// A `parent` that is no longer in the table is dropped to `nil` — the task
    /// is a root, adopted by init. Not defensive slack: a bundle's long-lived
    /// work outlives the dispatch that forked it, and a `send` it issues later
    /// must not claim a dead parent or the `ps` tree would lie.
    func register(id: TaskId, agent: AgentId, verb: String, parent: TaskId?, handle: TaskHandle) {
        lock.lock()
        defer { lock.unlock() }
        let parent = parent.flatMap { tasks[$0] != nil ? $0 : nil }
        order += 1
        tasks[id] = TaskEntry(
            agent: agent, parent: parent, verb: verb,
            startedAt: nowSecs(), order: order, handle: handle)
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
                    "id": .string(id.value),
                    "agent": .string(e.agent.value),
                    "parent": e.parent.map { JSON.string($0.value) } ?? .null,
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

    /// Phase ① of `cancel`: `id` + every descendant, POST-ORDER (leaves first —
    /// the kill order of a process group), with every one of them MARKED killed
    /// before the caller runs a single hook or stops a single task. `nil` if
    /// `id` isn't in the table.
    func beginCancel(_ id: TaskId) -> [TaskId]? {
        lock.lock()
        defer { lock.unlock() }
        guard tasks[id] != nil else { return nil }
        var kids: [TaskId: [TaskId]] = [:]
        for (tid, e) in tasks {
            if let p = e.parent {
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
    func abortSubtree(_ ids: [TaskId]) -> [String] {
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
        work: @escaping @Sendable () async -> Void
    ) -> TaskId {
        let taskId = tasks.mint(agent)
        let handle = TaskHandle()
        tasks.register(
            id: taskId, agent: agent, verb: verb,
            parent: KernelTaskLocals.currentTask, handle: handle)

        let table = tasks
        let t = Task {
            defer { table.forget(taskId) }
            // A `Task {}` INHERITS the forking dispatch's task-locals — stamp
            // this task's OWN id + killed flag so its `send`s nest under IT and
            // its own kill (not its forker's) is what refuses its work.
            await KernelTaskLocals.$currentTask.withValue(taskId) {
                await KernelTaskLocals.$currentKilled.withValue(handle.killed) {
                    await work()
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
    /// fails honestly, it never pretends.
    public func cancelTask(_ taskId: TaskId) async -> JSON {
        guard let ids = tasks.beginCancel(taskId) else {
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
            // not refusable.
            try? await bundle.onCancel(agentId: agentId, taskId: tid, kernel: self)
        }
        // ② SIGKILL — leaves-up.
        let killed = tasks.abortSubtree(ids)
        return .object([
            "cancelled": .bool(true),
            "killed": .array(killed.map { .string($0) }),
        ])
    }
}
