// Per-agent send coordination — the FIFO lock + queue + in-flight entry
// behind the `status` verb and the phase-status events. Mirrors Rust's
// `fantastic-ai-core::state` (BackendState / QueuedEntry / CurrentEntry /
// status_snapshot) and the Python `_queue` / `_current` machinery.
//
// Only ONE generation runs at a time per agent: concurrent callers (cli,
// browser tabs) queue in arrival order. Serialization also keeps the
// chat-store-persisted history race-free (two sends on one client
// can't interleave load→save).

import FantasticJSON
import Foundation

/// FIFO async mutex. Swift has no built-in async lock; this is the
/// minimal continuation-queue implementation — `acquire` suspends FIFO
/// when held, `release` wakes the oldest waiter.
actor AIFifoLock {
    private var locked = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func acquire() async {
        if !locked {
            locked = true
            return
        }
        await withCheckedContinuation { waiters.append($0) }
    }

    func release() {
        if waiters.isEmpty {
            locked = false
        } else {
            waiters.removeFirst().resume()
        }
    }

    /// True iff a generation currently holds the lock (best-effort
    /// contention check for the `queued` signal).
    func busy() -> Bool { locked }
}

/// One queued submission awaiting the FIFO lock.
struct AIQueuedEntry: Sendable {
    let clientId: String
    let text: String
    let sendId: String
    let queuedAt: Double
    /// This layer's own text to its own agent (a reminder, a malformed-call
    /// report): the incoming row is stored as `kind: "notice"`.
    var notice: Bool = false
}

/// The submission currently holding the lock (drives the `status` verb).
///
/// KEY ORDER IS THE WIRE ORDER for the redacted snapshot: python promotes the
/// QUEUED entry (`client_id, text, send_id, queued_at`) and adds `started_at`,
/// `phase`, `text_so_far`, `task_id` after it, and `_redact_text` keeps that
/// order minus the text-bearing keys.
struct AICurrentEntry: Sendable {
    let clientId: String
    let text: String
    let sendId: String
    /// When this submission ENTERED THE QUEUE — carried through the promotion,
    /// so a redacted snapshot can say how long it waited before it ran.
    let queuedAt: Double
    let startedAt: Double
    var phase: String  // thinking | streaming | tool_calling | done
    var textSoFar: String
    var lastTool: JSON?
    /// The KERNEL TASK this generation runs as (`"<agent>:<n>"`) — the pid `ps`
    /// reports. `cancel <task_id>` kills the turn; the bundle's `onCancel`
    /// matches on this to know the killed task is ITS generation (not some other
    /// dispatch on the same agent) and to close the turn out.
    let taskId: String?
    /// This pass's streaming parts — python's `(splitter, parts_out)` under
    /// `"parts"`: what `stop` checks a `stream_id` against and what `onCancel`
    /// aborts.
    var parts: AIStreamParts? = nil
    /// Who STOPPED this generation (the `stop` verb's caller, or "operator") —
    /// the name the cut marker carries.
    var stoppedBy: String? = nil
    /// Carried from the queue row: the incoming row is a `notice`.
    var notice: Bool = false
}

/// Per-agent coordination state — the FIFO lock, the waiting queue, and
/// the in-flight entry. NSLock guards the queue/current; the async FIFO
/// lock serializes generations.
final class AIAgentRunState: @unchecked Sendable {
    let fifo = AIFifoLock()
    private let lock = NSLock()
    private var queue: [AIQueuedEntry] = []
    private var current: AICurrentEntry?

    func enqueue(_ e: AIQueuedEntry) {
        lock.lock()
        defer { lock.unlock() }
        queue.append(e)
    }

    func queueDepth() -> Int {
        lock.lock()
        defer { lock.unlock() }
        return queue.count
    }

    func queueSnapshot() -> [AIQueuedEntry] {
        lock.lock()
        defer { lock.unlock() }
        return queue
    }

    /// Remove the entry with `sendId` from the queue and make it the
    /// current in-flight entry, started now at `phase`.
    func popToCurrent(sendId: String, startedAt: Double, phase: String, taskId: String?) {
        lock.lock()
        defer { lock.unlock() }
        guard let i = queue.firstIndex(where: { $0.sendId == sendId }) else { return }
        let e = queue.remove(at: i)
        current = AICurrentEntry(
            clientId: e.clientId, text: e.text, sendId: e.sendId, queuedAt: e.queuedAt,
            startedAt: startedAt, phase: phase, textSoFar: "", lastTool: nil,
            taskId: taskId, notice: e.notice)
    }

    /// Drop a still-QUEUED submission (its generation was cancelled before it
    /// ever acquired the FIFO lock, so it never became current).
    func dropQueued(sendId: String) {
        lock.lock()
        defer { lock.unlock() }
        queue.removeAll { $0.sendId == sendId }
    }

    /// Close out a generation the kernel is cancelling: if `taskId` names the
    /// turn currently holding the lock, clear the in-flight entry and hand it
    /// back so the backend can emit its final `status` + `done` (the `onCancel`
    /// half of the task table's two-phase kill). `nil` if the cancelled task
    /// isn't this agent's generation (a `history` / `reflect` dispatch on the
    /// same agent). Mirrors Rust's `take_current_for_cancel`.
    func takeCurrentForCancel(taskId: String) -> AICurrentEntry? {
        lock.lock()
        defer { lock.unlock() }
        guard let c = current, c.taskId == taskId else { return nil }
        current = nil
        return c
    }

    func updateCurrent(_ body: (inout AICurrentEntry) -> Void) {
        lock.lock()
        defer { lock.unlock() }
        guard current != nil else { return }
        body(&current!)
    }

    func currentSnapshot() -> AICurrentEntry? {
        lock.lock()
        defer { lock.unlock() }
        return current
    }

    func clearCurrent() {
        lock.lock()
        defer { lock.unlock() }
        current = nil
    }
}
