// KERNEL LIFECYCLE — drain, `shutdown_agents`, the stop signal, `shutdown()`.
//
// Port of python `kernel/_kernel.py` (`draining`, `shutdown_agents`,
// `shutdown`, `SHUTDOWN_VERB_TIMEOUT`, `QUIET_TIMEOUT`) and `kernel/_agent.py`
// (`Agent.shutdown`, the depth-first hook walk); rust
// `fantastic-kernel/src/kernel.rs` (`shutdown_agents`, `request_shutdown`).
//
// A graceful stop has TWO halves. `shutdownAgents` stops the AGENTS and keeps
// the kernel serving (reads, `messages`, the persister); `shutdown` stops the
// PROCESS: it drains, lands the last message window, then walks the tree
// calling each bundle's teardown hook — without touching records, disk, or
// tree membership, so the next boot rehydrates the same tree.

import FantasticJSON
import Foundation

extension Kernel {
    /// One bound for every agent's `shutdown` turn. Python
    /// `Kernel.SHUTDOWN_VERB_TIMEOUT`.
    public static let SHUTDOWN_VERB_TIMEOUT: Double = 30.0
    /// How long in-flight work may take to settle before it is cancelled.
    /// Python `Kernel.QUIET_TIMEOUT`.
    public static let QUIET_TIMEOUT: Double = 10.0

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
    /// the root-only `shutdown_kernel` verb AFTER it has produced its ack, so
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
        for w in waiters { w.resume() }
    }

    /// Resolves once `requestShutdown` has been called — at once if it already
    /// was. The daemon awaits this beside SIGINT/SIGTERM/SIGHUP; when it fires,
    /// the daemon runs `shutdown()`, releases the lock and exits 0.
    public func shutdownRequested() async {
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            lifecycleLock.lock()
            if _shutdownRequested {
                lifecycleLock.unlock()
                c.resume()
                return
            }
            _shutdownWaiters.append(c)
            lifecycleLock.unlock()
        }
    }

    // ── shutdown_agents ─────────────────────────────────────────

    /// THE AGENT HALF OF A GRACEFUL STOP — also the `shutdown_agents` kernel
    /// verb.
    ///
    /// 1. DRAIN: new roots are refused; sends with a live parent task still pass.
    /// 2. `shutdown` VERB: every agent whose bundle ADVERTISES it is told, all at
    ///    once, under the shutdown scope, bounded. For an AI it is a turn ("the
    ///    kernel is stopping; persist what matters, then done"); its tools work.
    /// 3. QUIET: wait for the task table to empty (persister loops marked
    ///    `keepalive` excepted), bounded; the rest is cancelled through the
    ///    normal `cancelTask` path (subtree, `onCancel` hooks).
    ///
    /// Idempotent: a second call answers the same shape and shuts nothing new.
    /// The kernel keeps serving afterwards — reads, `messages`, the persister —
    /// until `shutdown()` tears it down.
    ///
    /// WHO GETS TOLD: every agent whose reflect `verbs` map carries `shutdown`
    /// — python's `advertises("shutdown")` over the bundle's VERBS table, read
    /// here through `reflect` because a swift bundle has no static table (rust
    /// reads it the same way). A bundle that merely ACCEPTS the verb as a
    /// lifecycle no-op is not asking to be told. The reads run as OPERATOR
    /// (python's `advertises` consults no gate) under the shutdown scope, so
    /// the drain gate lets the kernel's own traffic through.
    public func shutdownAgents() async -> JSON {
        setDraining(true)
        let all = allAgents().map { $0.id }
        var targets: [AgentId] = []
        if !all.isEmpty {
            let asks = all.map { Item($0, .object(["type": .string("reflect")])) }
            let seen = await KernelTaskLocals.$currentSender.withValue(nil) {
                await KernelTaskLocals.withShutdown { await self.send(asks) }
            }
            for (id, o) in zip(all, seen) where o.result["verbs"].asObject?["shutdown"] != nil {
                targets.append(id)
            }
        }
        var shut: [String] = []
        if !targets.isEmpty {
            let items = targets.map {
                Item($0, .object(["type": .string("shutdown")]), timeout: Kernel.SHUTDOWN_VERB_TIMEOUT)
            }
            // EXEMPT FROM THE DRAIN GATE: this IS the shutdown. Operator
            // authority, as python's `operator_context()`.
            let outs = await KernelTaskLocals.$currentSender.withValue(nil) {
                await KernelTaskLocals.withShutdown { await self.send(items) }
            }
            for (id, o) in zip(targets, outs) where o.status == .completed {
                shut.append(id.value)
            }
        }
        // The dispatch running THIS verb is in the table too; it is not pending
        // work.
        let me = KernelTaskLocals.currentTask
        let deadline = Date().addingTimeInterval(quietTimeout)
        while Date() < deadline && !tasks.pending(except: me).isEmpty {
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
        var cancelled: [Int64] = []
        for tid in tasks.pending(except: me) {
            // ROOTS ONLY: the subtree dies with its root, so cancelling a child
            // here would race the parent's own teardown.
            guard tasks.isRoot(tid) else { continue }
            let out = await KernelTaskLocals.$currentSender.withValue(nil) {
                await self.cancelTask(tid)
            }
            cancelled += (out["killed"].asArray ?? []).compactMap { $0.asInt }
        }
        return .object([
            "drained": .bool(true),
            "shut": .array(shut.map { .string($0) }),
            "cancelled": .array(cancelled.map { .integer($0) }),
        ])
    }

    // ── shutdown ────────────────────────────────────────────────

    /// Graceful tree-wide process-shutdown: `shutdownAgents()` (drain, the
    /// `shutdown` verb, quiet), then the loader's `flushPendingMessages` —
    /// awaited BEFORE the teardown walk, so the persister can write its last
    /// window while its provider child is still alive — then the depth-first
    /// `onShutdown`/`onDelete` walk that frees OS resources (PTYs, subprocesses,
    /// sockets) without touching records or disk. The tree survives so the
    /// next boot rehydrates cleanly. Idempotent: guarded by `shutdownComplete`,
    /// so the signal handler and any safety net don't double-run.
    public func shutdown() async {
        guard !shutdownComplete, let root = root else { return }
        _ = await shutdownAgents()
        // Its writes are the kernel's own traffic.
        await KernelTaskLocals.withShutdown { await self.flushPendingMessages() }
        await shutdownWalk(root)
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
    /// teardown hook. Children first, then this agent's `onShutdown` (whose
    /// protocol default IS `onDelete`, python's fallback). A hook that throws
    /// is printed and the walk continues: one bundle's teardown cannot take
    /// down the rest.
    private func shutdownWalk(_ node: Agent) async {
        for cid in node.childIds() {
            if let child = agent(cid) {
                await shutdownWalk(child)
            }
        }
        // A record whose bundle belongs to another runtime has nothing to tear
        // down here.
        guard let hm = node.handlerModule, let bundle = bundles.get(hm) else { return }
        do {
            try await bundle.onShutdown(agentId: node.id, kernel: self)
        } catch {
            FileHandle.standardError.write(
                "  [shutdown] \(node.id.value) hook raised: \(error)\n".data(using: .utf8) ?? Data())
        }
    }
}
