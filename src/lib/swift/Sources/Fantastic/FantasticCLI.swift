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
// seeds a virgin `.fantastic/` and hydrates the persisted tree. `reflect`
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
        case initCmd([String])
        case oneShot(target: String, rest: [String])
        case usage
    }

    /// Pure routing decision — no kernel, no IO, so the twin can pin it.
    static func mode(for args: [String]) -> Mode {
        guard let first = args.first else { return .daemon }
        // `help` / `-h` / `--help` as the FIRST word; later words ignored.
        if first == "-h" || first == "--help" || first == "help" { return .help }
        if first == "reflect" { return .reflect(Array(args.dropFirst())) }
        // A14.3: checked by NAME, like `reflect` — `fantastic init` (a single
        // word) still routes here, not to `.usage`, exactly as python's
        // `dispatch_argv` matches subcommands before the generic `len>=2` case.
        if first == "init" { return .initCmd(Array(args.dropFirst())) }
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
          -> or, if stale, remove the lock:  rm .fantastic/lock.yaml
        """
    }

    /// Python `kernel/modes/dispatch.py:call` — a one-shot's refusal.
    static func callLockRefusal(pid: Int) -> String {
        "[call] another fantastic owns this dir (pid=\(pid)). "
            + "Stop it, or forward your call over the WS bridge "
            + "(see ws_bridge bundle)."
    }

    /// Python `kernel/modes/dispatch.py:init` — `init`'s own refusal (a
    /// shorter text than the generic one-shot's `callLockRefusal`).
    static func initLockRefusal(pid: Int) -> String {
        "[init] another fantastic owns this dir (pid=\(pid)). Stop it first."
    }

    /// Python `kernel/modes/dispatch.py:98-101` — `init`'s usage line.
    static let initUsageText =
        "usage: fantastic init <loader_bundle> <manifest>\n"
        + "   e.g. fantastic init static_loader ./fantastic.yaml"

    // MARK: - ledger cap (C80)

    /// Python `main.py:_ledger_cap_from` — strip `--ledger-cap <v>` from argv
    /// (value REQUIRED: a trailing bare flag with nothing after it stays,
    /// matching python's `i + 1 < len(argv)` guard); absent ⇒ fall back to
    /// `FANTASTIC_LEDGER_CAP`. `"inf"` ⇒ unbounded (`nil`); anything else must
    /// parse as an int (python's `int()`, `pyInt` here) or the caller exits 1.
    /// Returns `(argv without the flag, cap, whether a cap was named at all)`
    /// — `wasSet == false` means "leave the ledger's own default cap alone".
    static func ledgerCapFrom(
        _ argv: [String],
        env: [String: String] = ProcessInfo.processInfo
            .environment
    ) -> (argv: [String], cap: Int?, wasSet: Bool) {
        var raw: String?
        var out: [String] = []
        var i = 0
        while i < argv.count {
            if argv[i] == "--ledger-cap", i + 1 < argv.count {
                raw = argv[i + 1]
                i += 2
                continue
            }
            out.append(argv[i])
            i += 1
        }
        if raw == nil { raw = env["FANTASTIC_LEDGER_CAP"] }
        guard let raw else { return (out, nil, false) }
        if raw == "inf" { return (out, nil, true) }
        guard let cap = try? pyInt(.string(raw)) else {
            errLine(
                "fantastic: invalid --ledger-cap value \(pyRepr(raw)) (must be an int or 'inf')")
            exit(1)
        }
        return (out, cap, true)
    }

    /// Resolved ONCE at process start (`main()`), applied after EVERY kernel
    /// construction — `bootOrExit()` here, and `DaemonMode.run()`'s own,
    /// separate `startKernel` call, since python builds exactly one kernel
    /// per process but this runtime's mode branches each construct their own.
    /// `nonisolated(unsafe)`: written once by `main()` before any concurrent
    /// reader exists (every mode branch runs sequentially after that write).
    nonisolated(unsafe) static var ledgerCap: (cap: Int?, wasSet: Bool) = (nil, false)

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

        // `FANTASTIC_LEDGER_CAP` may itself come from the `.env` just loaded,
        // so this reads the environment AFTER that step — python's own order.
        let (strippedArgv, cap, capSet) = ledgerCapFrom(Array(CommandLine.arguments.dropFirst()))
        ledgerCap = (cap, capSet)

        switch mode(for: strippedArgv) {
        case .daemon:
            await DaemonMode.run()
        case .help:
            // Python bootstraps (and seeds a virgin dir) before dispatch even
            // looks at argv, so `fantastic help` leaves a `.fantastic/` behind.
            let kernel = await bootOrExit()
            FileHandle.standardOutput.write(
                (CliBundle.helpText + "\n").data(using: .utf8) ?? Data())
            // A14.18: every non-daemon mode ends with an idempotent kernel
            // shutdown (python's `atexit` → `Kernel.shutdown`).
            await kernel.shutdown()
        case .reflect(let argTokens):
            await runReflect(argTokens: argTokens)
        case .initCmd(let rest):
            await runInit(rest: rest)
        case .oneShot(let target, let rest):
            await runOneShotRPC(target: target, rest: rest)
        case .usage:
            let kernel = await bootOrExit()
            errLine(usageText)
            await kernel.shutdown()
            exit(2)
        }
    }

    // MARK: - bootstrap

    /// Python `main.py:_build_kernel` — the disk-backed bootstrap every mode
    /// runs: seed a virgin `.fantastic/` and hydrate the persisted tree (the
    /// kernel keeps no message log). NO lock: `reflect` is read-only and the one-shot takes
    /// the lock itself, before this.
    static func bootDiskBacked(cwd: String = FileManager.default.currentDirectoryPath)
        async throws -> Kernel
    {
        let kernel = try await startKernel(workdir: cwd, portHint: 0)
        // C80: applied after EVERY kernel construction, python's own
        // `if cap_set: kernel.ledger_cap = cap` right after `_build_kernel()`.
        // A no-op in tests, which never touch `ledgerCap` (stays `wasSet:
        // false`).
        if ledgerCap.wasSet { kernel.ledger.setCap(ledgerCap.cap) }
        composeCliIfTTY(kernel)
        return kernel
    }

    /// Python `main._bootstrap`: `if sys.stdin.isatty(): Cli(kernel,
    /// parent=kernel.root)` — an ephemeral stdout renderer composed for
    /// EVERY mode (one-shot, reflect, init, help, the daemon), not the
    /// daemon alone; a human (or an LLM in a PTY) landing in an
    /// interactive `fantastic reflect` sees `cli` in the tree exactly as
    /// python does. No-op without a root or a tty. Called exactly once per
    /// freshly-constructed kernel (`bootDiskBacked` / `DaemonMode.run`'s own
    /// `startKernel` call), never on an already-composed one.
    static func composeCliIfTTY(_ kernel: Kernel) {
        guard isatty(0) != 0, let root = kernel.root else { return }
        _ = kernel.composeEphemeralChild(
            id: AgentId("cli"), handlerModule: CliBundle.handlerModule, parent: root,
            meta: ["display_name": .string("cli"), "singleton": .bool(true)])
    }

    /// `bootDiskBacked` for the process entrypoints: a failure here is a dead
    /// invocation, so it prints and exits rather than propagating.
    private static func bootOrExit(cwd: String = FileManager.default.currentDirectoryPath)
        async -> Kernel
    {
        do {
            return try await bootDiskBacked(cwd: cwd)
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
        // A14.18: no lock was ever taken, so shutdown runs right after the print.
        await kernel.shutdown()
    }

    // MARK: - one-shot RPC

    private static func runOneShotRPC(target: String, rest: [String]) async {
        let verb = rest[0]

        // Acquire the workdir lock for dispatch duration — THE DAEMON'S OWN
        // flock, BEFORE any bootstrap, so a conflicting invocation writes
        // nothing (python `dispatch.py:_lock_or_exit`). Only a live holder
        // refuses: a lock file naming a pid that is alive but holds no flock (a
        // reused pid) is stale, and taken over (owner, 2026-10-03).
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

        let kernel = await bootOrExit()
        let reply = await dispatchOneShot(
            kernel: kernel, target: target, verb: verb, rest: rest.dropFirst())
        print(replyJSON(reply))
        // A14.18: shutdown runs AFTER the lock is released, never via `defer`
        // (which would fire before this line, running shutdown first).
        lock.release()
        await kernel.shutdown()
    }

    /// A14.6's actual sequencing, factored out so a test can pin it without
    /// the process-level lock/exit plumbing around it: boot the ROOT LOADER
    /// FIRST, so THIS call's mutations (create/update/delete) subscribe +
    /// persist (python `dispatch.py:73-78`) — rust/swift never did this: no
    /// pump, no ledger, no record writer installed for a one-shot RPC. Then
    /// dispatch the call itself, and drain the debounced writer
    /// SYNCHRONOUSLY — its own 150ms tick would never fire before this
    /// process exits otherwise (python `await loop.aflush()`).
    static func dispatchOneShot(
        kernel: Kernel, target: String, verb: String, rest: some Sequence<String>
    ) async -> JSON {
        if let root = kernel.root {
            _ = await kernel.send([Item(root.id, .object(["type": .string("boot")]))])[0].result
        }
        let body = cliPayload(verb: verb, kvTokens: rest)
        let reply = await kernel.send([Item(AgentId(target), body)])[0].result
        await kernel.flushRecords()
        return reply
    }

    // MARK: - init (A14.3)

    private static func runInit(rest: [String]) async {
        guard rest.count >= 2 else {
            let kernel = await initUsage()
            await kernel.shutdown()
            exit(2)
        }
        let loaderName = rest[0]
        let manifest = rest[1]

        // python `dispatch.py:_lock_or_exit`: the daemon's own flock; only a
        // live holder refuses (a stale file naming a reused pid is taken over).
        let workdirURL = URL(
            fileURLWithPath: FileManager.default.currentDirectoryPath,
            isDirectory: true)
        let lock = WorkdirLock(workdir: workdirURL)
        do {
            try lock.acquire()
        } catch WorkdirLock.LockError.alreadyRunning(let pid, _) {
            errLine(initLockRefusal(pid: Int(pid)))
            exit(1)
        } catch {
            errLine("fantastic: lock acquisition failed: \(error)")
            exit(1)
        }

        let kernel = await bootOrExit()
        let reply = await dispatchInit(kernel: kernel, loaderName: loaderName, manifest: manifest)
        print(replyJSON(reply))
        lock.release()
        await kernel.shutdown()
    }

    /// A14.3's USAGE branch, factored out like `dispatchInit` so a test can
    /// pin the ORDER without the process-level `exit(2)`: python builds the
    /// kernel in `main` BEFORE `dispatch_argv` runs, so even `init`'s usage
    /// error leaves the SEEDED root behind — `.fantastic/` + `agent.yaml`.
    /// Same shape as the
    /// `.usage` branch; NO lock — python's `init` probes the lock only once
    /// it has both arguments. Rust mirrors it in
    /// `fantastic-cli/src/main.rs:407`.
    static func initUsage(cwd: String = FileManager.default.currentDirectoryPath) async -> Kernel {
        let kernel = await bootOrExit(cwd: cwd)
        errLine(initUsageText)
        return kernel
    }

    /// A14.3's actual sequencing, factored out so a test can pin it without
    /// the process-level lock/exit plumbing: boot the root loader so the new
    /// loader record subscribes + persists, then `create_agent` it — which
    /// ALSO boots the loader CHILD, whose own boot reads the manifest and
    /// hydrates the roster (the kernel never parses YAML; the loader bundle
    /// does). Idempotent by id: a second `init` refuses as a duplicate
    /// create — python's own "no duplicate-init guard".
    static func dispatchInit(kernel: Kernel, loaderName: String, manifest: String) async -> JSON {
        let root = kernel.root!
        _ = await kernel.send([Item(root.id, .object(["type": .string("boot")]))])[0].result
        let reply = await kernel.send([
            Item(
                root.id,
                .object([
                    "type": .string("create_agent"),
                    "parent_id": .string(root.id.value),
                    "handler_module": .string("\(loaderName).tools"),
                    "id": .string(loaderName),
                    "manifest": .string(manifest),
                ]))
        ])[0].result
        await kernel.flushRecords()
        return reply
    }

    // MARK: - stderr

    private static func errLine(_ s: String) {
        FileHandle.standardError.write((s + "\n").data(using: .utf8) ?? Data())
    }
}
