// KERNEL LIFECYCLE — drain, `shutdown_agents`, the stop signal, `shutdown()`.
//
// Port of python `kernel/_kernel.py` (`draining`, `shutdown_agents`,
// `shutdown`, `SHUTDOWN_VERB_TIMEOUT`, `QUIET_TIMEOUT`) and `kernel/_agent.py`
// (`Agent.shutdown`, the depth-first hook walk); rust
// `fantastic-kernel/src/kernel.rs` (`shutdown_agents`, `request_shutdown`).
//
// A graceful stop has TWO halves. `shutdownAgents` stops the AGENTS and keeps
// the kernel serving (reads, the flusher); `shutdown` stops the PROCESS: it
// drains, every call settles (done, or cancelled into the dead letters), the
// last records and execution rows land, then it walks the tree calling each
// bundle's teardown hook — without touching records, disk, or tree membership,
// so the next boot rehydrates the same tree.

import FantasticJSON
import Foundation

/// Hooks run at the very end of `Kernel.shutdown()`, for process-wide state
/// keyed by `Kernel.serial` that the kernel itself cannot see (it lives in
/// modules that depend on the kernel, never the other way round).
private let teardownHookLock = NSLock()
nonisolated(unsafe) private var _teardownHooks: [@Sendable (Kernel) -> Void] = []

extension Kernel {
    /// Register a hook to drop THIS kernel's rows from a process-wide side table
    /// when it shuts down. Idempotent registration is the caller's job.
    public static func onTeardown(_ hook: @escaping @Sendable (Kernel) -> Void) {
        teardownHookLock.lock()
        defer { teardownHookLock.unlock() }
        _teardownHooks.append(hook)
    }

    static func teardownHooks() -> [@Sendable (Kernel) -> Void] {
        teardownHookLock.lock()
        defer { teardownHookLock.unlock() }
        return _teardownHooks
    }

    /// One bound for every agent's `shutdown` pass. Python
    /// `Kernel.SHUTDOWN_VERB_TIMEOUT`.
    public static let SHUTDOWN_VERB_TIMEOUT: Double = 30.0
    /// How long in-flight work may take to settle before it is cancelled.
    /// Python `Kernel.QUIET_TIMEOUT`.
    public static let QUIET_TIMEOUT: Double = 10.0
    /// How long a cancelled batch may take to record its ending. Python
    /// `Kernel.SETTLE_TIMEOUT`.
    public static let SETTLE_TIMEOUT: Double = 2.0

    /// Is the kernel draining? New ROOT sends are refused while it is.
    public var draining: Bool {
        lifecycleLock.lock()
        defer { lifecycleLock.unlock() }
        return _draining
    }

    func setDraining(_ on: Bool) {
        lifecycleLock.lock()
        _draining = on
        lifecycleLock.unlock()
    }

    /// `shutdown()` has run to the end. Python `_shutdown_complete`.
    public var shutdownComplete: Bool {
        lifecycleLock.lock()
        defer { lifecycleLock.unlock() }
        return _shutdownComplete
    }

    // ── the stop signal ─────────────────────────────────────────

    /// Signal the daemon serve loop to begin a graceful shutdown. Called by
    /// the root-only `kernel_stop` verb AFTER it has produced its ack, so
    /// the reply flushes over the web surface before the process exits.
    /// Idempotent — extra calls collapse to one shutdown.
    public func requestShutdown() {
        lifecycleLock.lock()
        if _shutdownRequested {
            lifecycleLock.unlock()
            return
        }
        _shutdownRequested = true
        let waiters = _shutdownWaiters
        _shutdownWaiters = []
        lifecycleLock.unlock()
        // `take()` is the one-shot: a waiter whose task was cancelled first has
        // already resumed itself and hands back nil here.
        for w in waiters { w.take()?.resume() }
    }

