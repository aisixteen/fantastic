// Twins for the CLI argv surface — python `P/main.py` +
// `P/kernel/modes/{dispatch,help,parse}.py`, `P/kernel/_env.py`, `P/kernel/_lock.py`.
//
// Everything here is the text or the routing decision python makes, pinned
// verbatim. The process-level behaviour those decisions drive (exit codes,
// stdout/stderr split) is the acceptance harness's job; these pin the bytes.

import FantasticCliBundle
import FantasticJSON
import FantasticKernel
import Foundation
import Testing

@testable import Fantastic

/// FNV-1a/64 over the utf-8 bytes — a dependency-free exact fingerprint, so a
/// one-character drift in a 1545-byte constant fails the twin.
private func fnv1a64(_ s: String) -> UInt64 {
    var h: UInt64 = 14_695_981_039_346_656_037
    for b in Array(s.utf8) {
        h ^= UInt64(b)
        h = h &* 1_099_511_628_211
    }
    return h
}

@Suite("CLI argv modes")
struct CLIModeTests {

    // A14.1 — `help` / `-h` / `--help` as the FIRST word, later words ignored.
    @Test func helpPrintsCliHelpMdVerbatim() {
        for spelling in ["help", "-h", "--help"] {
            #expect(FantasticCLI.mode(for: [spelling]) == .help)
            #expect(FantasticCLI.mode(for: [spelling, "foo", "bar"]) == .help)
        }
        // What `print(help.md.rstrip())` puts on stdout, byte for byte.
        let printed = CliBundle.helpText + "\n"
        #expect(printed.utf8.count == 1545)
        #expect(fnv1a64(printed) == 10_884_461_959_455_478_606)
        #expect(printed.hasPrefix("# fantastic — CLI\n"))
        #expect(printed.hasSuffix("everything else.\n"))
        // rstripped: no blank tail before the single newline `print` adds.
        #expect(!CliBundle.helpText.hasSuffix("\n"))
    }

