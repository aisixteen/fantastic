// Workdir bootstrap + daemon mode helpers.
//
// Mirrors Rust's `fantastic-kernel/src/bootstrap.rs`.
//
//   - `Bootstrap.daemon(workdir:)` — acquires lock, hydrates from
//     state.json if present, returns a kernel ready to run forever
//   - `Bootstrap.oneShot(workdir:)` — no lock; for `fantastic <id>
//     <verb>` style one-off RPC against an existing workdir
//   - `runUntilSignal(_:)` — daemon-mode helper that boots all loaded
//     agents, then blocks until SIGINT/SIGTERM, then shuts down

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
        let stateFile = url.appendingPathComponent(".fantastic/state.json")
        if let data = try? Data(contentsOf: stateFile),
            let json = String(data: data, encoding: .utf8)
        {
            try? kernel.load(json: json)
        }
        return kernel
    }

    /// Run a kernel forever in daemon mode. Boots every loaded agent,
    /// installs SIGINT/SIGTERM handlers, awaits a signal, then
    /// shuts the kernel down gracefully + releases the lock.
    public static func runUntilSignal(
        kernel: Kernel,
        lock: WorkdirLock
    ) async {
        // Boot every loaded agent.
        for agent in kernel.allAgents() {
            _ = await kernel.send(agent.id, .object(["type": .string("boot")]))
        }

        FileHandle.standardError.write(
            "fantastic: daemon up. \(kernel.allAgents().count) agent(s) loaded. Ctrl-C to stop.\n"
                .data(using: .utf8) ?? Data())

        // Wait for a signal.
        await waitForSignal()

        FileHandle.standardError.write(
            "fantastic: shutting down...\n".data(using: .utf8) ?? Data())

        // Reverse order so children stop before parents.
        for agent in kernel.allAgents().reversed() {
            _ = await kernel.send(agent.id, .object(["type": .string("shutdown")]))
        }
        kernel.shutdown()
        lock.release()
    }

    private static func waitForSignal() async {
        let signalSource = DispatchSource.makeSignalSource(signal: SIGINT, queue: .global())
        signal(SIGINT, SIG_IGN)
        let termSource = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .global())
        signal(SIGTERM, SIG_IGN)

        await withCheckedContinuation { (cont: CheckedContinuation<Void, Never>) in
            let resumed = Atomic(false)
            signalSource.setEventHandler {
                if resumed.compareAndSwap(false, true) {
                    cont.resume()
                }
            }
            termSource.setEventHandler {
                if resumed.compareAndSwap(false, true) {
                    cont.resume()
                }
            }
            signalSource.resume()
            termSource.resume()
        }
        signalSource.cancel()
        termSource.cancel()
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
