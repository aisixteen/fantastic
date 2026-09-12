// Workdir bootstrap + daemon mode helpers.
//
// Mirrors Rust's `fantastic-kernel/src/bootstrap.rs`.
//
//   - `Bootstrap.daemon(workdir:)` — acquires lock, hydrates from
//     state.json if present, returns a kernel ready to run forever
//   - `Bootstrap.oneShot(workdir:)` — no lock; for `fantastic <id>
//     <verb>` style one-off RPC against an existing workdir
//   - `runUntilSignal(_:)` — daemon-mode helper that boots all loaded
//     agents, then blocks until SIGINT/SIGTERM/SIGHUP or a root-only
//     `shutdown_kernel`, then runs the kernel's graceful `shutdown()`

import FantasticJSON
import FantasticKernel
import Foundation

#if canImport(Darwin)
    import Darwin
#endif

public enum Bootstrap {
    /// Bootstrap a disk-backed kernel for daemon mode. Acquires the
    /// workdir lock; throws if another process already owns it.
    /// Hydrates from `<workdir>/.fantastic/state.json` if present.
    public static func daemon(
        workdir: String,
        portHint: UInt16 = 0
    ) async throws -> (kernel: Kernel, lock: WorkdirLock) {
        let url = URL(fileURLWithPath: workdir, isDirectory: true)
        let fm = FileManager.default
        var isDir: ObjCBool = false
        guard fm.fileExists(atPath: url.path, isDirectory: &isDir), isDir.boolValue else {
            throw KernelStartupError.workdirInvalid(workdir)
        }
        let lock = WorkdirLock(workdir: url)
        do {
            try lock.acquire()
        } catch WorkdirLock.LockError.alreadyRunning(let pid, _) {
            throw KernelStartupError.internalError(
                "already running (pid \(pid)) in \(workdir)")
        }

        let kernel = try await startKernel(workdir: workdir, portHint: portHint)

        // THE MESSAGE STORE, hydrated COLD — before any agent exists, from
        // above (python `hydrate_messages` in the bootstrap). A file of another
        // version or a cut row refuses the boot rather than hydrating half a
        // conversation.
        try hydrateMessages(kernel, rootDir: url.appendingPathComponent(".fantastic"))

        // Hydrate state from disk if present.
        let stateFile = url.appendingPathComponent(".fantastic/state.json")
        if let data = try? Data(contentsOf: stateFile),
            let json = String(data: data, encoding: .utf8)
        {
            do {
                try kernel.load(json: json)
            } catch {
                // Weak-load: log + continue. Don't abort daemon boot
                // on a half-corrupted snapshot.
                FileHandle.standardError.write(
                    "fantastic: snapshot load failed (\(error)); booting empty\n"
                        .data(using: .utf8) ?? Data())
            }
        }

        return (kernel, lock)
    }

    /// Boot a one-shot kernel against a workdir WITHOUT acquiring the
    /// lock. Used by `fantastic reflect [<id>]` and
    /// `fantastic <id> <verb>` so a running daemon can co-exist with
    /// one-off probes. Hydrates from state.json (read-only).
    public static func oneShot(
        workdir: String
    ) async throws -> Kernel {
        let url = URL(fileURLWithPath: workdir, isDirectory: true)
        let kernel = try await startKernel(workdir: workdir, portHint: 0)
        try hydrateMessages(kernel, rootDir: url.appendingPathComponent(".fantastic"))
        let stateFile = url.appendingPathComponent(".fantastic/state.json")
        if let data = try? Data(contentsOf: stateFile),
            let json = String(data: data, encoding: .utf8)
        {
            try? kernel.load(json: json)
        }
        // THE DURABLE LEDGER. `installLedgerPersistence` had no production
        // caller in swift, so every settled execution was lost on exit and a
        // reboot started with an empty ledger — dedup and join silently stopped
        // holding across restarts (measured 2026-09-02).
        await kernel.installLedgerPersistence()
        return kernel
    }