    // A14.2 — exactly one remaining word that is not a subcommand.
    @Test func singlePositionalPrintsUsageAndExits2() {
        #expect(FantasticCLI.mode(for: ["memo"]) == .usage)
        #expect(FantasticCLI.mode(for: ["boot"]) == .usage)
        #expect(
            FantasticCLI.usageText == """
                usage: fantastic <id> <verb> [k=v ...]
                       fantastic reflect [<id>]
                       fantastic                              (interactive REPL when stdin is a tty;
                                                               daemon when a web agent is persisted)
                """)
        // Two words is a one-shot, never the usage.
        #expect(
            FantasticCLI.mode(for: ["memo", "reflect"])
                == .oneShot(target: "memo", rest: ["reflect"]))
        // `boot` is an agent id like any other (no daemon fallthrough).
        #expect(
            FantasticCLI.mode(for: ["boot", "reflect"])
                == .oneShot(target: "boot", rest: ["reflect"]))
        // No args at all is the daemon.
        #expect(FantasticCLI.mode(for: []) == .daemon)
        // `reflect` routes by its own branch, with or without arguments.
        #expect(FantasticCLI.mode(for: ["reflect"]) == .reflect([]))
        #expect(
            FantasticCLI.mode(for: ["reflect", "memo", "tree=ids"])
                == .reflect(["memo", "tree=ids"]))
    }

    // A14.7 — a one-shot while a live pid owns the lock.
    @Test func oneShotRefusesALiveLockWithPythonText() {
        #expect(
            FantasticCLI.callLockRefusal(pid: 4321)
                == "[call] another fantastic owns this dir (pid=4321). "
                + "Stop it, or forward your call over the WS bridge "
                + "(see ws_bridge bundle).")
    }

    // A14.8 — the daemon's refusal (python raises out of `acquire_lock`, and
    // `main_dispatch` prints `[kernel] <e>`).
    @Test func daemonLockRefusalIsPythonText() {
        #expect(
            FantasticCLI.daemonLockRefusal(pid: 99) == """
                [kernel] another fantastic owns this dir: pid=99
                  -> kill it:  kill 99
                  -> or, if stale, remove the lock:  rm .fantastic/lock.json
                """)
    }

    // A14.9 — stdout JSON is `json.dumps(reply, indent=2, default=str)`.
    @Test func oneShotJsonIsPythonEnsureAscii() {
        let reply: JSON = .object([
            "more": .string("2 more — use after=2"),
            "astral": .string("\u{1F600}"),
            "f": .double(1.5),
            "inf": .double(.infinity),
            "nan": .double(.nan),
        ])
        #expect(
            FantasticCLI.replyJSON(reply) == """
                {
                  "more": "2 more \\u2014 use after=2",
                  "astral": "\\ud83d\\ude00",
                  "f": 1.5,
                  "inf": Infinity,
                  "nan": NaN
                }
                """)
        // Empty containers and nesting keep python's separators.
        #expect(
            FantasticCLI.replyJSON(.object(["a": .array([]), "b": .object([:])])) == """
                {
                  "a": [],
                  "b": {}
                }
                """)
    }

    // C80: `--ledger-cap` is stripped from argv only when a value follows (a
    // trailing bare flag stays, python's `i + 1 < len(argv)` guard); absent
    // ⇒ `FANTASTIC_LEDGER_CAP`; `"inf"` ⇒ unbounded.
    @Test func ledgerCapFlagIsStripped() {
        let (argv, cap, wasSet) = FantasticCLI.ledgerCapFrom(
            ["kernel", "--ledger-cap", "42", "reflect"], env: [:])
        #expect(argv == ["kernel", "reflect"])
        #expect(cap == 42)
        #expect(wasSet)

        let (argv2, cap2, wasSet2) = FantasticCLI.ledgerCapFrom(["kernel", "reflect"], env: [:])
        #expect(argv2 == ["kernel", "reflect"])
        #expect(cap2 == nil)
        #expect(!wasSet2)

        let (argv3, cap3, wasSet3) = FantasticCLI.ledgerCapFrom(
            ["kernel", "--ledger-cap", "inf"], env: [:])
        #expect(argv3 == ["kernel"])
        #expect(cap3 == nil)
        #expect(wasSet3)

        // A trailing bare flag (no value follows) is left untouched — python
        // requires `i + 1 < len(argv)` before it treats it as the flag.
        let (argv4, _, wasSet4) = FantasticCLI.ledgerCapFrom(["kernel", "--ledger-cap"], env: [:])
        #expect(argv4 == ["kernel", "--ledger-cap"])
        #expect(!wasSet4)

        // The env fallback, only consulted when the flag is absent.
        let (_, cap5, wasSet5) = FantasticCLI.ledgerCapFrom(
            ["kernel"], env: ["FANTASTIC_LEDGER_CAP": "7"])
        #expect(cap5 == 7)
        #expect(wasSet5)
    }

    // A14.3 — `init` is matched by NAME, like `reflect`, before the generic
    // `len>=2` one-shot case; a single-word `init` still routes here.
    @Test func initRoutesLikeReflect() {
        #expect(
            FantasticCLI.mode(for: ["init", "static_loader", "./fantastic.yaml"])
                == .initCmd(["static_loader", "./fantastic.yaml"]))
        #expect(FantasticCLI.mode(for: ["init"]) == .initCmd([]))
        #expect(
            FantasticCLI.initUsageText
                == "usage: fantastic init <loader_bundle> <manifest>\n"
                + "   e.g. fantastic init static_loader ./fantastic.yaml")
        #expect(
            FantasticCLI.initLockRefusal(pid: 55)
                == "[init] another fantastic owns this dir (pid=55). Stop it first.")
    }

    // A14.13 — the daemon lifecycle banner texts, pinned as python spells
    // them (`longrun.py:62,79-83,124,129,207`).
    @Test func daemonBannersArePythonText() {
        #expect(DaemonMode.upLine == "[kernel] up")
        #expect(DaemonMode.tearingDownLine == "[kernel] tearing down agents...")
        #expect(DaemonMode.downLine == "[kernel] down")
        #expect(DaemonMode.stopSignalLine("SIGTERM") == "\n[kernel] SIGTERM — shutting down...")
        #expect(DaemonMode.stopSignalLine("SIGINT") == "\n[kernel] SIGINT — shutting down...")
        #expect(
            DaemonMode.bootRaisedLine(id: AgentId("np"), error: .string("No module named 'nope'"))
                == "  [kernel] boot 'np' raised: No module named 'nope'")
    }

    // A14.15 — web composes by `handler_module == "web.tools"`, never by the
    // literal id `"web"`: a renamed web agent still composes, and an
    // unrelated agent id'd `web` does not.
    @Test func webDetectedByHandlerModule() {
        let reg = BundleRegistry()
        reg.register("web.tools", DaemonTestNoop())
        reg.register("noop.tools", DaemonTestNoop())
        let k = Kernel(storage: .inMemory, bundles: reg)
        let root = Agent(id: AgentId(ROOT_ID), handlerModule: nil, parentId: nil)
        k.register(root)
        k.setRoot(root)
        #expect(!DaemonMode.hasWebAgent(k), "no web agent yet")

        let renamed = Agent(
            id: AgentId("myweb"), handlerModule: "web.tools", parentId: root.id)
        k.register(renamed)
        #expect(DaemonMode.hasWebAgent(k), "a renamed web agent still composes")

        let k2 = Kernel(storage: .inMemory, bundles: reg)
        let root2 = Agent(id: AgentId(ROOT_ID), handlerModule: nil, parentId: nil)
        k2.register(root2)
        k2.setRoot(root2)
        let impostor = Agent(
            id: AgentId("web"), handlerModule: "noop.tools", parentId: root2.id)
        k2.register(impostor)
        #expect(!DaemonMode.hasWebAgent(k2), "an id merely SPELLED web is not the web bundle")
    }
}

