// No-args `fantastic` daemon mode.
//
// Mirrors `python/kernel/modes/longrun.py:_default` verbatim. Python's
// composition rule:
//
//   1. Build the kernel (read the persisted tree; NO lock yet).
//   2. Decide composition: a persisted `web.tools` agent, or `stdin.isatty()`
//      → PTY. NEITHER → return silently, no lock ever taken (A14.13).
//   3. Only then acquire `.fantastic/lock.yaml` (fail if a live process owns
//      it) and run THE START SEQUENCE (`kernel_start`): the root's memory store
//      first, then everyone. A root with no `memory_store_id` does not start —
//      fail loud, exit non-zero.
//   4. web → `_block_forever()`; tty → the REPL loop. Race those against the
//      stop signal (SIGINT/SIGTERM/SIGHUP, or the root-only `kernel_stop`
//      verb); first to finish wins and triggers `Kernel.shutdown()`.
//
// Workdir is **always cwd** — matches Python's implicit-cwd
// convention. No `<workdir>` argument; the operator picks the
// project dir by `cd`-ing into it.

import FantasticCliBundle
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation

#if canImport(Darwin)
    import Darwin
#endif

enum DaemonMode {
    /// A0.15/A14.15: web composes by `handler_module`, never by the literal id
    /// `"web"` — a renamed or aliased web agent still composes, and an
    /// unrelated agent someone happened to id `web` does not. Python:
    /// `_find_bundle_module("web", ctx=kernel)` then filters by that module;
    /// swift has no runtime package-entry-point registry to resolve a short
    /// bundle NAME the way python's plugin discovery does, so this names the
    /// one handler_module string directly — the same result python's
    /// resolution produces for the bundle this runtime ships.
    static let webHandlerModule = "web.tools"

    static func hasWebAgent(_ kernel: Kernel) -> Bool {
        kernel.allAgents().contains { $0.handlerModule == webHandlerModule }
    }

    /// Python's plain stdout marker on a non-tty daemon, once every agent has
    /// booted (`longrun.py:62`, no `file=` argument).
    static let upLine = "[kernel] up"
    /// Both STDERR (`file=sys.stderr`), bracketing `Kernel.shutdown()`
    /// (`longrun.py:124,129`).
    static let tearingDownLine = "[kernel] tearing down agents..."
    static let downLine = "[kernel] down"

    /// Python's `_request_stop` line (`longrun.py:79-83`) — printed ONLY for
    /// a real OS signal, never for `kernel_stop` (which sets the same
    /// stop event directly, with no signal name to report).
    static func stopSignalLine(_ signalName: String) -> String {
        "\n[kernel] \(signalName) — shutting down..."
    }

    /// A root that does not start (python `longrun._default`): the start
    /// sequence's refusal, raised as `RuntimeError(f"{error}\n  hint: {hint}")`
    /// and printed by `main_dispatch` as `[kernel] <e>`.
    static func startRefusedLine(_ refusal: JSON) -> String {
        var line = "[kernel] " + pyStr(refusal["error"])
        if pyTruthy(refusal["hint"]) { line += "\n  hint: " + pyStr(refusal["hint"]) }
        return line
    }

