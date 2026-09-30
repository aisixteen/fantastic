// THE DAEMON AND THE MEMORY STORE — subprocess level, the real binary.
//
// The kernel keeps no conversation and writes no message log: its execution rows
// are flushed into the root's MEMORY STORE (`memory_store_id`), and the `ledger`
// verb is a projection over that store plus what is still in RAM. Both CLI
// shapes are pinned here because each has its own path:
//
//   - the DAEMON runs `kernel_start`: a root with no `memory_store_id` does not
//     start (fail loud, exit non-zero, the hint names ram_store); with one, live
//     calls over the REST leg land in the store and `ledger` reads them back;
//     SIGTERM → `Kernel.shutdown()` → exit 0, the id counter's bound on disk.
//   - the ONE-SHOT runs an UNSTARTED kernel: no store required, so an admin can
//     create the store and name it before the first `kernel_start`.

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation
    import Testing

    #if canImport(Darwin)
        import Darwin
    #endif

    /// The WHOLE environment every launched binary gets — fixed, never inherited.
    ///
    /// An inherited environment makes `posix_spawn` walk the process-global
    /// `environ` while suites running in parallel `setenv`/`unsetenv` their own
    /// keys (the password rules, dotenv, the shell tests): a walk racing a
    /// reallocation reads freed memory and the spawn fails `EFAULT` — "Bad
    /// address" (measured standalone: 2 of 12,000 inherited-env spawns failed
    /// with a mutating thread beside them, 0 of 12,000 with an explicit
    /// environment). It was first read as "`Process.run()` is not thread-safe"
    /// and papered over with a per-file lock, which could not reach the other
    /// suites' `setenv`. The binary needs nothing from the environment; a fixed
    /// one is also hermetic — no developer key or config leaks into the daemon.
    private let launchEnvironment: [String: String] = [
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": NSHomeDirectory(),
    ]

    /// `proc.run()` with the fixed environment above.
    private func launch(_ proc: Process) throws {
        proc.environment = launchEnvironment
        try proc.run()
    }

    @Suite("daemon + one-shot: the memory store", .serialized)
    struct DaemonLedgerTests {

        // ── harness ────────────────────────────────────────────────

        /// The built `fantastic_kernel`. Missing ⇒ the run is not a run: no
        /// skip, no fallback binary.
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
                .appendingPathComponent("fantastic-ledger-\(UUID().uuidString)")
            try? FileManager.default.createDirectory(at: u, withIntermediateDirectories: true)
            return u
        }

        /// A loopback port the OS just handed out. LOUD on failure — 0 is a
        /// legal port hint and would silently produce `127.0.0.1:0`.
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

        /// One `fantastic <argv>` one-shot in `cwd`. Returns (stdout, exit).
        @discardableResult
        private func oneShot(_ cwd: URL, _ argv: [String]) throws -> (String, Int32) {
            let proc = Process()
            proc.executableURL = binary
            proc.currentDirectoryURL = cwd
            proc.arguments = argv
            let out = Pipe()
            proc.standardOutput = out
            proc.standardError = Pipe()
            proc.standardInput = FileHandle.nullDevice
            try launch(proc)
            let data = out.fileHandleForReading.readDataToEndOfFile()
            proc.waitUntilExit()
            return (String(data: data, encoding: .utf8) ?? "", proc.terminationStatus)
        }

        /// Seed the record store, the memory store (named by the root), the web
        /// host and its REST leg, all through the CLI, exactly as an operator
        /// would. Every reply is checked: a silent setup failure would make the
        /// ledger assertion meaningless.
        private func seed(_ dir: URL, port: UInt16, memory: Bool = true) throws {
            var steps: [[String]] = [
                [
                    "kernel", "create_agent", "parent_id=kernel_state",
                    "handler_module=file_bridge.tools", "id=store", "root=.fantastic", #"allow=["."]"#,
                    "ingress_rule=allow_all_ingress",
                    // BOTH SIDES, granted from above: the leg's `can_call`
                    // alone is half the gate.
                    "callable_by={\"rest\": \"*\", \"$self\": \"*\", \"$parent\": \"*\", \"$children\": \"*\"}",
                ],
                [
                    "kernel", "create_agent", "parent_id=kernel_state",
                    "handler_module=web.tools", "id=web", "port=\(port)",
                ],
                [
                    "kernel", "create_agent", "parent_id=web",
                    "handler_module=web_rest.tools", "id=rest",
                    "ingress_rule=allow_all_ingress",
                    "can_call={\"store\": \"*\", \"kernel_state\": [\"ledger\"]}",
                ],
            ]
            if memory {
                steps += [
                    [
                        "kernel", "create_agent", "parent_id=kernel_state",
                        "handler_module=ram_store.tools", "id=memory", "operator=true",
                        "admins=[\"kernel\"]",
                    ],
                    ["kernel", "update_agent", "id=kernel_state", "memory_store_id=memory"],
                ]
            }
            for argv in steps {
                let (out, code) = try oneShot(dir, argv)
                #expect(code == 0, "\(argv) exited \(code): \(out)")
                #expect(!out.contains("\"error\""), "\(argv): \(out)")
            }
        }

        /// Everything the daemon has printed so far, grown off the pipe.
        private final class Tail: @unchecked Sendable {
            private let lock = NSLock()
            private var buf = Data()
            func append(_ d: Data) {
                lock.lock()
                buf.append(d)
                lock.unlock()
            }
            var text: String {
                lock.lock()
                defer { lock.unlock() }
                return String(data: buf, encoding: .utf8) ?? ""
            }
        }

        /// Poll until `cond`, or fail after `seconds`. No fixed sleeps: the
        /// wait ends the moment the condition holds.
        private func wait(
            _ seconds: Double, _ what: String, _ detail: @autoclosure () -> String,
            _ cond: () -> Bool
        ) async {
            let deadline = Date().addingTimeInterval(seconds)
            while Date() < deadline {
                if cond() { return }
                try? await Task.sleep(nanoseconds: 20_000_000)
            }
            Issue.record("timed out waiting for \(what): \(detail())")
        }

        /// The root record on disk.
        private func rootRecord(_ dir: URL) throws -> JSON {
            try YamlIO.load(
                try String(
                    contentsOf: dir.appendingPathComponent(".fantastic/agent.yaml"),
                    encoding: .utf8))
        }

        // ── the daemon path ────────────────────────────────────────

        @Test func daemonFlushesTheLastExecutionOnSigterm() async throws {
            let dir = tmpDir()
            defer { try? FileManager.default.removeItem(at: dir) }
            let port = freePort()
            try seed(dir, port: port)

            let proc = Process()
            proc.executableURL = binary
            proc.currentDirectoryURL = dir
            proc.arguments = []
            proc.standardInput = FileHandle.nullDevice
            let outPipe = Pipe()
            let errPipe = Pipe()
            proc.standardOutput = outPipe
            proc.standardError = errPipe
            let tail = Tail()
            outPipe.fileHandleForReading.readabilityHandler = { tail.append($0.availableData) }
            errPipe.fileHandleForReading.readabilityHandler = { tail.append($0.availableData) }
            try launch(proc)
            defer {
                if proc.isRunning { kill(proc.processIdentifier, SIGKILL) }
                outPipe.fileHandleForReading.readabilityHandler = nil
                errPipe.fileHandleForReading.readabilityHandler = nil
            }

            await wait(60, "[kernel] up", tail.text) { tail.text.contains("[kernel] up") }

            // THREE NAMED CALLS over the REST leg — the octet leg names its
            // execution from `X-Fantastic-Execution-Id`.
            let session = URLSession(configuration: .ephemeral)
            let names = ["daemon-call-1", "daemon-call-2", "daemon-call-3"]
            for name in names {
                var req = URLRequest(
                    url: URL(string: "http://127.0.0.1:\(port)/rest/store")!)
                req.httpMethod = "POST"
                req.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
                req.setValue(
                    "{\"type\":\"list\",\"path\":\"\"}", forHTTPHeaderField: "X-Fantastic-Payload")
                req.setValue(name, forHTTPHeaderField: "X-Fantastic-Execution-Id")
                req.httpBody = Data()
                let (d, resp) = try await session.data(for: req)
                let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
                #expect(code == 200, "\(name): \(code) \(String(data: d, encoding: .utf8) ?? "")")
                #expect(
                    !(String(data: d, encoding: .utf8) ?? "").contains("\"error\""),
                    "\(name): \(String(data: d, encoding: .utf8) ?? "")")
            }

            // THE LEDGER — a projection over the kernel's memory store (the
            // flusher has had its debounce) plus RAM, scoped to the caller: the
            // REST leg made these calls, so it sees them, results kept (named).
            try await Task.sleep(nanoseconds: 500_000_000)
            var lreq = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/rest/kernel")!)
            lreq.httpMethod = "POST"
            lreq.setValue("application/json", forHTTPHeaderField: "Content-Type")
            lreq.httpBody = Data("{\"type\":\"ledger\"}".utf8)
            let (ld, _) = try await session.data(for: lreq)
            let ledger = try JSON.parse(ld)
            let executions = ledger["executions"].asArray ?? []
            for name in names {
                let e = executions.first { $0["name"].asString == name }
                #expect(e?["status"].asString == "completed", "\(name): \(ledger.serialize())")
                #expect(e?["outcome"]["result"] != nil && e?["outcome"]["result"] != .null,
                    "a named execution keeps its result: \(ledger.serialize())")
            }

            // SIGTERM — the graceful path python takes.
            kill(proc.processIdentifier, SIGTERM)
            await wait(60, "the daemon to exit", tail.text) { !proc.isRunning }
            proc.waitUntilExit()
            #expect(proc.terminationStatus == 0, "exit \(proc.terminationStatus): \(tail.text)")
            // THE COUNTER'S BOUND IS ON DISK, beside the guid: a restart never
            // repeats an id. No message log was written.
            let rec = try rootRecord(dir)
            #expect(rec["next_id"].asInt != nil, "\(rec.serialize())")
            #expect(rec["guid"].asString?.isEmpty == false)
            #expect(
                !FileManager.default.fileExists(
                    atPath: dir.appendingPathComponent(".fantastic/messages.yaml").path))
        }

        @Test func aDaemonWithoutAMemoryStoreFailsLoud() async throws {
            let dir = tmpDir()
            defer { try? FileManager.default.removeItem(at: dir) }
            try seed(dir, port: freePort(), memory: false)
            let proc = Process()
            proc.executableURL = binary
            proc.currentDirectoryURL = dir
            proc.arguments = []
            proc.standardInput = FileHandle.nullDevice
            let errPipe = Pipe()
            proc.standardOutput = Pipe()
            proc.standardError = errPipe
            try launch(proc)
            let err = errPipe.fileHandleForReading.readDataToEndOfFile()
            proc.waitUntilExit()
            let text = String(data: err, encoding: .utf8) ?? ""
            #expect(proc.terminationStatus != 0, "a root with no store started: \(text)")
            #expect(text.contains("memory_store_id"), "\(text)")
            #expect(text.contains("create a ram_store"), "the hint names what to create: \(text)")
        }

        // ── the one-shot path ──────────────────────────────────────

        /// A one-shot runs an UNSTARTED kernel: no store is required, so the
        /// admin creates the store and names it before the first start.
        @Test func aOneShotNeedsNoStoreAndCanNameOne() async throws {
            let dir = tmpDir()
            defer { try? FileManager.default.removeItem(at: dir) }
            for argv in [
                [
                    "kernel", "create_agent", "parent_id=kernel_state",
                    "handler_module=file_bridge.tools", "id=store", "root=.fantastic", #"allow=["."]"#,
                    "ingress_rule=allow_all_ingress",
                ],
                [
                    "kernel", "create_agent", "parent_id=kernel_state",
                    "handler_module=ram_store.tools", "id=memory", "operator=true",
                ],
                ["kernel", "update_agent", "id=kernel_state", "memory_store_id=memory"],
            ] {
                let (out, code) = try oneShot(dir, argv)
                #expect(code == 0 && !out.contains("\"error\""), "\(argv): \(out)")
            }
            #expect(try rootRecord(dir)["memory_store_id"].asString == "memory")
        }
    }

#endif
