// No-args `fantastic` daemon mode.
//
// Mirrors `python/kernel/_modes.py:_default` verbatim. Python's
// composition rule:
//
//   1. Acquire `.fantastic/lock.json` PID lock (fail if a live
//      process owns the dir).
//   2. Boot all persisted agents.
//   3. If any persisted `web` agent → run a `_block_forever()` task.
//   4. If `stdin.isatty()` → also run the `_repl_loop` task.
//   5. Race those tasks against the stop signal (SIGINT/SIGTERM/SIGHUP,
//      or the root-only `shutdown_kernel` verb); first to finish (REPL
//      exit, signal, fatal error) wins and triggers `Kernel.shutdown()`.
//   6. If neither web nor TTY → exit silently with no lock acquired.
//
// Swift's startup gap (carried over from Phase 8H): the substrate
// doesn't yet hydrate `.fantastic/agents/<id>/agent.json` records
// on boot. Today only the auto-created `web` agent exists in-memory
// after `startKernel()`. Pre-seeding workdirs for integration tests
// is therefore done post-startup via WS, not via direct file
// writes. Hydration is a tracked follow-up; this daemon is correct
// once it lands.
//
// Workdir is **always cwd** — matches Python's implicit-cwd
// convention. No `<workdir>` argument; the operator picks the
// project dir by `cd`-ing into it.

import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation

#if canImport(Darwin)
    import Darwin
#endif

enum DaemonMode {

