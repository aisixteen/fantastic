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
//   5. Race those tasks; first to finish (REPL exit, signal, fatal
//      error) wins and triggers shutdown.
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
        let isTTY = isatty(fileno(stdin)) != 0

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
        let listReply = await kernel.send(
            AgentId("core"), .object(["type": .string("list_agents")]))
        let bootedIds: [String] = (listReply["agents"].asArray ?? [])
            .compactMap { $0["id"].asString }
            .filter { $0 != "core" }  // root has no handler_module
        for id in bootedIds {
            let bootReply = await kernel.send(
                AgentId(id), .object(["type": .string("boot")]))
            if let err = bootReply["error"].asString {
                FileHandle.standardError.write(
                    "[kernel] boot \(id) failed: \(err)\n"
                        .data(using: .utf8) ?? Data())
            }
        }
        if webExists,
            let webReflect = kernel.agent(AgentId("web"))
        {
            // web's boot reply included the bound port via the live
            // server. Re-reflect to surface it in the log.
            let r = await kernel.send(
                AgentId("web"), .object(["type": .string("reflect")]))
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

        // Step 5 — Race the REPL task (if tty) and the block task
        // (if web). First to finish wins.
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
            // Race — first task to return ends the group; other tasks
            // get cancelled by the task group's structured concurrency.
            await group.next()
            group.cancelAll()
        }

        // Step 6 — Graceful shutdown. Send {type:"shutdown"} to the
        // web agent so its NWListener stops cleanly.
        if webExists {
            _ = await kernel.send(
                AgentId("web"), .object(["type": .string("shutdown")]))
        }
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
