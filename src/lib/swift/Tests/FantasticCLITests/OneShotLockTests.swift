// ONE-SHOTS PROBE THE OS LOCK, NOT THE PID (owner 2026-10-03) — subprocess
// level, the real binary. Twin of python `test_one_shots_probe_the_flock_not_the_pid`
// and rust `one_shots_take_over_a_lock_whose_pid_holds_no_flock`.
//
//   - a lock file naming a LIVE pid that holds no flock (a dead holder's file,
//     its pid reused) is stale: `call` and `init` take it over and release it;
//   - a held flock (this test process takes the daemon's own lock) refuses them
//     with python's `[call]` / `[init]` line, exit 1.

#if os(macOS)

    import FantasticKernel
    import Foundation
    import Testing

    #if canImport(Darwin)
        import Darwin
    #endif

    private let launchEnvironment: [String: String] = [
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": NSHomeDirectory(),
    ]

    private var binary: URL {
        let pkg = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // FantasticCLITests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // src/lib/swift
        let bin = pkg.appendingPathComponent(".build/debug/fantastic_kernel")
        precondition(
            FileManager.default.isExecutableFile(atPath: bin.path),
            "fantastic_kernel is not built at \(bin.path)")
        return bin
    }

    private func tmpDir() -> URL {
        let u = FileManager.default.temporaryDirectory
            .appendingPathComponent("fantastic-oneshotlock-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(
            at: u.appendingPathComponent(".fantastic"), withIntermediateDirectories: true)
        return u
    }

    /// Run the binary to completion: (exit status, stdout, stderr).
    private func run(_ dir: URL, _ args: [String]) throws -> (Int32, String, String) {
        let p = Process()
        p.executableURL = binary
        p.arguments = args
        p.currentDirectoryURL = dir
        p.environment = launchEnvironment
        let out = Pipe()
        let err = Pipe()
        p.standardOutput = out
        p.standardError = err
        p.standardInput = FileHandle.nullDevice
        try p.run()
        let o = out.fileHandleForReading.readDataToEndOfFile()
        let e = err.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return (p.terminationStatus, String(decoding: o, as: UTF8.self), String(decoding: e, as: UTF8.self))
    }

    private func lockPath(_ dir: URL) -> String {
        dir.appendingPathComponent(".fantastic/lock.yaml").path
    }

    @Suite("one-shot lock — the flock, not the pid", .serialized)
    struct OneShotLockTests {

        @Test func oneShotsTakeOverALockWhosePidHoldsNoFlock() throws {
            let dir = tmpDir()
            defer { try? FileManager.default.removeItem(at: dir) }
            let stale = { try "pid: \(getppid())\n".write(toFile: lockPath(dir), atomically: true, encoding: .utf8) }
            try stale()
            let (code, out, err) = try run(dir, ["kernel", "list_agents"])
            #expect(code == 0, "\(err)")
            #expect(out.contains("\"agents\""), "\(out)")
            #expect(!FileManager.default.fileExists(atPath: lockPath(dir)), "released")
            try stale()
            try "agents: []\n".write(
                to: dir.appendingPathComponent("fantastic.yaml"), atomically: true, encoding: .utf8)
            let (icode, _, ierr) = try run(dir, ["init", "static_loader", "./fantastic.yaml"])
            #expect(icode == 0, "\(ierr)")
            #expect(!FileManager.default.fileExists(atPath: lockPath(dir)), "released")
        }

        @Test func aHeldFlockRefusesOneShotsWithPythonText() throws {
            let dir = tmpDir()
            defer { try? FileManager.default.removeItem(at: dir) }
            let lock = WorkdirLock(workdir: dir)
            try lock.acquire()
            defer { lock.release() }
            let pid = getpid()
            let (code, out, err) = try run(dir, ["kernel", "list_agents"])
            #expect(code == 1)
            #expect(out.isEmpty, "\(out)")
            #expect(
                err == "[call] another fantastic owns this dir (pid=\(pid)). Stop it, or forward "
                    + "your call over the WS bridge (see ws_bridge bundle).\n", "\(err)")
            let (icode, iout, ierr) = try run(dir, ["init", "static_loader", "./fantastic.yaml"])
            #expect(icode == 1)
            #expect(iout.isEmpty, "\(iout)")
            #expect(ierr == "[init] another fantastic owns this dir (pid=\(pid)). Stop it first.\n", "\(ierr)")
        }
    }

#endif
