// Per-agent send coordination — the FIFO lock + queue + in-flight entry
// behind the `status` verb and the phase-status events. Mirrors Rust's
// `fantastic-ai-core::state` (BackendState / QueuedEntry / CurrentEntry /
// status_snapshot) and the Python `_queue` / `_current` machinery.
//
// Only ONE generation runs at a time per agent: concurrent callers (cli,
// browser tabs) queue in arrival order. Serialization also keeps the
// file_bridge-persisted chat history race-free (two sends on one client
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
}

/// The submission currently holding the lock (drives the `status` verb).
struct AICurrentEntry: Sendable {
    let clientId: String
    let text: String
    let sendId: String
    let startedAt: Double
    var phase: String  // thinking | streaming | tool_calling | done
    var textSoFar: String
    var lastTool: JSON?
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
    func popToCurrent(sendId: String, startedAt: Double, phase: String) {
        lock.lock()
        defer { lock.unlock() }
        guard let i = queue.firstIndex(where: { $0.sendId == sendId }) else { return }
        let e = queue.remove(at: i)
        current = AICurrentEntry(
            clientId: e.clientId, text: e.text, sendId: e.sendId,
            startedAt: startedAt, phase: phase, textSoFar: "", lastTool: nil)
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
