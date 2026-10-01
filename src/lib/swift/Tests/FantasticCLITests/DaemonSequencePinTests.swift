// THE DAEMON'S START/STOP SEQUENCE — subprocess level, the real binary.
//
// The daemon runs in a child process, so the in-process coverage tools never see
// it; `DaemonLedgerTests` drives the store/ledger half. This file pins the rest
// of python `longrun._default`'s sequence as an operator observes it:
//
//   - nothing to serve (no web agent, stdin not a tty) → return silently, exit
//     0, and the lock is never taken;
//   - while a daemon holds the dir, a second one is refused with python's three
//     lines naming the live pid, exit 1, and the first keeps running;
//   - `[kernel] up` goes to STDOUT; on SIGTERM the signal line, then
//     `[kernel] tearing down agents...`, then `[kernel] down` go to STDERR in
//     that order, the lock is released, exit 0.

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation
    import Testing

    #if canImport(Darwin)
        import Darwin
    #endif

    /// A fixed environment (see `DaemonLedgerTests.launchEnvironment`): an
    /// inherited one races other suites' `setenv`.
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
            .appendingPathComponent("fantastic-daemonseq-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: u, withIntermediateDirectories: true)
        return u
    }

    private func freePort() -> UInt16 {
        for _ in 0..<64 {
            let fd = socket(AF_INET, Int32(SOCK_STREAM), 0)
            guard fd >= 0 else { continue }
            defer { close(fd) }
            var addr = sockaddr_in()
            addr.sin_family = sa_family_t(AF_INET)
            addr.sin_port = 0
            addr.sin_addr.s_addr = inet_addr("127.0.0.1")
            let ok = withUnsafePointer(to: &addr) { p in
                p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
                }
            }
            guard ok == 0 else { continue }
            var out = sockaddr_in()
            var len = socklen_t(MemoryLayout<sockaddr_in>.size)
            let named = withUnsafeMutablePointer(to: &out) { p in
                p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    getsockname(fd, $0, &len)
                }
            }
            guard named == 0 else { continue }
            let port = UInt16(bigEndian: out.sin_port)
            if port != 0 { return port }
        }
        fatalError("freePort: the OS handed out no loopback port in 64 probes")
    }

    /// Grows off a pipe as the child writes.
    private final class Tail: @unchecked Sendable {
        private let lock = NSLock()
        private var buf = Data()
        func append(_ d: Data) { lock.withLock { buf.append(d) } }
        var text: String { lock.withLock { String(data: buf, encoding: .utf8) ?? "" } }
    }

    /// A launched `fantastic` with stdout and stderr captured separately.
    private struct Run {
        let proc: Process
        let out: Tail
        let err: Tail
        let pipes: (Pipe, Pipe)

        func detach() {
            pipes.0.fileHandleForReading.readabilityHandler = nil
            pipes.1.fileHandleForReading.readabilityHandler = nil
        }
    }

    private func start(_ cwd: URL, _ argv: [String] = []) throws -> Run {
        let proc = Process()
        proc.executableURL = binary
        proc.currentDirectoryURL = cwd
        proc.arguments = argv
        proc.standardInput = FileHandle.nullDevice
        let o = Pipe()
        let e = Pipe()
        proc.standardOutput = o
        proc.standardError = e
        let out = Tail()
        let err = Tail()
        o.fileHandleForReading.readabilityHandler = { out.append($0.availableData) }
        e.fileHandleForReading.readabilityHandler = { err.append($0.availableData) }
        proc.environment = launchEnvironment
        try proc.run()
        return Run(proc: proc, out: out, err: err, pipes: (o, e))
    }

    private func oneShot(_ cwd: URL, _ argv: [String]) throws -> (String, Int32) {
        let r = try start(cwd, argv)
        r.proc.waitUntilExit()
        // the handlers may still be draining the last chunk
        Thread.sleep(forTimeInterval: 0.05)
        r.detach()
        return (r.out.text, r.proc.terminationStatus)
    }

    private func wait(
        _ seconds: Double, _ what: String, _ detail: @autoclosure () -> String, _ cond: () -> Bool
    )
        async
    {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if cond() { return }
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        Issue.record("timed out waiting for \(what): \(detail())")
    }

    /// A store, a memory store named by the root, and a web host on `port` —
    /// the least a daemon serves.
    private func seedServable(_ dir: URL, port: UInt16) throws {
        for argv in [
            [
                "kernel", "create_agent", "parent_id=kernel_state",
                "handler_module=file_bridge.tools", "id=store", "root=.fantastic", #"allow=["."]"#,
                "ingress_rule=allow_all_ingress",
            ],
            [
                "kernel", "create_agent", "parent_id=kernel_state", "handler_module=web.tools",
                "id=web", "port=\(port)",
            ],
            [
                "kernel", "create_agent", "parent_id=kernel_state",
                "handler_module=ram_store.tools", "id=memory", "operator=true",
                #"admins=["kernel"]"#,
            ],
            ["kernel", "update_agent", "id=kernel_state", "memory_store_id=memory"],
        ] {
            let (out, code) = try oneShot(dir, argv)
            #expect(code == 0 && !out.contains("\"error\""), "\(argv) exited \(code): \(out)")
        }
    }

    private func lockPath(_ dir: URL) -> String {
        dir.appendingPathComponent(".fantastic/lock.yaml").path
    }

    @Suite("daemon: the start/stop sequence", .serialized)
    struct DaemonSequencePinTests {

        @Test(.timeLimit(.minutes(1)))
        func nothingToServeReturnsSilentlyWithoutTheLock() async throws {
            let dir = tmpDir()
            defer { try? FileManager.default.removeItem(at: dir) }
            let r = try start(dir)
            r.proc.waitUntilExit()
            try await Task.sleep(nanoseconds: 50_000_000)
            r.detach()
            #expect(r.proc.terminationStatus == 0, "\(r.err.text)")
            #expect(r.out.text == "", "\(r.out.text)")
            #expect(!r.err.text.contains("[kernel] up"))
            #expect(!FileManager.default.fileExists(atPath: lockPath(dir)), "no lock is ever taken")
        }

        @Test(.timeLimit(.minutes(2)))
        func aSecondDaemonIsRefusedAndSigtermTearsDownInOrder() async throws {
            let dir = tmpDir()
            defer { try? FileManager.default.removeItem(at: dir) }
            try seedServable(dir, port: freePort())

            let first = try start(dir)
            defer {
                if first.proc.isRunning { kill(first.proc.processIdentifier, SIGKILL) }
                first.detach()
            }
            await wait(60, "[kernel] up", first.out.text + first.err.text) {
                first.out.text.contains("[kernel] up")
            }
            #expect(first.out.text.contains("[kernel] up\n"), "the up marker is STDOUT")
            #expect(!first.err.text.contains("[kernel] up"))
            #expect(
                FileManager.default.fileExists(atPath: lockPath(dir)), "the daemon holds the lock")

            // a second daemon in the same dir is refused, naming the live pid
            let second = try start(dir)
            second.proc.waitUntilExit()
            try await Task.sleep(nanoseconds: 50_000_000)
            second.detach()
            let pid = first.proc.processIdentifier
            #expect(second.proc.terminationStatus == 1, "\(second.err.text)")
            #expect(
                second.err.text.hasSuffix(
                    "[kernel] another fantastic owns this dir: pid=\(pid)\n"
                        + "  -> kill it:  kill \(pid)\n"
                        + "  -> or, if stale, remove the lock:  rm .fantastic/lock.yaml\n"),
                "\(second.err.text)")
            #expect(first.proc.isRunning, "the refusal must not disturb the owner")

            // SIGTERM: the signal line, the teardown bracket, in order, on stderr
            kill(pid, SIGTERM)
            await wait(60, "the daemon to exit", first.err.text) { !first.proc.isRunning }
            first.proc.waitUntilExit()
            try await Task.sleep(nanoseconds: 50_000_000)
            #expect(
                first.proc.terminationStatus == 0,
                "exit \(first.proc.terminationStatus): \(first.err.text)")
            let err = first.err.text
            let sig = err.range(of: "\n[kernel] SIGTERM — shutting down...\n")
            let down = err.range(of: "[kernel] tearing down agents...\n")
            let gone = err.range(of: "[kernel] down\n")
            #expect(sig != nil && down != nil && gone != nil, "\(err)")
            if let sig, let down, let gone {
                #expect(
                    sig.upperBound <= down.lowerBound && down.upperBound <= gone.lowerBound,
                    "\(err)")
            }
            #expect(!first.out.text.contains("tearing down"), "the teardown lines are STDERR")
            #expect(!FileManager.default.fileExists(atPath: lockPath(dir)), "the lock is released")
        }
    }

#endif
