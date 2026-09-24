// TAPE DURABILITY AT SHUTDOWN — subprocess level, the real binary.
//
// ONE TAPE (2026-09-24): there is no `ledger.json`. The LAST execution a run
// performed is on disk in `.fantastic/messages.xml` when the process is gone —
// its `call` row WITH the dispatch payload, its `settle` row WITH the whole
// outcome body — and the execution ledger is rebuilt from exactly those rows at
// the next boot. Both CLI shapes are pinned here because each has its own exit
// path:
//
//   - the DAEMON: `[kernel] up`, live calls over the REST leg, SIGTERM →
//     `Kernel.shutdown()` → exit 0.
//   - the ONE-SHOT: one call, then the process returns from `main`.
//
// In-process tests cannot see either: what matters is whether the process lands
// the pump's last window before the lock is released.

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation
    import Testing

    #if canImport(Darwin)
        import Darwin
    #endif

    /// Foundation's `Process.run()` is NOT safe to call from two threads at
    /// once: the suites around this one launch in parallel and posix_spawn came
    /// back `EFAULT` (measured once in six full runs). Every launch in this file
    /// goes through this one PROCESS-WIDE lock — swift-testing builds a fresh
    /// suite instance per test, so a stored property would be a lock per test.
    private let spawnLock = NSLock()

    /// `proc.run()`, serialised. Sync on purpose: `NSLock` is refused inside an
    /// async frame.
    private func launch(_ proc: Process) throws {
        spawnLock.lock()
        defer { spawnLock.unlock() }
        try proc.run()
    }

    @Suite("daemon + one-shot: the tape survives exit", .serialized)
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

        /// Seed the store + the web host + its REST leg, all through the CLI,
        /// exactly as an operator would. Every reply is checked: a silent
        /// setup failure would make the ledger assertion meaningless.
        private func seed(_ dir: URL, port: UInt16) throws {
            for argv in [
                [
                    "kernel", "create_agent", "parent_id=kernel_state",
                    "handler_module=file_bridge.tools", "id=store", "root=.fantastic",
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
                    "ingress_rule=allow_all_ingress", "can_call={\"store\": \"*\"}",
                ],
            ] {
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

        /// Every row of the ONE TAPE. `ledger.json` must not exist — an old one
        /// is ignored and nothing ever writes it again.
        private func tapeRows(_ dir: URL) throws -> [Row] {
            #expect(
                !FileManager.default.fileExists(
                    atPath: dir.appendingPathComponent(".fantastic/ledger.json").path),
                "ledger.json is deleted — nothing may write it")
            let f = dir.appendingPathComponent(".fantastic/messages.xml")
            #expect(FileManager.default.fileExists(atPath: f.path), "no messages.xml at \(f.path)")
            return try MessagesXML.parseMessages(try String(contentsOf: f, encoding: .utf8))
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

            // SIGTERM — the graceful path python takes.
            kill(proc.processIdentifier, SIGTERM)
            await wait(60, "the daemon to exit", tail.text) { !proc.isRunning }
            proc.waitUntilExit()
            #expect(proc.terminationStatus == 0, "exit \(proc.terminationStatus): \(tail.text)")

            // THE LAST EXECUTION IS ON THE TAPE — call AND settle, both whole.
            let rows = try tapeRows(dir)
            let landed = rows.compactMap { $0["name"]?.asString }
            for name in names {
                #expect(landed.contains(name), "\(name) never reached messages.xml: \(landed)")
            }
            let last = try #require(rows.last { $0["name"]?.asString == "daemon-call-3" })
            #expect(last["agent"]?.asString == "store", "\(last)")
            #expect(last["named"] == .bool(true), "\(last)")
            #expect(last["payload"]?["type"] == .string("list"), "the payload verbatim: \(last)")
            #expect(last["attempts"] == .integer(1) && last["retries"] == .integer(0))
            let settle = try #require(
                rows.last { $0["kind"]?.asString == "settle" && $0["parent"] == last["id"] })
            #expect(settle["outcome"]?["status"] == .string("completed"), "\(settle)")
            #expect(settle["outcome"]?["result"] != nil, "the whole outcome body: \(settle)")
            #expect(settle["attempts"] == .integer(1))
        }

        // ── the one-shot path ──────────────────────────────────────

        @Test func oneShotFlushesItsExecutionBeforeExit() async throws {
            let dir = tmpDir()
            defer { try? FileManager.default.removeItem(at: dir) }
            let (mk, mkCode) = try oneShot(
                dir,
                [
                    "kernel", "create_agent", "parent_id=kernel_state",
                    "handler_module=file_bridge.tools", "id=store", "root=.fantastic",
                    "ingress_rule=allow_all_ingress",
                ])
            #expect(mkCode == 0, "\(mk)")

            let (out, code) = try oneShot(dir, ["store", "list", "path="])
            #expect(code == 0, "\(out)")
            #expect(!out.contains("\"error\""), "\(out)")

            // The process is GONE — everything it executed is on the tape.
            //
            // NOT the `store list` itself: an OPERATOR call on the discovered
            // store is the loader's own persistence traffic, which never enters
            // the log (`MessagePump.skips`, python's `_is_own_persistence`) —
            // ledger.json used to keep it, the tape deliberately does not. What
            // is pinned is the execution the one-shot performed BEFORE the store
            // was the store: the `create_agent`, with its payload and its
            // settle, landed by the exit path.
            let rows = try tapeRows(dir)
            let call = try #require(
                rows.last {
                    $0["kind"]?.asString == "call"
                        && $0["payload"]?["type"] == .string("create_agent")
                },
                "the one-shot's own execution is missing: \(rows.count) rows")
            #expect(call["payload"]?["id"] == .string("store"), "the payload verbatim: \(call)")
            #expect(call["attempts"] == .integer(1) && call["named"] == .bool(false))
            let settle = try #require(
                rows.last { $0["kind"]?.asString == "settle" && $0["parent"] == call["id"] },
                "no settle row for the one-shot's call")
            #expect(settle["outcome"]?["status"] == .string("completed"), "\(settle)")
            #expect(settle["outcome"]?["result"]["id"] == .string("store"), "\(settle)")
        }
    }

#endif