    /// Run no-args daemon mode in cwd. Returns when the daemon
    /// finishes (REPL exit, SIGINT, SIGTERM, or fatal error).
    static func run() async {
        let fm = FileManager.default
        let cwd = fm.currentDirectoryPath
        let workdirURL = URL(fileURLWithPath: cwd, isDirectory: true)

        // Step 1 (python `_build_kernel`) — build + hydrate the kernel
        // UNCONDITIONALLY, before any lock or composition decision. Python's
        // `_default(kernel)` receives an ALREADY-built kernel from
        // `main_dispatch`; only what THIS function itself does (the lock,
        // the `{type:boot}` sends) is gated on "is there anything to serve".
        let kernel: Kernel
        do {
            // The kernel keeps no conversation and hydrates no rows: `load`
            // restores the tree and the id counter; execution rows live in the
            // root's memory store, which `kernel_start` wires below.
            kernel = try await startKernel(workdir: cwd)
            // C80: applied after every kernel construction (this mode builds
            // its own, separate from `FantasticCLI.bootDiskBacked`'s).
            if FantasticCLI.ledgerCap.wasSet { kernel.ledger.setCap(FantasticCLI.ledgerCap.cap) }
            FantasticCLI.composeCliIfTTY(kernel)
        } catch {
            FileHandle.standardError.write(
                "fantastic: kernel boot failed: \(error)\n"
                    .data(using: .utf8) ?? Data())
            exit(1)
        }

        // Step 2 (A14.13/A14.15) — decide composition BEFORE the lock and
        // any boot: a persisted agent whose `handler_module` is `web.tools`
        // (never `id == "web"` — a renamed/aliased web agent still
        // composes), OR a tty. Neither ⇒ return silently, no lock ever
        // taken — matches python's `if not web_agents and not has_repl:
        // return`, which never even opens `FantasticLock()`.
        let webExists = hasWebAgent(kernel)
        // fd 0 is stdin — avoid the `stdin` C global, which Glibc exposes as a
        // mutable `var` that Swift 6 strict concurrency rejects on Linux.
        let isTTY = isatty(0) != 0
        if !webExists && !isTTY { return }

        // Step 3 — Acquire PID lock. Refuse if another live process
        // already holds it (matches Python's `FantasticLock` shape).
        let lock = WorkdirLock(workdir: workdirURL)
        do {
            try lock.acquire()
        } catch WorkdirLock.LockError.alreadyRunning(let pid, _) {
            // Python raises out of `acquire_lock` and `main_dispatch` prints
            // `[kernel] <e>` — three lines naming the pid and both ways out.
            FileHandle.standardError.write(
                (FantasticCLI.daemonLockRefusal(pid: Int(pid)) + "\n")
                    .data(using: .utf8) ?? Data())
            exit(1)
        } catch {
            FileHandle.standardError.write(
                "fantastic: lock acquisition failed: \(error)\n"
                    .data(using: .utf8) ?? Data())
            exit(1)
        }
        // A fresh root's record lands only now, under the lock.
        do { try kernel.takeSeed() } catch {
            FileHandle.standardError.write(
                "fantastic: the root record could not be written: \(error)\n"
                    .data(using: .utf8) ?? Data())
            exit(1)
        }
        defer { lock.release() }

        // FIRST PTY PUSH (tty only): orient whoever lands in the terminal —
        // human or an LLM dropped into the PTY — BEFORE boot, while the port is
        // still unknown. The text lives in the `cli` renderer bundle.
        let cli = kernel.allAgents().first { $0.handlerModule == CliBundle.handlerModule }
        if isTTY, let cli {
            _ = await kernel.send(
                [Item(cli.id, .object(["type": .string("intro_booting")]))])[0].result
        }

        // Step 4 — THE START SEQUENCE (python `kernel.start(boot_all=True)`): the
        // root boots (it mints the guid every store key carries), then the root's
        // memory store, then the kernel starts flushing its execution rows into
        // it, then every other agent boots in REGISTRATION ORDER. A root with no
        // `memory_store_id` does not start: fail loud, exit non-zero — python
        // raises out of `_default` and `main_dispatch` prints `[kernel] <e>`.
        let started = await kernel.start(bootAll: true)
        if pyTruthy(started["error"]) {
            RuntimeLog.warn(startRefusedLine(started))
            await kernel.shutdown()
            lock.release()
            exit(1)
        }
        // The stop handlers go in BEFORE the "all booted" close / `[kernel] up`:
        // whoever reads that line may signal at once, and a SIGTERM that landed
        // while the handlers were only set up inside the wait below took the
        // default action — the process died with no teardown and `lock.yaml`
        // held (rust closed the same window: `StopSignals::install`).
        let stopSignals = StopSignals.install()
        // Second PTY push: the "all booted" close. Each agent already announced
        // its OWN endpoints during its boot — the kernel does NOT gather them,
        // and does NOT re-`reflect` web for a port. Non-tty keeps the plain
        // one-line marker, on STDOUT (python has no `file=` on this one).
        if isTTY, let cli {
            _ = await kernel.send([Item(cli.id, .object(["type": .string("booted")]))])[0]
                .result
        } else {
            printFlushed(upLine)
        }

        // Step 5 — Race the REPL task (if tty), the block task (if web) and
        // the stop signal (an OS signal, or `kernel_stop` over the web
        // surface, which deferred its trigger so its ack flushed first).
        // First to finish wins.
        let signalName = await withTaskGroup(of: String?.self) { group -> String? in
            var caught: String??
            if isTTY {
                group.addTask {
                    await REPLMode.run(kernel: kernel)
                    return nil
                }
            }
            if webExists {
                group.addTask {
                    await blockForever()
                    return nil
                }
            }
            group.addTask {
                await Bootstrap.awaitStop(kernel: kernel, signals: stopSignals)
            }
            // Race — first task to return ends the group; other tasks
            // get cancelled by the task group's structured concurrency.
            caught = await group.next()
            group.cancelAll()
            return caught ?? nil
        }
        // An OS signal (not `kernel_stop`, which sets the same event with
        // no name attached) prints python's line before the teardown walk.
        if let signalName {
            RuntimeLog.warn(stopSignalLine(signalName))
        }

        // Step 6 — Graceful shutdown: drain the agents, every call settles, the
        // last records and execution rows land (provider and store still
        // alive), then the depth-first
        // `onShutdown`/`onDelete` walk that stops the listener and frees every
        // PTY and subprocess. Records and tree untouched. Both lines are
        // STDERR in python (`file=sys.stderr`), unlike the stdout `[kernel]
        // up` marker above.
        RuntimeLog.warn(tearingDownLine)
        await kernel.shutdown()
        RuntimeLog.warn(downLine)
    }

    /// Sleeps indefinitely until cancelled by the task group when a
    /// sibling task (REPL exit, signal-driven cancellation) finishes
    /// first. Equivalent to Python's `_block_forever()`.
    private static func blockForever() async {
        // A stream nothing yields to: its iteration ends only when this task is
        // cancelled — no timer, no wakeups (owner, 2026-10-08).
        let (stream, continuation) = AsyncStream<Void>.makeStream()
        for await _ in stream {}
        continuation.finish()
    }
}
