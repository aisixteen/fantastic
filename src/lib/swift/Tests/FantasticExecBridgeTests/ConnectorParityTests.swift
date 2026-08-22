// The vendored connector must be byte-identical to python's.
//
// A copy without this test is worse than no copy: drift surfaces as a program that
// runs on one kernel and mysteriously fails on another — the hardest class of bug to
// trace. Skips (loudly) when the python tree is absent, so a swift-only checkout
// still builds.

import Foundation
import Testing

@testable import FantasticExecBridge

@Suite("connector parity")
struct ConnectorParityTests {

    @Test func vendoredConnectorMatchesPythonByteForByte() throws {
        let py = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // FantasticExecBridgeTests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // swift
            .deletingLastPathComponent()  // lib
            .appendingPathComponent(
                "python/bundled_agents/io/exec_bridge/src/exec_bridge/_connector.py")
        guard let src = try? String(contentsOf: py, encoding: .utf8) else {
            print("skipping: python tree not present at \(py.path)")
            return
        }
        let open = "CONNECTOR_SRC = r\"\"\""
        guard let a = src.range(of: open), let b = src.range(of: "\"\"\"", options: .backwards)
        else {
            Issue.record("python _connector.py has no CONNECTOR_SRC")
            return
        }
        let expected = String(src[a.upperBound..<b.lowerBound])
        if expected != CONNECTOR_SRC {
            let msg = "vendored connector drifted from \(py.path) — re-vendor it; the "
                + "child half of the wire is shared by every runtime and must not fork"
            Issue.record(Comment(rawValue: msg))
        }
    }

    /// Runs even in a swift-only checkout, where the test above skips.
    @Test func vendoredConnectorCarriesTheWireContract() {
        for needle in [
            "FANTASTIC_CTRL_FD", "FANTASTIC_SELF_ID", "FANTASTIC_ARGS",
            #"_st.pack(">I""#, "_o._exit(0)",
        ] {
            #expect(CONNECTOR_SRC.contains(needle))
        }
    }
}

// ─── the control-socket handoff ─────────────────────────────────

/// THE decisive test for the durable design. Everything else — relay, inbox,
/// self-reap — is built on the child actually receiving the socket.
///
/// It also pins the empirical finding that Foundation's `Process` passes an inherited
/// fd at all. If a toolchain ever turns on POSIX_SPAWN_CLOEXEC_DEFAULT, this fails
/// here rather than as a mysterious hang inside the relay.
@Suite("python spawn")
struct SpawnTests {

    private func python3() -> String? {
        for p in ["/usr/bin/python3", "/opt/homebrew/bin/python3", "/usr/local/bin/python3"]
        where FileManager.default.isExecutableFile(atPath: p) { return p }
        return nil
    }

    @Test func childSeesTheControlSocketAndItsEnvelope() throws {
        guard let py = python3() else { print("skipping: no python3"); return }
        let code = """
            import os, stat
            fd = int(os.environ["FANTASTIC_CTRL_FD"])
            print("sock", stat.S_ISSOCK(os.fstat(fd).st_mode), "self", os.environ["FANTASTIC_SELF_ID"], "args", os.environ["FANTASTIC_ARGS"])
            """
        let s = try Spawn.job(
            interp: py, code: code, selfId: "owner_agent", argsJSON: #"{"n":41}"#, cwd: nil)
        let data = s.stdout.fileHandleForReading.readDataToEndOfFile()
        s.process.waitUntilExit()
        close(s.ctrlFd)
        let text = String(data: data, encoding: .utf8) ?? ""
        #expect(text.contains("sock True"))
        #expect(text.contains("self owner_agent"))
        #expect(text.contains(#"args {"n":41}"#))
    }

    /// The lifeline: drop the parent's socket and the child reaps ITSELF, even parked
    /// forever. This is what stops a dying kernel leaving orphans — and it only works
    /// because `Spawn.job` closes its copy of the child end.
    @Test func theChildSelfReapsWhenTheControlSocketCloses() throws {
        guard let py = python3() else { print("skipping: no python3"); return }
        // The self-reap lives in the CONNECTOR (its reader thread sees EOF and
        // `_o._exit(0)`s) — the shim is opt-in now, so this program takes it the
        // way a parked durable tool would.
        let s = try Spawn.job(
            interp: py,
            code: CONNECTOR_SRC + "\nimport threading\nthreading.Event().wait()\n",
            selfId: "owner_agent", argsJSON: "{}", cwd: nil)
        Thread.sleep(forTimeInterval: 0.5)
        #expect(s.process.isRunning, "child exited early")

        close(s.ctrlFd)  // the only thing holding it open
        let deadline = Date().addingTimeInterval(10)
        while s.process.isRunning && Date() < deadline { Thread.sleep(forTimeInterval: 0.05) }
        #expect(!s.process.isRunning, "child did not self-reap within 10s")
        #expect(s.process.terminationStatus == 0, "self-reap should exit 0")
    }
}