private struct DaemonTestNoop: AgentBundle {
    let name = "noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? { .null }
}

@Suite("CLI .env autoload")
struct DotEnvTests {

    // A14.5 — python `kernel/_env.py:_load_dotenv`, the first step of every mode.
    @Test func dotenvIsLoadedAndAnnounced() throws {
        let dir = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("fantastic-dotenv-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let tag = UUID().uuidString.prefix(8).uppercased()
        setenv("SHELL_WINS_\(tag)", "from-shell", 1)
        let file = dir.appendingPathComponent(".env")
        try """
        # a comment

        PLAIN_\(tag)=one
        export EXPORTED_\(tag)=two
          SPACED_\(tag)  =  three
        DQ_\(tag)="in quotes"
        SQ_\(tag)='in quotes'
        UNMATCHED_\(tag)="half
        EMPTY_\(tag)=
        =novalue
        no_equals_line
        SHELL_WINS_\(tag)=from-file
        """.write(to: file, atomically: true, encoding: .utf8)

        let n = try loadDotEnv(path: file.path)
        // 7 set: PLAIN, EXPORTED, SPACED, DQ, SQ, UNMATCHED, EMPTY — nothing for
        // the `=novalue` / `no_equals_line` / already-in-environ lines.
        #expect(n == 7)
        #expect(String(cString: getenv("PLAIN_\(tag)")) == "one")
        #expect(String(cString: getenv("EXPORTED_\(tag)")) == "two")
        #expect(String(cString: getenv("SPACED_\(tag)")) == "three")
        #expect(String(cString: getenv("DQ_\(tag)")) == "in quotes")
        #expect(String(cString: getenv("SQ_\(tag)")) == "in quotes")
        #expect(String(cString: getenv("UNMATCHED_\(tag)")) == "\"half")
        #expect(String(cString: getenv("EMPTY_\(tag)")) == "")
        // The shell wins — an existing entry is never overwritten.
        #expect(String(cString: getenv("SHELL_WINS_\(tag)")) == "from-shell")
        #expect(getenv("novalue") == nil)

        for k in [
            "PLAIN", "EXPORTED", "SPACED", "DQ", "SQ", "UNMATCHED", "EMPTY", "SHELL_WINS",
        ] {
            unsetenv("\(k)_\(tag)")
        }
    }

    @Test func dotenvIsSilentWhenAbsent() throws {
        let missing = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("fantastic-no-dotenv-\(UUID().uuidString)").path
        #expect(try loadDotEnv(path: missing) == 0)
    }

    @Test func splitlinesMatchesPython() {
        // Python `str.splitlines()` breaks on more than `\n`, and yields no
        // trailing empty piece.
        #expect(
            pySplitlines("a\nb\r\nc\rd\u{0b}e\u{0c}f\u{1c}g\u{85}h") == [
                "a", "b", "c", "d", "e", "f", "g", "h",
            ])
        #expect(pySplitlines("a\n") == ["a"])
        #expect(pySplitlines("") == [])
        #expect(pySplitlines("\n\n") == ["", ""])
    }
}

@Suite("CLI disk-backed bootstrap")
struct CLIBootstrapTests {

