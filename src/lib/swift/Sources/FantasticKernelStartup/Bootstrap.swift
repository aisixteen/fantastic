// Workdir bootstrap + daemon mode helpers.
//
// Mirrors Rust's `fantastic-kernel/src/bootstrap.rs`.
//
//   - `Bootstrap.daemon(workdir:)` — acquires lock, hydrates from
//     state.json if present, returns a kernel ready to run forever
//   - `Bootstrap.oneShot(workdir:)` — no lock; for `fantastic <id>
//     <verb>` style one-off RPC against an existing workdir
//   - `Bootstrap.awaitStop(kernel:)` — blocks until SIGINT/SIGTERM/SIGHUP or
//     a root-only `shutdown_kernel`; the real daemon entrypoint (`Fantastic/
//     Daemon.swift`) races it against the REPL/block tasks and runs the
//     kernel's graceful `shutdown()` itself.

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
        return kernel
    }

    /// Block until an OS stop signal (SIGINT/SIGTERM/SIGHUP) arrives OR the
    /// kernel's own `requestShutdown` fires (the root-only `shutdown_kernel`
    /// verb, which deferred its trigger 100 ms so its ack flushed first). The
    /// two are raced; whichever loses is cancelled. Python `longrun` installs
    /// the same three handlers on one stop event that `shutdown_kernel` shares.
    ///
    /// Returns the SIGNAL's name (`"SIGINT"`/`"SIGTERM"`/`"SIGHUP"`) when that
    /// leg won, or `nil` for `shutdown_kernel`/cancellation — the caller
    /// prints python's `\n[kernel] <SIG> — shutting down...` line ONLY for a
    /// real signal (python's `_request_stop` runs only from
    /// `add_signal_handler`; `shutdown_kernel` sets the same event directly,
    /// with no such print).
    ///
    /// BOTH LEGS ARE CANCELLATION-AWARE. `shutdownRequested()` parked on a
    /// `withCheckedContinuation` with no cancellation handler, so `cancelAll`
    /// never returned: a SIGTERM printed nothing, the teardown lines never ran,
    /// `kernel.shutdown()` never ran, and the process was still alive 15 s later
    /// with `lock.json` left behind (measured; the e2e driver SIGKILLed swift).
    public static func awaitStop(kernel: Kernel) async -> String? {
        await withTaskGroup(of: String?.self) { group -> String? in
            group.addTask { await waitForSignal() }
            group.addTask {
                await kernel.shutdownRequested()
                return nil
            }
            let caught = await group.next()
            group.cancelAll()
            return caught ?? nil
        }
    }

    /// Resolve with the caught signal's name on the first of SIGINT/SIGTERM/
    /// SIGHUP — or `nil` on cancellation, so a serve loop that ends another
    /// way (`shutdown_kernel`) does not hang on a continuation nobody will
    /// ever resume.
    private static func waitForSignal() async -> String? {
        let names: [Int32: String] = [SIGINT: "SIGINT", SIGTERM: "SIGTERM", SIGHUP: "SIGHUP"]
        let sigs = [SIGINT, SIGTERM, SIGHUP]
        let sources = sigs.map { sig -> DispatchSourceSignal in
            let src = DispatchSource.makeSignalSource(signal: sig, queue: .global())
            signal(sig, SIG_IGN)
            return src
        }
        let resumed = Atomic(false)
        let slot = ContinuationSlot()
        let caught = await withTaskCancellationHandler {
            await withCheckedContinuation { (cont: CheckedContinuation<String?, Never>) in
                slot.store(cont)
                for (sig, src) in zip(sigs, sources) {
                    src.setEventHandler {
                        if resumed.compareAndSwap(false, true) {
                            slot.take()?.resume(returning: names[sig])
                        }
                    }
                    src.resume()
                }
                // Cancelled before the continuation was stored: honour it now.
                if Task.isCancelled, resumed.compareAndSwap(false, true) {
                    slot.take()?.resume(returning: nil)
                }
            }
        } onCancel: {
            if resumed.compareAndSwap(false, true) {
                slot.take()?.resume(returning: nil)
            }
        }
        for src in sources { src.cancel() }
        return caught
    }
}

/// One-slot holder so the signal handlers and the cancellation handler can
/// hand the same continuation to exactly one resumer.
private final class ContinuationSlot: @unchecked Sendable {
    private let lock = NSLock()
    private var cont: CheckedContinuation<String?, Never>?

    func store(_ c: CheckedContinuation<String?, Never>) {
        lock.lock()
        defer { lock.unlock() }
        cont = c
    }

    func take() -> CheckedContinuation<String?, Never>? {
        lock.lock()
        defer { lock.unlock() }
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
        lock.lock()
        defer { lock.unlock() }
        if value == expected {
            value = new
            return true
        }
        return false
    }
}
