// `fantastic` CLI binary — argv dispatcher.
//
// Matches Python's CLI surface (canonical reference at
// `python/main.py` + `python/kernel/_modes.py`):
//
//   fantastic                       no-args daemon mode (cwd workdir)
//                                   — acquires PID lock, boots persisted
//                                   agents, races REPL (if tty) + block
//                                   forever (if web persisted)
//   fantastic <id> <verb> [k=v ...] one-shot RPC against the workdir
//                                   — PID-locked
//   fantastic reflect [<id>]        reflect shorthand — no lock
//                                   — defaults target to `kernel`
//
// There is no `install` / `install-bundle` command in any kernel:
// bundles are compile-time linked across all kernels (Swift, Rust,
// Python alike), so there is nothing to install at runtime.

import FantasticJSON
import FantasticKernel
import FantasticKernelBridge
import FantasticKernelStartup
import Foundation

#if canImport(Darwin)
    import Darwin
#endif

func parseKV(_ s: String) -> JSON {
    switch s {
    case "true": return .bool(true)
    case "false": return .bool(false)
    default: break
    }
    if let i = Int64(s) { return .integer(i) }
    return .string(s)
}

@main
struct FantasticCLI {
    static func main() async {
        let args = CommandLine.arguments.dropFirst()

        // No-args → daemon mode (matches Python's `_default`).
        if args.isEmpty {
            await DaemonMode.run()
            return
        }

        // `reflect [<id>] [k=v ...]` — read-only, no lock.
        // Default target is `kernel` (matches Python; Swift's
        // substrate doesn't have a `kernel`-named agent yet, so the
        // reply will be {error:"no agent kernel"} until primer
        // reflect lands).
        if args.first == "reflect" {
            await runReflect(argTokens: Array(args.dropFirst()))
            return
        }

        // `<id> <verb> [k=v ...]` — one-shot RPC. PID-locked to
        // serialize concurrent CLI invocations on the same workdir
        // (matches Python's behavior).
        if args.count >= 2 {
            await runOneShotRPC(argTokens: Array(args))
            return
        }

        FileHandle.standardError.write(
            "fantastic: unrecognized arguments\n"
                .data(using: .utf8) ?? Data())
        exit(2)
    }

    // MARK: - reflect

    private static func runReflect(argTokens: [String]) async {
        // First non-kv token is the target; default is `kernel`
        // (matches Python's `_modes.py:reflect`).
        var target = "kernel"
        var kvTokens: [String] = []
        if let first = argTokens.first, !first.contains("=") {
            target = String(first)
            kvTokens = Array(argTokens.dropFirst())
        } else {
            kvTokens = argTokens
        }

        // No lock — reflect is read-only.
        let kernel: Kernel
        do {
            kernel = try await startKernelInMemory(portHint: 0)
        } catch {
            FileHandle.standardError.write(
                "fantastic: kernel boot failed: \(error)\n"
                    .data(using: .utf8) ?? Data())
            exit(1)
        }

        var payload: [(String, JSON)] = [("type", .string("reflect"))]
        for kv in kvTokens {
            if let eq = kv.firstIndex(of: "=") {
                let key = String(kv[..<eq])
                let value = String(kv[kv.index(after: eq)...])
                payload.append((key, parseKV(value)))
            }
        }
        let reply = await kernel.send(
            AgentId(target),
            .object(.init(uniqueKeysWithValues: payload))
        )
        print(reply.serializePretty(indent: 2))
    }

    // MARK: - one-shot RPC

    private static func runOneShotRPC(argTokens: [String]) async {
        let id = String(argTokens[0])
        let verb = String(argTokens[1])

        // Acquire PID lock for dispatch duration. Refuse if a live
        // daemon owns the workdir (matches Python's behavior).
        let workdirURL = URL(
            fileURLWithPath: FileManager.default.currentDirectoryPath,
            isDirectory: true)
        let lock = WorkdirLock(workdir: workdirURL)
        do {
            try lock.acquire()
        } catch WorkdirLock.LockError.alreadyRunning(let pid, _) {
            FileHandle.standardError.write(
                "fantastic: another fantastic owns this dir (pid=\(pid)) — "
                    .data(using: .utf8) ?? Data())
            FileHandle.standardError.write(
                "use the web surface (WS or REST) to talk to the running kernel\n"
                    .data(using: .utf8) ?? Data())
            exit(1)
        } catch {
            FileHandle.standardError.write(
                "fantastic: lock acquisition failed: \(error)\n"
                    .data(using: .utf8) ?? Data())
            exit(1)
        }
        defer { lock.release() }

        // Boot kernel disk-backed against the workdir. This hydrates
        // any persisted agents AND ensures `create_agent` writes get
        // flushed to disk so subsequent invocations (or the
        // long-running daemon) see them. Matches Python's one-shot
        // behavior.
        let kernel: Kernel
        do {
            kernel = try await startKernel(
                workdir: FileManager.default.currentDirectoryPath,
                portHint: 0)
        } catch {
            FileHandle.standardError.write(
                "fantastic: kernel boot failed: \(error)\n"
                    .data(using: .utf8) ?? Data())
            exit(1)
        }

        var payload: [(String, JSON)] = [("type", .string(verb))]
        for kv in argTokens.dropFirst(2) {
            if let eq = kv.firstIndex(of: "=") {
                let key = String(kv[..<eq])
                let value = String(kv[kv.index(after: eq)...])
                payload.append((key, parseKV(value)))
            }
        }
        let reply = await kernel.send(
            AgentId(id),
            .object(.init(uniqueKeysWithValues: payload))
        )
        print(reply.serializePretty(indent: 2))
    }
}
