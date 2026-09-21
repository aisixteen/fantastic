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
            "FANTASTIC_CTRL_FD", "FANTASTIC_SELF_ID",
            #"_st.pack(">I""#, "_o._exit(0)",
            // The write half of the 16 MiB bound: the client refuses to send a
            // frame the relay would close the socket over, and says what to do
            // instead. Naming it here keeps the two ends from drifting apart.
            "_MAX_FRAME", "frame exceeds 16 MiB", "write_stream",
        ] {
            #expect(CONNECTOR_SRC.contains(needle))
        }
    }
}

// ─── the control-socket handoff ─────────────────────────────────

/// THE decisive test for the durable design. Everything else — relay, inbox,
/// self-reap — is built on the child actually receiving the socket.
///
/// It also pins that the `posix_spawnp` file actions really do land the socket on fd
/// 3. If that ever stops holding, this fails here rather than as a mysterious hang
/// inside the relay.
@Suite("python spawn")
struct SpawnTests {

    private func python3() -> String? {
        for p in ["/usr/bin/python3", "/opt/homebrew/bin/python3", "/usr/local/bin/python3"]
        where FileManager.default.isExecutableFile(atPath: p) { return p }
        return nil
    }

    @Test func childSeesTheControlSocketAndItsEnvelope() async throws {
        guard let py = python3() else { print("skipping: no python3"); return }
        let code = """
            import os, stat
            fd = int(os.environ["FANTASTIC_CTRL_FD"])
            print("sock", stat.S_ISSOCK(os.fstat(fd).st_mode), "self", os.environ["FANTASTIC_SELF_ID"], "env", os.environ.get("MYVAR"))
            """
        let s = try Spawn.job(
            argv: [py, "-u", "-c", code], selfId: "owner_agent",
            envExtra: ["MYVAR": "hello"], cwd: nil)
        let data = s.stdout.readDataToEndOfFile()
        _ = await s.child.wait()
        close(s.ctrlFd)
        let text = String(data: data, encoding: .utf8) ?? ""
        #expect(text.contains("sock True"))
        #expect(text.contains("self owner_agent"))
        #expect(text.contains("env hello"))
    }

    /// `close_fds=True`: the child holds 0/1/2/3 and nothing else. A stray descriptor
    /// of the kernel — another job's control socket above all — must not cross the
    /// spawn, or that job's child never sees EOF and never self-reaps.
    @Test func childInheritsNoStrayDescriptor() async throws {
        guard let py = python3() else { print("skipping: no python3"); return }
        var low: [Int32] = [0, 0]
        #expect(pipe(&low) == 0)  // NOT close-on-exec, like a socket mid-`socketpair`
        // Moved HIGH: a low number is one the child legitimately reuses (3 is its
        // control socket, and the interpreter opens its own low descriptors).
        let stray = low.map { fcntl($0, F_DUPFD, 200) }
        low.forEach { close($0) }
        #expect(stray.allSatisfy { $0 >= 200 })
        defer { stray.forEach { close($0) } }
        let code = """
            import os
            def held(fd):
                try: os.fstat(fd); return True
                except OSError: return False
            print("stray", held(\(stray[0])), held(\(stray[1])), "stdin", held(0), "ctrl", held(3))
            """
        let s = try Spawn.job(argv: [py, "-u", "-c", code], selfId: "owner_agent", envExtra: [:], cwd: nil)
        let data = s.stdout.readDataToEndOfFile()
        _ = await s.child.wait()
        close(s.ctrlFd)
        let text = String(data: data, encoding: .utf8) ?? ""
        #expect(text.contains("stray False False stdin True ctrl True"), "got: \(text)")
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
            argv: [
                py, "-u", "-c",
                CONNECTOR_SRC + "\nimport threading\nthreading.Event().wait()\n",
            ],
            selfId: "owner_agent", envExtra: [:], cwd: nil)
        Thread.sleep(forTimeInterval: 0.5)
        #expect(s.child.isAlive, "child exited early")

        close(s.ctrlFd)  // the only thing holding it open
        let deadline = Date().addingTimeInterval(10)
        while s.child.isAlive && Date() < deadline { Thread.sleep(forTimeInterval: 0.05) }
        #expect(!s.child.isAlive, "child did not self-reap within 10s")
        #expect(s.child.exitCode == 0, "self-reap should exit 0")
    }
}