    // A14.12 — `reflect` (and every other mode) boots disk-backed: the
    // persisted tree, the persisted root guid, a seeded virgin dir.
    @Test func reflectReadsThePersistedTree() async throws {
        let dir = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("fantastic-reflect-\(UUID().uuidString)")
        let dot = dir.appendingPathComponent(".fantastic")
        let memo = dot.appendingPathComponent("agents").appendingPathComponent("memo")
        try FileManager.default.createDirectory(at: memo, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        try
            #"{"id": "kernel_state", "handler_module": null, "parent_id": null, "guid": "deadbeef"}"#
            .write(
                to: dot.appendingPathComponent("agent.json"), atomically: true, encoding: .utf8)
        try #"{"id": "memo", "handler_module": "cli.tools", "parent_id": "kernel_state"}"#
            .write(
                to: memo.appendingPathComponent("agent.json"), atomically: true, encoding: .utf8)

        let kernel = try await FantasticCLI.bootDiskBacked(cwd: dir.path)
        #expect(kernel.agent(AgentId("memo")) != nil)
        #expect(kernel.root?.metaValue(forKey: "guid")?.asString == "deadbeef")
    }

    @Test func aVirginDirIsSeeded() async throws {
        let dir = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("fantastic-virgin-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        _ = try await FantasticCLI.bootDiskBacked(cwd: dir.path)
        #expect(
            FileManager.default.fileExists(
                atPath: dir.appendingPathComponent(".fantastic/agent.json").path))
    }

    // A14.6: a one-shot boots the ROOT LOADER before the call — the
    // observable effect is the debounced record writer existing afterward
    // (installed only by the root's OWN `boot` verb, `kernel_state.tools`).
    // Before this fix, rust/swift never sent that boot at all: no pump, no
    // ledger, no record writer for a bare one-shot RPC.
    @Test func oneShotBootsTheRootBeforeTheCall() async throws {
        let dir = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("fantastic-oneshot-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let kernel = try await FantasticCLI.bootDiskBacked(cwd: dir.path)
        #expect(kernel.recordWriter == nil, "not installed before the one-shot's own boot send")
        let reply = await FantasticCLI.dispatchOneShot(
            kernel: kernel, target: "kernel", verb: "reflect", rest: [])
        #expect(reply["id"].asString == "kernel_state")
        #expect(kernel.recordWriter != nil, "the root's boot verb installs it")
    }

    /// A14.18: every non-daemon mode ends with an idempotent kernel shutdown
    /// (python's `atexit` → `Kernel.shutdown`) — after `dispatchOneShot`'s
    /// own root boot, calling `shutdown()` (what `runOneShotRPC` does once
    /// the lock is released) must complete cleanly and mark it done, exactly
    /// as it would from the real process-exit path.
    @Test func oneShotRunsKernelShutdownAtExit() async throws {
        let dir = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("fantastic-oneshot-shutdown-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let kernel = try await FantasticCLI.bootDiskBacked(cwd: dir.path)
        _ = await FantasticCLI.dispatchOneShot(
            kernel: kernel, target: "kernel", verb: "reflect", rest: [])
        #expect(!kernel.shutdownComplete)
        await kernel.shutdown()
        #expect(kernel.shutdownComplete)
        // Idempotent — a second call (the atexit safety net, if the in-loop
        // path already ran) is a no-op, never a crash or a double walk.
        await kernel.shutdown()
        #expect(kernel.shutdownComplete)
    }

    // A14.3: `init <loader_bundle> <manifest>` creates the loader agent
    // under the root and boots it (its own boot reads the manifest — the
    // kernel never parses YAML).
    //
    // The manifest read obeys the running-dir law (`file_bridge`'s cold
    // bootstrap clamp is the PROCESS's real cwd, independent of the `cwd`
    // `bootDiskBacked` passes to `startKernel` for the kernel's OWN
    // `.fantastic` root — measured, not assumed). Sibling suites share this
    // process and cannot `chdir`, so the fixture comes to the real cwd under
    // a unique name, exactly as `FantasticBundlesTests/StaticLoaderTests`
    // already does for the same bundle.
    @Test func initCreatesTheLoaderUnderTheRoot() async throws {
        let dir = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("fantastic-init-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let fixtureName = ".init_fixture_\(UUID().uuidString.prefix(8))"
        let fixtureDir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(fixtureName)
        try FileManager.default.createDirectory(at: fixtureDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: fixtureDir) }
        let manifestPath = "\(fixtureName)/fantastic.yaml"
        try "agents: []\n".write(
            to: fixtureDir.appendingPathComponent("fantastic.yaml"), atomically: true,
            encoding: .utf8)

        let kernel = try await FantasticCLI.bootDiskBacked(cwd: dir.path)
        let reply = await FantasticCLI.dispatchInit(
            kernel: kernel, loaderName: "static_loader", manifest: manifestPath)
        #expect(reply["id"].asString == "static_loader", "\(reply.serialize())")
        #expect(reply["handler_module"].asString == "static_loader.tools")
        #expect(kernel.agent(AgentId("static_loader")) != nil)
    }
}