    /// Resolves once `requestShutdown` has been called — at once if it already
    /// was. The daemon awaits this beside SIGINT/SIGTERM/SIGHUP; when it fires,
    /// the daemon runs `shutdown()`, releases the lock and exits 0.
    ///
    /// CANCELLATION-AWARE. It parked on a bare `withCheckedContinuation`, so the
    /// LOSER of that race — this leg, whenever a signal won — was never resumed
    /// and `cancelAll()` waited on it forever: the daemon printed nothing on
    /// SIGTERM/SIGINT, never tore down, and had to be SIGKILLed with `lock.yaml`
    /// left behind. A cancelled wait now resolves exactly like a fired one; the
    /// caller is already past the point where it cares which.
    public func shutdownRequested() async {
        let slot = ShutdownWaitSlot()
        await withTaskCancellationHandler {
            await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
                lifecycleLock.lock()
                if _shutdownRequested {
                    lifecycleLock.unlock()
                    c.resume()
                    return
                }
                slot.store(c)
                _shutdownWaiters.append(slot)
                lifecycleLock.unlock()
                // Cancelled before the continuation was stored: honour it now.
                if Task.isCancelled { slot.take()?.resume() }
            }
        } onCancel: {
            slot.take()?.resume()
        }
    }

    /// `runHandler`, bounded: true when the handler COMPLETED within `seconds`.
    /// On expiry the handler's task is cancelled and the wait ends at once — a
    /// handler that ignores cancellation is not waited on.
    func runHandlerBounded(_ target: Agent, _ payload: JSON, seconds: Double) async -> Bool {
        let box = DispatchBox()
        let work = Task {
            let (_, status) = await self.runHandler(target, payload)
            box.resolve(.bool(status == .completed))
        }
        let timer = Task {
            try? await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
            if Task.isCancelled { return }
            work.cancel()
            box.resolve(nil)
        }
        let done = await box.wait()
        timer.cancel()
        return done == .bool(true)
    }

    // ── shutdown_agents ─────────────────────────────────────────

    /// THE AGENT HALF OF A GRACEFUL STOP — also the `shutdown_agents` kernel
    /// verb.
    ///
    /// 1. DRAIN: new roots are refused; sends with a live parent task still pass.
    /// 2. `shutdown` VERB: every agent whose bundle ADVERTISES it is told, all at
    ///    once, under the shutdown scope, bounded. For an AI it is a message ("the
    ///    kernel is stopping; persist what matters, then done"); its tools work.
    /// 3. QUIET: wait for the in-flight DISPATCHES to settle (persister loops
    ///    marked `keepalive` and `spawnTask` service loops excepted), bounded;
    ///    the rest is cancelled through the normal `cancelTask` path (subtree,
    ///    `onCancel` hooks).
    ///
    /// Idempotent: a second call answers the same shape and shuts nothing new.
    /// The kernel keeps serving afterwards — reads, the flusher — until
    /// `shutdown()` tears it down.
    ///
    /// WHO GETS TOLD: every agent whose bundle DECLARES `shutdown` in its
    /// static `verbs` table — python's `advertises("shutdown")`, read with NO
    /// send: no reflect call row, no task row, no watcher echo. A bundle that
    /// merely ACCEPTS the verb as a lifecycle no-op is not asking to be told.
    public func shutdownAgents() async -> JSON {
        setDraining(true)
        // python `[a for a in self.agents.values() if a.advertises("shutdown")]`
        // — the bundle's static table, no dispatch of any kind.
        let targets: [AgentId] = allAgents().compactMap { a in
            guard let hm = a.handlerModule, let b = bundles.get(hm),
                b.verbs.contains("shutdown")
            else { return nil }
            return a.id
        }
        var shut: [String] = []
        if !targets.isEmpty {
            // THE KERNEL'S OWN BOOKKEEPING: each advertiser's handler runs
            // directly (`runHandler`), as itself, concurrently, bounded by
            // `SHUTDOWN_VERB_TIMEOUT` — never a send against anyone's grant.
            // EXEMPT FROM THE DRAIN GATE: this IS the shutdown, so what those
            // handlers send passes it.
            let ok: [Bool] = await withTaskGroup(of: (Int, Bool).self) { group in
                for (i, id) in targets.enumerated() {
                    guard let a = agent(id) else { continue }
                    group.addTask {
                        let done = await KernelTaskLocals.withShutdown {
                            await self.runHandlerBounded(
                                a, .object(["type": .string("shutdown")]),
                                seconds: Kernel.SHUTDOWN_VERB_TIMEOUT)
                        }
                        return (i, done)
                    }
                }
                var out = [Bool](repeating: false, count: targets.count)
                for await (i, done) in group { out[i] = done }
                return out
            }
            for (id, done) in zip(targets, ok) where done {
                shut.append(id.value)
            }
        }
        // The dispatch running THIS verb is in the table too; it is not pending
        // work.
        let me = KernelTaskLocals.currentTask
        let deadline = Date().addingTimeInterval(quietTimeout)
        // SETTLING, not PENDING: a fork never settles by construction, so
        // waiting on one burned the whole quiet timeout on every daemon with a
        // scheduler / web / bridge loop on the tree (python, 2026-09-21:
        // SIGTERM→exit 10.25 s with a scheduler, 0.29 s without). The cancel
        // loop below still takes every non-keepalive root, forks included.
        while Date() < deadline && !tasks.settling(except: me).isEmpty {
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
        var cancelled: [Int64] = []
        for tid in tasks.pending(except: me) {
            // ROOTS ONLY: the subtree dies with its root, so cancelling a child
            // here would race the parent's own teardown.
            guard tasks.isRoot(tid) else { continue }
            let out = await cancelTask(tid)
            cancelled += (out["killed"].asArray ?? []).compactMap { $0.asInt }
        }
        // EVERY CALL SETTLES before this returns. A killed dispatch's batch
        // records its `cancelled` outcome (and its settle row) a few scheduling
        // passes after the abort; the stop that follows flushes the last rows, so
        // it must not start before those endings exist. Bounded: a batch that
        // never settles is not waited on forever.
        let settleBy = Date().addingTimeInterval(Kernel.SETTLE_TIMEOUT)
        while Date() < settleBy
            && ledger.inFlightIds().contains(where: { !tasks.contains(TaskId($0)) })
        {
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        return .object([
            "drained": .bool(true),
            "shut": .array(shut.map { .string($0) }),
            "cancelled": .array(cancelled.map { .integer($0) }),
        ])
    }

    // ── shutdown ────────────────────────────────────────────────

    /// Graceful tree-wide process-shutdown: `shutdownAgents()` (drain, the
    /// `shutdown` verb, quiet, every call settled), then each bundle's
    /// `flushPending` (the loader's record writer) and the kernel's FINAL
    /// EXECUTION FLUSH — both awaited BEFORE the teardown walk, while the
    /// provider and the memory store are still alive — then the depth-first
    /// `onShutdown` walk that frees OS resources (PTYs, subprocesses,
    /// sockets) without touching records or disk. The tree survives so the
    /// next boot rehydrates cleanly. Idempotent: guarded by `shutdownComplete`,
    /// so the signal handler and any safety net don't double-run.
    public func shutdown() async {
        guard !shutdownComplete, let root = root else { return }
        _ = await shutdownAgents()
        // THE LOADER'S OWN HOOK, before the walk and while its provider child is
        // still alive: the root's `kernel_state` bundle lands the last records
        // here (python `flush_pending`). Its writes are the kernel's own
        // traffic. Every loaded agent is offered it — a bundle with nothing to
        // land has a no-op default.
        await KernelTaskLocals.withShutdown {
            for a in self.allAgents() {
                guard let hm = a.handlerModule, let b = self.bundles.get(hm) else { continue }
                do {
                    // the kernel running the agent's own hook: AS the agent
                    try await self.asSelf(a.id) {
                        try await b.flushPending(agentId: a.id, kernel: self)
                    }
                } catch {
                    // C87: python's line carries NO agent id
                    // (`print(f"  [shutdown] flush_pending raised: {e}",
                    // file=sys.stderr)`) — it is called on the root alone
                    // there; kept id-less here too even though this loop
                    // offers the hook more broadly.
                    RuntimeLog.warn("  [shutdown] flush_pending raised: \(pyErrorText(error))")
                }
            }
        }
        if started {
            // THE LAST EXECUTION ROWS, while the store is still alive: everything
            // settled by the drain (done, or cancelled into the dead letters)
            // lands. Then the flush loop stops.
            _ = await KernelTaskLocals.withShutdown { await self.flushExecutions() }
            await stopFlusher()
        }
        await shutdownWalk(root)
        // PROCESS-WIDE SIDE TABLES KEYED BY KERNEL IDENTITY (the AI core's
        // incarnation counters and per-agent run states) are dropped here. Swift
        // hosts many kernels in one process, so those maps grew one entry per
        // kernel that ever existed and nothing ever removed them — the kernel
        // died, its rows did not.
        for hook in Kernel.teardownHooks() { hook(self) }
        markShutdownComplete()
    }

    /// Sync-scoped: `NSLock` is refused inside an async frame.
    private func markShutdownComplete() {
        lifecycleLock.lock()
        _shutdownComplete = true
        lifecycleLock.unlock()
    }

    /// Python `Agent.shutdown`: depth-first like cascade-delete, but DOES NOT
    /// touch records, disk, or tree membership — only invokes each bundle's
    /// teardown hook. Children first, then this agent's `onShutdown` — ONLY
    /// that hook, never `onDelete` in its place (audit c9). A hook that throws
    /// is printed and the walk continues: one bundle's teardown cannot take
    /// down the rest.
    ///
    /// C87 residual (not reproduced, by construction): python additionally
    /// catches around the RECURSIVE `await child.shutdown()`
    /// (`  [shutdown] <cid> raised: <e>`). This function is non-throwing (its
    /// one fallible step, the hook call, has its own catch below), so nothing
    /// propagates OUT of the recursive call to catch here.
    private func shutdownWalk(_ node: Agent) async {
        for cid in node.childIds() {
            if let child = agent(cid) {
                await shutdownWalk(child)
            }
        }
        guard let hm = node.handlerModule else { return }
        // A record whose bundle this runtime does not have — `create_agent`
        // accepts one at runtime — is python's failed import: SAID on stderr
        // with python's `ModuleNotFoundError` text, and the walk goes on.
        guard let bundle = bundles.get(hm) else {
            if let missing = bundles.moduleNotFound(hm) {
                RuntimeLog.warn(
                    "  [shutdown] \(node.id.value) import \(pyRepr(hm)) raised: \(missing)")
            }
            return
        }
        do {
            // the kernel running the agent's own hook: AS the agent
            try await asSelf(node.id) {
                try await bundle.onShutdown(agentId: node.id, kernel: self)
            }
        } catch {
            FileHandle.standardError.write(
                "  [shutdown] \(node.id.value) hook raised: \(pyErrorText(error))\n"
                    .data(using: .utf8) ?? Data())
        }
    }
}

/// One-slot holder for a `shutdownRequested()` waiter, so the stop signal and
/// the task's own cancellation can hand the same continuation to exactly one
/// resumer. Resuming a `CheckedContinuation` twice is a crash, and never
/// resuming it is the hang this exists to prevent.
final class ShutdownWaitSlot: @unchecked Sendable {
    private let lock = NSLock()
    private var cont: CheckedContinuation<Void, Never>?

    func store(_ c: CheckedContinuation<Void, Never>) {
        lock.lock()
        defer { lock.unlock() }
        cont = c
    }

    func take() -> CheckedContinuation<Void, Never>? {
        lock.lock()
        defer { lock.unlock() }
        let c = cont
        cont = nil
        return c
    }
}
