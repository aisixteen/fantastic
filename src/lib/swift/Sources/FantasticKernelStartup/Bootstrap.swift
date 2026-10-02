// Workdir bootstrap + daemon mode helpers.
//
// Mirrors Rust's `fantastic-kernel/src/bootstrap.rs`.
//
//   - `Bootstrap.daemon(workdir:)` — acquires lock, hydrates the tree from
//     its `agent.yaml` records, returns a kernel ready to run forever
//   - `Bootstrap.oneShot(workdir:)` — no lock; for `fantastic <id>
//     <verb>` style one-off RPC against an existing workdir
//   - `Bootstrap.awaitStop(kernel:signals:)` — blocks until SIGINT/SIGTERM/SIGHUP or
//     a root-only `kernel_stop`; the real daemon entrypoint (`Fantastic/
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
    /// workdir lock; throws if another process already owns it. The tree
    /// comes ONLY from the `agent.yaml` records (`startKernel`) — python reads
    /// no snapshot file at boot.
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

        // The kernel keeps no conversation and hydrates no rows: `load` restores
        // the tree and the id counter (the root record's reserved bound);
        // execution rows live in the root's memory store, which `kernel_start`
        // wires.
        let kernel = try await startKernel(workdir: workdir, portHint: portHint)
        return (kernel, lock)
    }

    /// Boot a one-shot kernel against a workdir WITHOUT acquiring the
    /// lock. Used by `fantastic reflect [<id>]` and
    /// `fantastic <id> <verb>` so a running daemon can co-exist with
    /// one-off probes. Hydrates the tree from its `agent.yaml` records.
    public static func oneShot(
        workdir: String
    ) async throws -> Kernel {
        try await startKernel(workdir: workdir, portHint: 0)
    }

    /// Block until an OS stop signal (SIGINT/SIGTERM/SIGHUP) arrives OR the
    /// kernel's own `requestShutdown` fires (the root-only `kernel_stop`
    /// verb, which deferred its trigger 100 ms so its ack flushed first). The
    /// two are raced; whichever loses is cancelled. Python `longrun` installs
    /// the same three handlers on one stop event that `kernel_stop` shares.
    ///
    /// `signals` was INSTALLED by the caller before it announced itself
    /// (`[kernel] up`), so a signal that landed before this wait started is
    /// HELD and returned here, not lost to the default action.
    ///
    /// Returns the SIGNAL's name (`"SIGINT"`/`"SIGTERM"`/`"SIGHUP"`) when that
    /// leg won, or `nil` for `kernel_stop`/cancellation — the caller
    /// prints python's `\n[kernel] <SIG> — shutting down...` line ONLY for a
    /// real signal (python's `_request_stop` runs only from
    /// `add_signal_handler`; `kernel_stop` sets the same event directly,
    /// with no such print).
    ///
    /// BOTH LEGS ARE CANCELLATION-AWARE. `shutdownRequested()` parked on a
    /// `withCheckedContinuation` with no cancellation handler, so `cancelAll`
    /// never returned: a SIGTERM printed nothing, the teardown lines never ran,
    /// `kernel.shutdown()` never ran, and the process was still alive 15 s later
    /// with `lock.yaml` left behind (measured; the e2e driver SIGKILLed swift).
    public static func awaitStop(kernel: Kernel, signals: StopSignals) async -> String? {
        await withTaskGroup(of: String?.self) { group -> String? in
            group.addTask { await signals.recv() }
            group.addTask {
                await kernel.shutdownRequested()
                return nil
            }
            let caught = await group.next()
            group.cancelAll()
            return caught ?? nil
        }
    }
}

/// The daemon's stop signals — SIGINT, SIGTERM and SIGHUP, the three python's
/// `_request_stop` handles. INSTALLED by `install()`: from the moment it
/// returns, a delivered signal is HELD until `recv()` takes it, however late
/// that await starts. The daemon installs before it prints `[kernel] up` —
/// whoever reads that line may signal at once, and a SIGTERM that landed while
/// the handlers were only set up inside the wait took the default action: the
/// process died with no teardown and `lock.yaml` held (rust closed the same
/// window: `StopSignals::install`).
///
/// The handlers stay for the life of the object: a second signal during the
/// teardown is ignored, as python's `_request_stop` ignores it once stop is set.
public final class StopSignals: @unchecked Sendable {
    private static let names: [(Int32, String)] = [
        (SIGINT, "SIGINT"), (SIGTERM, "SIGTERM"), (SIGHUP, "SIGHUP"),
    ]
    private let lock = NSLock()
    private var held: String?
    private var waiter: CheckedContinuation<String?, Never>?
    private var sources: [DispatchSourceSignal] = []

    private init() {}

    /// Ignore the three signals' default action and watch each with a dispatch
    /// source, resumed here — the watch is live when this returns.
    public static func install() -> StopSignals {
        let stop = StopSignals()
        for (sig, name) in names {
            signal(sig, SIG_IGN)
            let src = DispatchSource.makeSignalSource(signal: sig, queue: .global())
            src.setEventHandler { [weak stop] in stop?.deliver(name) }
            src.resume()
            stop.sources.append(src)
        }
        return stop
    }

    deinit {
        for src in sources { src.cancel() }
    }

    /// The first signal wins: it goes to a parked `recv`, or is held for the
    /// next one. Later signals are dropped.
    private func deliver(_ name: String) {
        let cont: CheckedContinuation<String?, Never>? = lock.withLock {
            if let w = waiter {
                waiter = nil
                return w
            }
            if held == nil { held = name }
            return nil
        }
        cont?.resume(returning: name)
    }

    /// The held signal's name, or the next one to arrive — `nil` on
    /// cancellation, so a serve loop that ends another way (`kernel_stop`)
    /// does not hang on a continuation nobody will ever resume.
    public func recv() async -> String? {
        await withTaskCancellationHandler {
            await withCheckedContinuation { (cont: CheckedContinuation<String?, Never>) in
                let now: String?? = lock.withLock {
                    if let h = held {
                        held = nil
                        return .some(h)
                    }
                    if Task.isCancelled { return .some(nil) }
                    waiter = cont
                    return nil
                }
                if let now { cont.resume(returning: now) }
            }
        } onCancel: {
            let cont = lock.withLock { () -> CheckedContinuation<String?, Never>? in
                let w = waiter
                waiter = nil
                return w
            }
            cont?.resume(returning: nil)
        }
    }
}