    /// Run no-args daemon mode in cwd. Returns when the daemon
    /// finishes (REPL exit, SIGINT, SIGTERM, or fatal error).
    static func run() async {
        let fm = FileManager.default
        let cwd = fm.currentDirectoryPath
        let workdirURL = URL(fileURLWithPath: cwd, isDirectory: true)

        // Step 1 — Acquire PID lock. Refuse if another live process
        // already holds it (matches Python's `FantasticLock` shape).
        let lock = WorkdirLock(workdir: workdirURL)
        do {
            try lock.acquire()
        } catch WorkdirLock.LockError.alreadyRunning(let pid, let dir) {
            FileHandle.standardError.write(
                "fantastic: another fantastic owns this dir (pid=\(pid), workdir=\(dir))\n"
                    .data(using: .utf8) ?? Data())
            exit(1)
        } catch {
            FileHandle.standardError.write(
                "fantastic: lock acquisition failed: \(error)\n"
                    .data(using: .utf8) ?? Data())
            exit(1)
        }
        defer { lock.release() }

        // Step 2 — Boot kernel against the workdir.
        let kernel: Kernel
        do {
            kernel = try await startKernel(workdir: cwd, portHint: 0)
        } catch {
            FileHandle.standardError.write(
                "fantastic: kernel boot failed: \(error)\n"
                    .data(using: .utf8) ?? Data())
            exit(1)
        }

        // THE MESSAGE STORE, hydrated COLD — before any agent boots, from
        // above (python `hydrate_messages` in the bootstrap). A file of another
        // version or a cut row refuses the boot rather than hydrating half a
        // conversation.
        do {
            try hydrateMessages(kernel, rootDir: workdirURL.appendingPathComponent(".fantastic"))
        } catch {
            FileHandle.standardError.write(
                "fantastic: \(error)\n".data(using: .utf8) ?? Data())
            exit(1)
        }

        // Step 3 — Identify composition: does a persisted web agent
        // exist? Is stdin a tty?
        //
        // (Substrate hydration gap noted in file header — today this
        // sees only the auto-created web from startKernel, not any
        // pre-seeded persisted state. Behavior is still correct:
        // web auto-created → web boot path runs → daemon blocks.
        // Aligns with Python's behavior on a freshly-initialized
        // workdir.)
        let webExists = kernel.agent(AgentId("web")) != nil
        // fd 0 is stdin — avoid the `stdin` C global, which Glibc exposes as a
        // mutable `var` that Swift 6 strict concurrency rejects on Linux.
        let isTTY = isatty(0) != 0

        if !webExists && !isTTY {
            // Nothing to do; exit silently. Matches Python's
            // `_default()` exit-with-no-lock path.
            return
        }

        // Step 4 — Boot every persisted agent. Mirrors Python's
        // `_boot_all_agents` in `python/kernel/_modes.py`: walks the
        // kernel's flat agent index and sends `{type:"boot"}` to
        // each. Bundle-level boot hooks (web binds HTTP listener,
        // kernel_bridge attaches transport, ollama spawns HTTP
        // client, etc.) all fire here. Errors are logged but don't
        // abort the daemon — weak loading carries through.
        let listReply = await kernel.send([Item(AgentId("core"), .object(["type": .string("list_agents")]))])[0].result
        let bootedIds: [String] = (listReply["agents"].asArray ?? [])
            .compactMap { $0["id"].asString }
            .filter { $0 != "core" }  // root has no handler_module
        for id in bootedIds {
            let bootReply = await kernel.send([Item(AgentId(id), .object(["type": .string("boot")]))])[0].result
            if let err = bootReply["error"].asString {
                FileHandle.standardError.write(
                    "[kernel] boot \(id) failed: \(err)\n"
                        .data(using: .utf8) ?? Data())
            }
        }
        // AFTER the boot loop: the store agent must be live before the ledger
        // can rehydrate through it, and THE PUMP SUBSCRIBES HERE — after
        // hydration (cold, above) and the ledger, at the highest id present.
        // `subscribe(after:)` replays what the boot loop minted meanwhile.
        await kernel.installLedgerPersistence()
        kernel.installMessagePump(after: kernel.rows.nextId - 1)
        if webExists,
            let webReflect = kernel.agent(AgentId("web"))
        {
            // web's boot reply included the bound port via the live
            // server. Re-reflect to surface it in the log.
            let r = await kernel.send([Item(AgentId("web"), .object(["type": .string("reflect")]))])[0].result
            if let p = r["port"].asInt {
                FileHandle.standardError.write(
                    "[kernel] up — web bound on port \(p)\n"
                        .data(using: .utf8) ?? Data())
            }
            _ = webReflect
        } else {
            FileHandle.standardError.write(
                "[kernel] up — REPL only (no web agent persisted)\n"
                    .data(using: .utf8) ?? Data())
        }

        // Step 5 — Race the REPL task (if tty), the block task (if web) and
        // the stop signal (an OS signal, or `shutdown_kernel` over the web
        // surface, which deferred its trigger so its ack flushed first).
        // First to finish wins.
        await withTaskGroup(of: Void.self) { group in
            if isTTY {
                group.addTask {
                    await REPLMode.run(kernel: kernel)
                }
            }
            if webExists {
                group.addTask {
                    await blockForever()
                }
            }
            group.addTask {
                await Bootstrap.awaitStop(kernel: kernel)
            }
            // Race — first task to return ends the group; other tasks
            // get cancelled by the task group's structured concurrency.
            await group.next()
            group.cancelAll()
        }

        // Step 6 — Graceful shutdown: drain the agents, land the last message
        // window (provider still alive), then the depth-first
        // `onShutdown`/`onDelete` walk that stops the listener and frees every
        // PTY and subprocess. Records and tree untouched. Python `longrun`.
        FileHandle.standardError.write(
            "[kernel] tearing down agents...\n".data(using: .utf8) ?? Data())
        await kernel.shutdown()
        FileHandle.standardError.write(
            "[kernel] down\n".data(using: .utf8) ?? Data())
    }

    /// Sleeps indefinitely until cancelled by the task group when a
    /// sibling task (REPL exit, signal-driven cancellation) finishes
    /// first. Equivalent to Python's `_block_forever()`.
    private static func blockForever() async {
        while !Task.isCancelled {
            // 1-hour granularity; cancellation propagates faster
            // because `Task.sleep` is cancellation-aware.
            try? await Task.sleep(nanoseconds: 3600 * 1_000_000_000)
        }
    }
}
