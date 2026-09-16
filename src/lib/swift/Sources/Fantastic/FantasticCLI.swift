// `fantastic` CLI binary — argv dispatcher.
//
// Mirrors python's CLI surface (canonical: `P/main.py` +
// `P/kernel/modes/{dispatch,help,parse,print}.py`):
//
//   fantastic                       no-args daemon mode (cwd workdir)
//                                   — acquires PID lock, boots persisted
//                                   agents, races REPL (if tty) + block
//                                   forever (if web persisted)
//   fantastic help | -h | --help    print `cli/help.md`, exit 0
//                                   (later words ignored)
//   fantastic reflect [<id>] [k=v]  reflect shorthand — no lock
//                                   — defaults target to `kernel`
//   fantastic <id> <verb> [k=v ...] one-shot RPC against the workdir
//                                   — PID-locked
//   fantastic <one word>            python's 4-line usage on stderr, exit 2
//
// Every mode runs the same two preludes python's `main_dispatch` runs before
// argv is looked at: the `.env` autoload, then the disk-backed bootstrap that
// seeds a virgin `.fantastic/` and hydrates the cold message log. `reflect`
// takes no lock; it still reads the persisted tree.
//
// There is no `install` / `install-bundle` command in any kernel:
// bundles are compile-time linked across all kernels (Swift, Rust,
// Python alike), so there is nothing to install at runtime.

import FantasticCliBundle
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import OrderedCollections

#if canImport(Darwin)
    import Darwin
#endif

/// Python `kernel/modes/parse.py:_coerce` — the CLI's `k=v` value rule.
///
/// The shared scalar rule first (`coerceScalar`, which the model's `<k>v</k>`
/// path uses too), then ONE extra branch for the operator: a `{…}` / `[…]`
/// literal that parses is the JSON it spells. Anything else is the string as
/// written. The model↔harness path carries no JSON at all, which is why the
/// branch lives here and not in `coerceScalar`.
func cliCoerce(_ v: String) -> JSON {
    let typed = coerceScalar(v)
    guard case .string = typed else { return typed }
    let looksJSON =
        (v.hasPrefix("{") && v.hasSuffix("}")) || (v.hasPrefix("[") && v.hasSuffix("]"))
    if looksJSON, let parsed = try? JSON.parse(v) { return parsed }
    return .string(v)
}

/// Python `kernel/modes/parse.py:_parse_kv` merged into `{"type": verb, **kv}`.
///
/// Built BY ASSIGNMENT, like python's dict: a repeated key keeps its FIRST
/// position and takes the LAST value, and `type=` overrides the verb in place.
/// `uniqueKeysWithValues` used to trap here (`Fatal error: Duplicate key`) on
/// `limit=1 limit=2` and on `reflect type=list_agents`.
func cliPayload(verb: String, kvTokens: some Sequence<String>) -> JSON {
    var payload: OrderedDictionary<String, JSON> = ["type": .string(verb)]
    for kv in kvTokens {
        guard let eq = kv.firstIndex(of: "=") else { continue }
        payload[String(kv[..<eq])] = cliCoerce(String(kv[kv.index(after: eq)...]))
    }
    return .object(payload)
}

@main
struct FantasticCLI {

    /// argv → mode, python `kernel/modes/dispatch.py:dispatch_argv` order.
    enum Mode: Equatable {
        case daemon
        case help
        case reflect([String])
        case oneShot(target: String, rest: [String])
        case usage
    }

    /// Pure routing decision — no kernel, no IO, so the twin can pin it.
    static func mode(for args: [String]) -> Mode {
        guard let first = args.first else { return .daemon }
        // `help` / `-h` / `--help` as the FIRST word; later words ignored.
        if first == "-h" || first == "--help" || first == "help" { return .help }
        if first == "reflect" { return .reflect(Array(args.dropFirst())) }
        if args.count >= 2 { return .oneShot(target: first, rest: Array(args.dropFirst())) }
        return .usage
    }

    /// Python `kernel/modes/dispatch.py:34-40` — the single-positional usage.
    static let usageText = """
        usage: fantastic <id> <verb> [k=v ...]
               fantastic reflect [<id>]
               fantastic                              (interactive REPL when stdin is a tty;
                                                       daemon when a web agent is persisted)
        """

    /// Python `kernel/_lock.py:60-64` rendered through `main.py:128` — the
    /// daemon's refusal when another live pid owns the dir.
    static func daemonLockRefusal(pid: Int) -> String {
        """
        [kernel] another fantastic owns this dir: pid=\(pid)
          -> kill it:  kill \(pid)
          -> or, if stale, remove the lock:  rm .fantastic/lock.json
        """
    }