    /// Run a kernel forever in daemon mode. Boots every loaded agent,
    /// installs SIGINT/SIGTERM/SIGHUP handlers, awaits a signal OR the
    /// kernel's own stop request (`shutdown_kernel`), then runs the graceful
    /// `Kernel.shutdown()` — drain, the last message window, the depth-first
    /// hook walk — and releases the lock.
    public static func runUntilSignal(
        kernel: Kernel,
        lock: WorkdirLock
    ) async {
        // Boot every loaded agent.
        for agent in kernel.allAgents() {
            _ = await kernel.send([Item(agent.id, .object(["type": .string("boot")]))])[0].result
        }
        // AFTER the boot loop: the store agent must be live before the ledger
        // can rehydrate through it or install its flusher.
        await kernel.installLedgerPersistence()
        // THE PUMP SUBSCRIBES HERE: after hydration (cold, above) and the
        // ledger, at the highest id present. `subscribe(after:)` replays what
        // the boot loop minted meanwhile, so nothing is lost.
        kernel.installMessagePump(after: kernel.rows.nextId - 1)

        FileHandle.standardError.write(
            "fantastic: daemon up. \(kernel.allAgents().count) agent(s) loaded. Ctrl-C to stop.\n"
                .data(using: .utf8) ?? Data())

        await awaitStop(kernel: kernel)

        FileHandle.standardError.write(
            "fantastic: shutting down...\n".data(using: .utf8) ?? Data())
        // Drain, land the last message window (provider still alive), then the
        // depth-first `onShutdown`/`onDelete` walk. Records and tree untouched.
        await kernel.shutdown()
        lock.release()
    }

    /// Block until an OS stop signal (SIGINT/SIGTERM/SIGHUP) arrives OR the
    /// kernel's own `requestShutdown` fires (the root-only `shutdown_kernel`
    /// verb, which deferred its trigger 100 ms so its ack flushed first). The
    /// two are raced; whichever loses is cancelled. Python `longrun` installs
    /// the same three handlers on one stop event that `shutdown_kernel` shares.
    public static func awaitStop(kernel: Kernel) async {
        await withTaskGroup(of: Void.self) { group in
            group.addTask { await waitForSignal() }
            group.addTask { await kernel.shutdownRequested() }
            await group.next()
            group.cancelAll()
        }
    }

    /// Resolve on the first of SIGINT/SIGTERM/SIGHUP — or on cancellation, so
    /// a serve loop that ends another way (`shutdown_kernel`) does not hang on
    /// a continuation nobody will ever resume.
    private static func waitForSignal() async {
        let sources = [SIGINT, SIGTERM, SIGHUP].map { sig -> DispatchSourceSignal in
            let src = DispatchSource.makeSignalSource(signal: sig, queue: .global())
            signal(sig, SIG_IGN)
            return src
        }
        let resumed = Atomic(false)
        let slot = ContinuationSlot()
        await withTaskCancellationHandler {
            await withCheckedContinuation { (cont: CheckedContinuation<Void, Never>) in
                slot.store(cont)
                for src in sources {
                    src.setEventHandler {
                        if resumed.compareAndSwap(false, true) {
                            slot.take()?.resume()
                        }
                    }
                    src.resume()
                }
                // Cancelled before the continuation was stored: honour it now.
                if Task.isCancelled, resumed.compareAndSwap(false, true) {
                    slot.take()?.resume()
                }
            }
        } onCancel: {
            if resumed.compareAndSwap(false, true) {
                slot.take()?.resume()
            }
        }
        for src in sources { src.cancel() }
    }
}

/// One-slot holder so the signal handlers and the cancellation handler can
/// hand the same continuation to exactly one resumer.
private final class ContinuationSlot: @unchecked Sendable {
    private let lock = NSLock()
    private var cont: CheckedContinuation<Void, Never>?

    func store(_ c: CheckedContinuation<Void, Never>) {
        lock.lock(); defer { lock.unlock() }
        cont = c
    }

    func take() -> CheckedContinuation<Void, Never>? {
        lock.lock(); defer { lock.unlock() }
        let c = cont
        cont = nil
        return c
    }
}

/// Minimal compare-and-swap helper for the signal-resume guard.
/// (We avoid Swift Atomics package dependency for this single
/// use case; an NSLock-protected Bool is plenty.)
private final class Atomic<T: Equatable>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T

    init(_ value: T) { self.value = value }

    func compareAndSwap(_ expected: T, _ new: T) -> Bool {
        lock.lock(); defer { lock.unlock() }
        if value == expected {
            value = new
            return true
        }
        return false
    }
}
