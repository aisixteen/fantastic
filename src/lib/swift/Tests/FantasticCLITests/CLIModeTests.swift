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

        try #"{"id": "core", "handler_module": null, "parent_id": null, "guid": "deadbeef"}"#
            .write(
                to: dot.appendingPathComponent("agent.json"), atomically: true, encoding: .utf8)
        try #"{"id": "memo", "handler_module": "cli.tools", "parent_id": "core"}"#
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
}