    /// Python `kernel/modes/dispatch.py:65-70` — a one-shot's refusal.
    static func callLockRefusal(pid: Int) -> String {
        "[call] another fantastic owns this dir (pid=\(pid)). "
            + "Stop it, or forward your call over the WS bridge "
            + "(see ws_bridge bundle)."
    }

    static func main() async {
        // Python `main.py:89-91` — the `.env` autoload is the first step of
        // EVERY mode, before the kernel is built.
        do {
            let n = try loadDotEnv()
            if n > 0 {
                errLine("[kernel] loaded \(n) var(s) from .env")
            }
        } catch {
            errLine("fantastic: \(error)")
            exit(1)
        }

        switch mode(for: Array(CommandLine.arguments.dropFirst())) {
        case .daemon:
            await DaemonMode.run()
        case .help:
            // Python bootstraps (and seeds a virgin dir) before dispatch even
            // looks at argv, so `fantastic help` leaves a `.fantastic/` behind.
            _ = await bootOrExit()
            FileHandle.standardOutput.write(
                (CliBundle.helpText + "\n").data(using: .utf8) ?? Data())
        case .reflect(let argTokens):
            await runReflect(argTokens: argTokens)
        case .oneShot(let target, let rest):
            await runOneShotRPC(target: target, rest: rest)
        case .usage:
            _ = await bootOrExit()
            errLine(usageText)
            exit(2)
        }
    }

    // MARK: - bootstrap

    /// Python `main.py:_build_kernel` — the disk-backed bootstrap every mode
    /// runs: seed a virgin `.fantastic/`, hydrate the persisted tree, then the
    /// cold message log. NO lock: `reflect` is read-only and the one-shot takes
    /// the lock itself, before this.
    static func bootDiskBacked(cwd: String = FileManager.default.currentDirectoryPath)
        async throws -> Kernel
    {
        let kernel = try await startKernel(workdir: cwd, portHint: 0)
        try hydrateMessages(
            kernel,
            rootDir: URL(fileURLWithPath: cwd).appendingPathComponent(".fantastic"))
        return kernel
    }

    /// `bootDiskBacked` for the process entrypoints: a failure here is a dead
    /// invocation, so it prints and exits rather than propagating.
    private static func bootOrExit() async -> Kernel {
        do {
            return try await bootDiskBacked()
        } catch {
            errLine("fantastic: kernel boot failed: \(error)")
            exit(1)
        }
    }

    /// Python `json.dumps(reply, indent=2, default=str)` — what a one-shot, a
    /// `reflect` and the REPL print for a reply. ensure_ascii, python float
    /// repr, `NaN`/`Infinity` spellings.
    static func replyJSON(_ v: JSON) -> String {
        pyJsonDumps(v, indent: 2)
    }

    // MARK: - reflect

    private static func runReflect(argTokens: [String]) async {
        // First non-kv token is the target; default is `kernel`
        // (python `kernel/modes/dispatch.py:164-167`).
        var target = "kernel"
        var kvTokens: [String] = argTokens
        if let first = argTokens.first, !first.contains("=") {
            target = first
            kvTokens = Array(argTokens.dropFirst())
        }

        // No lock — reflect is read-only. Disk-backed all the same: python
        // reflects the PERSISTED tree, guid and readmes, not a phantom RAM one.
        let kernel = await bootOrExit()
        let body = cliPayload(verb: "reflect", kvTokens: kvTokens)
        let reply = await kernel.send([Item(AgentId(target), body)])[0].result
        print(replyJSON(reply))
    }

    // MARK: - one-shot RPC

    private static func runOneShotRPC(target: String, rest: [String]) async {
        let verb = rest[0]

        // Acquire PID lock for dispatch duration. Refuse if a live
        // daemon owns the workdir — BEFORE any bootstrap, so a conflicting
        // invocation writes nothing (python `dispatch.py:63-71`).
        let workdirURL = URL(
            fileURLWithPath: FileManager.default.currentDirectoryPath,
            isDirectory: true)
        let lock = WorkdirLock(workdir: workdirURL)
        do {
            try lock.acquire()
        } catch WorkdirLock.LockError.alreadyRunning(let pid, _) {
            errLine(callLockRefusal(pid: Int(pid)))
            exit(1)
        } catch {
            errLine("fantastic: lock acquisition failed: \(error)")
            exit(1)
        }
        defer { lock.release() }

        let kernel = await bootOrExit()
        let body = cliPayload(verb: verb, kvTokens: rest.dropFirst())
        let reply = await kernel.send([Item(AgentId(target), body)])[0].result
        print(replyJSON(reply))
    }

    // MARK: - stderr

    private static func errLine(_ s: String) {
        FileHandle.standardError.write((s + "\n").data(using: .utf8) ?? Data())
    }
}
