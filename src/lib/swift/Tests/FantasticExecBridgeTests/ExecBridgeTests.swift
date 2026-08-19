// FantasticExecBridge tests — the subprocess bundle, and THE ORPHAN GATE.
//
// The bundle's `start` spawns a BACKGROUND `python3` child that outlives the
// dispatch (there is no `exec`). So the job gets its OWN row in the task table
// — `ps` shows `{agent:<py>, verb:"start"}` while it runs — and `cancel` on
// that row must reach the real OS process.
//
// The test that matters (`cancelReapsTheRealOsSubprocess`) asserts on the PID:
// the child writes its `os.getpid()` to a temp file and then sleeps forever;
// after `cancel`, `kill(pid, 0)` must fail with ESRCH. Asserting "the Swift
// Task was cancelled" would pass with the hook removed — and leave a live
// `python3` behind, which is the whole bug (this repo once leaked 33 such
// orphans). Mirrors python's
// `test_exec_bridge_jobs.py` (cancel reaps the OS subprocess).

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation
    import OrderedCollections
    import Testing

    @testable import FantasticExecBridge

    // MARK: - harness

    private func freshKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("exec_bridge.tools", ExecBridgeBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    /// `meta` rides `create_agent`'s `**meta` — the way an operator sets `venv` /
    /// `python` on the record.
    private func createPy(
        _ kernel: Kernel, id: String, meta: OrderedDictionary<String, JSON> = [:]
    ) async -> AgentId {
        var payload: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"),
            "handler_module": .string("exec_bridge.tools"),
            "id": .string(id),
            "ingress_rule": .string("allow_all"),
            "shim": .string("python"),
        ]
        if let interp = py3() {
            payload["interpreters"] = .array([.string(interp)])
        }
        for (k, v) in meta { payload[k] = v }
        let r = await kernel.send(AgentId("core"), .object(payload))
        return AgentId(r["id"].asString ?? id)
    }

    /// A REAL interpreter on this machine, or nil. There is NO default interpreter in
    /// this runtime (it is not a python process, so it has no `sys.executable` to
    /// inherit and refuses to guess a path), so every test that needs a live child
    /// must name one — and skip when the machine has none.
    private func py3() -> String? {
        for p in ["/usr/bin/python3", "/opt/homebrew/bin/python3", "/usr/local/bin/python3"]
        where FileManager.default.isExecutableFile(atPath: p) { return p }
        return nil
    }

    /// The `ps` reply's rows — through the VERB, like any caller.
    private func psRows(_ kernel: Kernel) async -> [JSON] {
        let r = await kernel.send(AgentId("kernel"), .object(["type": .string("ps")]))
        return r["tasks"].asArray ?? []
    }

    private func cancelTask(_ kernel: Kernel, _ taskId: String) async -> JSON {
        await kernel.send(
            AgentId("kernel"),
            .object(["type": .string("cancel"), "task_id": .string(taskId)]))
    }

    /// Poll `ps` until the row shows up (the job's row is inserted concurrently).
    private func waitForRow(_ kernel: Kernel, agent: String, verb: String) async throws -> JSON {
        for _ in 0..<300 {
            if let row = await psRows(kernel).first(where: {
                $0["agent"].asString == agent && $0["verb"].asString == verb
            }) {
                return row
            }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        Issue.record("no `\(verb)` row for \(agent) in ps")
        throw CancellationError()
    }

    /// Is the OS process `pid` gone? `kill(pid, 0)` probes without signalling:
    /// ESRCH = no such process. A ZOMBIE still answers 0 here, so this is also
    /// the check that the child was REAPED, not merely killed.
    private func processGone(_ pid: Int32) -> Bool {
        kill(pid, 0) == -1 && errno == ESRCH
    }

    /// Poll until the pid is really gone (kill → exit → reap is asynchronous).
    private func waitProcessGone(_ pid: Int32, timeout: Double = 5) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if processGone(pid) { return true }
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        return processGone(pid)
    }

    /// A scratch file the child writes its pid into — the test's only window onto
    /// the REAL process (no bundle internals peeked at).
    private func pidFile() -> URL {
        FileManager.default.temporaryDirectory
            .appendingPathComponent("fantastic_pyrt_\(UUID().uuidString).pid")
    }

    /// Python that publishes its pid, then sleeps forever. Killing the call must
    /// kill THIS.
    private func sleepForeverCode(_ file: URL) -> String {
        """
        import os, time, pathlib
        pathlib.Path(\(file.path.debugDescription)).write_text(str(os.getpid()))
        while True:
            time.sleep(0.5)
        """
    }

    /// Wait for the child to publish its pid.
    private func waitForPid(_ file: URL, timeout: Double = 10) async throws -> Int32 {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if let s = try? String(contentsOf: file, encoding: .utf8),
                let pid = Int32(
                    s.trimmingCharacters(in: .whitespacesAndNewlines))
            {
                return pid
            }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        Issue.record("the child never published its pid to \(file.path)")
        throw CancellationError()
    }

    // MARK: - tests

    @Suite("FantasticPythonRuntime")
    struct PythonRuntimeTests {

        @Test func readmePresentAndTitled() {
            let bundle = ExecBridgeBundle()
            #expect(bundle.readme?.contains("exec_bridge") == true)
            #expect(bundle.name == "exec_bridge")
        }

        /// The readme is what a model reads before authoring `code`, so it must state
        /// the execution model: a SCRIPT run top to bottom, a bare `def` inert.
        @Test func readmeStatesTheScriptExecutionModel() throws {
            let readme = try #require(ExecBridgeBundle().readme)
            #expect(readme.contains("SCRIPT"), "\(readme)")
            #expect(readme.contains("top to bottom"), "\(readme)")
            #expect(readme.contains("def run(payload)"), "\(readme)")
            #expect(readme.contains("kernel.on_message(callback)"), "\(readme)")
            #expect(!readme.contains("IS the tool"), "\(readme)")
        }

        /// EVERY key of `reflect.verbs` must be a verb `handle` really dispatches —
        /// otherwise reflect is a lie a caller cannot detect. The map advertised
        /// `exec` (a verb that has not existed since jobs became background +
        /// addressable) and OMITTED `start`, the one that does the work. An
        /// unadvertised verb falls through to the inbox and, with no live job, comes
        /// back as `unknown type`; that is the probe used here.
        @Test func reflectAdvertisesOnlyDispatchableVerbs() async throws {
            let kernel = freshKernel()
            let py = await createPy(kernel, id: "py_verbs")
            let reflect = await kernel.send(py, .object(["type": .string("reflect")]))
            let verbs = try #require(reflect["verbs"].asObject)

            // The stale verb must be GONE from the advertisement...
            #expect(verbs["exec"] == nil, "`exec` is still advertised: \(reflect.serialize())")
            // ...and undispatchable, which is why advertising it was a lie.
            let exec = await kernel.send(py, .object(["type": .string("exec")]))
            #expect(
                exec["error"].asString?.contains("unknown type") == true, "\(exec.serialize())")
            #expect(
                reflect["sentence"].asString?.contains("exec") != true,
                "\(reflect.serialize())")

            // Both verbs that carry the work must be advertised.
            for verb in ["start", "boot", "reflect", "interrupt", "stop", "shutdown"] {
                #expect(verbs[verb] != nil, "\(verb) is dispatched but not advertised")
            }

            // Every advertised verb reaches a real handler. `start` with no `code`
            // answers with its OWN argument error — a dispatch, not a fall-through.
            for verb in verbs.keys {
                let r = await kernel.send(py, .object(["type": .string(verb)]))
                #expect(
                    r["error"].asString?.contains("unknown type") != true,
                    "advertised verb `\(verb)` is not dispatched: \(r.serialize())")
            }
        }

        /// THE COMMENTS ARE A SURFACE TOO — and nothing guarded them, which is why the
        /// file header went on describing `exec` (a RUN-AND-WAIT verb, `python3 -c` per
        /// call, "stateless across calls", a `ps` row reading `verb:"exec"`, a function
        /// `execPython`) long after the verb left the dispatch switch. The reflect
        /// surface in the SAME FILE already said `verb:"start"`: two copies of one fact,
        /// one of them wrong — the defect class this reads the source to catch. (Rust's
        /// selftest states an `exec` key "is a regression"; swift had no equivalent.)
        ///
        /// Cheap by design: exact stale claims, each one a defect that shipped. It
        /// cannot prove a comment TRUE, only stop these from coming back.
        @Test func theSourceDocsDoNotDescribeAVerbThatIsGone() throws {
            let source = URL(fileURLWithPath: #filePath)
                .deletingLastPathComponent()  // FantasticPythonRuntimeTests/
                .deletingLastPathComponent()  // Tests/
                .deletingLastPathComponent()  // swift/
                .appendingPathComponent("Sources/FantasticExecBridge/ExecBridge.swift")
            let text = try #require(
                try? String(contentsOf: source, encoding: .utf8),
                "cannot read \(source.path)")
            for stale in [
                "execPython",  // a function that no longer exists
                #"verb:"exec""#,  // the ps claim — the same file's reflect says "start"
                "MARK: - exec",  // a section marker for a removed verb
                "Stateless across calls",  // it is durable: the `code` lives on the record
                "RUN-AND-WAIT",  // `start` returns a job_id; the output is not the reply
            ] {
                #expect(!text.contains(stale), "the docs still claim \"\(stale)\"")
            }
            // And the live shape IS stated: the row a job really holds.
            #expect(text.contains(#"verb:"start""#), "the header must name the real ps row")
        }

        // MARK: - the task table

        /// A running job IS a process, and the kernel sees it: the job's row
        /// carries `verb:"start"`, and the bundle's job table counts the child.
        @Test func aRunningJobIsARowInPsAndIsCountedInFlight() async throws {
            guard let interp = py3() else { return }
            let kernel = freshKernel()
            let py = await createPy(kernel, id: "py_ps")
            let file = pidFile()
            defer { try? FileManager.default.removeItem(at: file) }

            let call = Task {
                await kernel.send(
                    py,
                    .object([
                        "type": .string("start"), "interpreter": .string(interp),
                        "code": .string(sleepForeverCode(file)),
                    ]))
            }
            let pid = try await waitForPid(file)
            let row = try await waitForRow(kernel, agent: py.value, verb: "start")
            #expect(row["id"].asString?.hasPrefix(py.value + ":") == true, "\(row.serialize())")
            #expect(row["parent"].isNull, "the start was called from outside any dispatch")

            let reflect = await kernel.send(py, .object(["type": .string("reflect")]))
            #expect(reflect["running"].asInt == 1, "\(reflect.serialize())")

            // `stop` is the signal-precise verb — it SIGKILLs the agent's children.
            let stopped = await kernel.send(py, .object(["type": .string("stop")]))
            #expect(stopped["killed"].asInt == 1, "\(stopped.serialize())")
            _ = await call.value
            #expect(await waitProcessGone(pid), "pid \(pid) survived `stop`")
        }

        /// THE REGRESSION GATE — the orphan bug.
        ///
        /// Cancelling a Swift `Task` kills NO OS process. The bundle's `onCancel`
        /// hook does, and the kernel awaits it BEFORE aborting the task (SIGTERM →
        /// SIGKILL). So: assert the PROCESS is dead — `kill(pid, 0)` → ESRCH — not
        /// merely that the call came back. Remove `onCancel` and this test fails with
        /// a surviving pid while the reply still says `{"cancelled": true}`; that is
        /// exactly the orphan.
        @Test func cancelReapsTheRealOsSubprocess() async throws {
            guard let interp = py3() else { return }
            let kernel = freshKernel()
            let py = await createPy(kernel, id: "py_orphan")
            let file = pidFile()
            defer { try? FileManager.default.removeItem(at: file) }

            // `start` returns at once; the job outlives the dispatch. That is exactly
            // the orphan class this gate exists for.
            _ = await kernel.send(
                py,
                .object([
                    "type": .string("start"), "interpreter": .string(interp),
                    "code": .string(sleepForeverCode(file)),
                ]))
            let pid = try await waitForPid(file)
            #expect(!processGone(pid), "the child must really be running before we cancel")

            let row = try await waitForRow(kernel, agent: py.value, verb: "start")
            let taskId = try #require(row["id"].asString)
            let cancelled = await cancelTask(kernel, taskId)
            #expect(cancelled["cancelled"].asBool == true, "\(cancelled.serialize())")
            #expect(
                cancelled["killed"].asArray?.compactMap { $0.asString } == [taskId],
                "\(cancelled.serialize())")

            // THE POINT: the OS process — not the coroutine — is gone, and reaped
            //    (a zombie would still answer `kill(pid, 0)`).
            #expect(await waitProcessGone(pid), "pid \(pid) survived cancel — ORPHANED")

            // The row left the table.
            let rows = await psRows(kernel)
            #expect(!rows.contains { $0["id"].asString == taskId }, "the cancelled row is gone")
            let reflect = await kernel.send(py, .object(["type": .string("reflect")]))
            #expect(reflect["running"].asInt == 0, "\(reflect.serialize())")
        }

        /// Cancel is TASK-scoped, not agent-scoped: two `start`s on one agent are two
        /// tasks, and killing one leaves the other's child running (python's
        /// precision; `stop` is the kill-them-all verb).
        @Test func cancelKillsOnlyItsOwnTasksChild() async throws {
            guard let interp = py3() else { return }
            let kernel = freshKernel()
            let py = await createPy(kernel, id: "py_two")
            let fileA = pidFile()
            let fileB = pidFile()
            defer {
                try? FileManager.default.removeItem(at: fileA)
                try? FileManager.default.removeItem(at: fileB)
            }

            Task {
                await kernel.send(
                    py,
                    .object([
                        "type": .string("start"), "interpreter": .string(interp),
                        "code": .string(sleepForeverCode(fileA)),
                    ]))
            }
            let pidA = try await waitForPid(fileA)
            // Second job, started only once A's row exists, so the rows are tellable
            // apart by their pid file.
            Task {
                await kernel.send(
                    py,
                    .object([
                        "type": .string("start"), "interpreter": .string(interp),
                        "code": .string(sleepForeverCode(fileB)),
                    ]))
            }
            let pidB = try await waitForPid(fileB)
            #expect(pidA != pidB)

            // Two live rows on the same agent; A is the older one.
            var rows: [JSON] = []
            for _ in 0..<300 {
                rows = await psRows(kernel).filter {
                    $0["agent"].asString == py.value && $0["verb"].asString == "start"
                }
                if rows.count == 2 { break }
                try await Task.sleep(nanoseconds: 10_000_000)
            }
            #expect(rows.count == 2, "both jobs must hold a row: \(rows.map { $0.serialize() })")
            let taskA = try #require(rows.first?["id"].asString)

            let cancelled = await cancelTask(kernel, taskA)
            #expect(cancelled["cancelled"].asBool == true)
            #expect(await waitProcessGone(pidA), "pid \(pidA) survived cancel — ORPHANED")

            // B is untouched — still running, still counted.
            #expect(!processGone(pidB), "cancelling A's task must not kill B's child")
            let reflect = await kernel.send(py, .object(["type": .string("reflect")]))
            #expect(reflect["running"].asInt == 1, "\(reflect.serialize())")

            _ = await kernel.send(py, .object(["type": .string("stop")]))
            #expect(await waitProcessGone(pidB))
        }

        /// The agent's death is a cascade: `delete_agent` SIGKILLs the children it
        /// still owns (the same teardown `onCancel` uses), so a deleted agent cannot
        /// leave a `python3` behind either.
        @Test func deletingTheAgentReapsItsChildren() async throws {
            guard let interp = py3() else { return }
            let kernel = freshKernel()
            let py = await createPy(kernel, id: "py_delete")
            let file = pidFile()
            defer { try? FileManager.default.removeItem(at: file) }

            let call = Task {
                await kernel.send(
                    py,
                    .object([
                        "type": .string("start"), "interpreter": .string(interp),
                        "code": .string(sleepForeverCode(file)),
                    ]))
            }
            let pid = try await waitForPid(file)
            _ = try await waitForRow(kernel, agent: py.value, verb: "start")

            _ = await kernel.send(
                AgentId("core"),
                .object(["type": .string("delete_agent"), "id": .string(py.value)]))

            #expect(await waitProcessGone(pid), "pid \(pid) survived delete — ORPHANED")
            _ = await call.value
        }
    }

    // ─── the ALLOWLIST: refusals name the legal set (py canonical) ──

    @Suite("interpreter allowlist")
    struct AllowlistTests {
        @Test func emptyAllowlistRefusesTellingTheOperatorWhatToSet() {
            #expect(throws: Interpreter.Err.self) {
                try Interpreter.resolve(payload: .object([:]), record: nil)
            }
        }

        @Test func resolveAndRefusalsMirrorPy() async throws {
            let registry = BundleRegistry()
            registry.register("exec_bridge.tools", ExecBridgeBundle())
            let kernel = Kernel(storage: .inMemory, bundles: registry)
            let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
            kernel.register(root)
            kernel.setRoot(root)
            _ = await kernel.send(
                AgentId("core"),
                .object([
                    "type": .string("create_agent"),
                    "handler_module": .string("exec_bridge.tools"),
                    "id": .string("leg"),
                    "interpreters": .array([.string("/a/bin/python"), .string("/b/bin/python")]),
                ]))
            let agent = kernel.agent(AgentId("leg"))
            do {
                _ = try Interpreter.resolve(payload: .object([:]), record: agent)
                Issue.record("ambiguity must refuse")
            } catch let e as Interpreter.Err {
                #expect(e.message.contains("/a/bin/python"), "\(e)")
            }
            do {
                _ = try Interpreter.resolve(
                    payload: .object(["interpreter": .string("/evil/python")]), record: agent)
                Issue.record("unlisted must refuse")
            } catch let e as Interpreter.Err {
                #expect(e.message.contains("not allowlisted"), "\(e)")
                #expect(e.message.contains("/b/bin/python"), "\(e)")
            }
            let got = try Interpreter.resolve(
                payload: .object(["interpreter": .string("/b/bin/python")]), record: agent)
            #expect(got == "/b/bin/python")
        }

        @Test func sealedByDefaultDeniesStartAndReflectStillAnswers() async throws {
            let registry = BundleRegistry()
            registry.register("exec_bridge.tools", ExecBridgeBundle())
            let kernel = Kernel(storage: .inMemory, bundles: registry)
            let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
            kernel.register(root)
            kernel.setRoot(root)
            _ = await kernel.send(
                AgentId("core"),
                .object([
                    "type": .string("create_agent"),
                    "handler_module": .string("exec_bridge.tools"),
                    "id": .string("sealed"),
                ]))
            let d = await kernel.send(
                AgentId("sealed"), .object(["type": .string("start"), "code": .string("x")]))
            #expect(d["reason"].asString == "unauthorized", "\(d.serialize())")
            #expect(
                d["hint"].asString?.contains("ingress_rule=allow_all") == true,
                "the denial must teach how to open: \(d.serialize())")
            let r = await kernel.send(AgentId("sealed"), .object(["type": .string("reflect")]))
            #expect(r["sealed"].asBool == true)
            #expect(r["interpreters"].asArray != nil, "discovery must work sealed")
        }
    }

#endif  // os(macOS)

// ─── durable jobs, end to end ───────────────────────────────────

/// Drain an agent's inbox until `pred` matches or the timeout passes.
private func drainUntil(
    _ stream: AsyncStream<JSON>, timeout: Double = 25,
    where pred: @escaping @Sendable (JSON) -> Bool
) async -> [JSON] {
    final class Sink: @unchecked Sendable {
        let lock = NSLock()
        var events: [JSON] = []
        func add(_ e: JSON) { lock.withLock { events.append(e) } }
        func hit(_ p: (JSON) -> Bool) -> Bool { lock.withLock { events.contains(where: p) } }
        func all() -> [JSON] { lock.withLock { events } }
    }
    let sink = Sink()
    let consumer = Task { for await ev in stream { sink.add(ev) } }
    let deadline = Date().addingTimeInterval(timeout)
    while Date() < deadline {
        if sink.hit(pred) { break }
        try? await Task.sleep(nanoseconds: 20_000_000)
    }
    consumer.cancel()
    return sink.all()
}

@Suite("python durable jobs")
struct PythonDurableTests {

    /// `start` returns AT ONCE — that is the contract. A blocking start is `exec`.
    @Test func startReturnsAJobIdImmediately() async throws {
        guard let py = py3() else { return }
        let kernel = freshKernel()
        let a = await createPy(kernel, id: "dur_start")
        let t0 = Date()
        let r = await kernel.send(
            a,
            .object([
                "type": .string("start"), "python": .string(py),
                "code": .string("import time; time.sleep(30)"),
            ]))
        #expect(r["job_id"].asString != nil, "no job_id: \(r.serialize())")
        #expect(Date().timeIntervalSince(t0) < 5, "start blocked")
        _ = await kernel.send(a, .object(["type": .string("stop")]))
    }

    /// The connector round-trip: the program reaches ANOTHER agent by id and reads the
    /// reply. This is the whole point of the control socket.
    @Test func theProgramReachesAnotherAgentThroughTheConnector() async throws {
        guard let py = py3() else { return }
        let kernel = freshKernel()
        let a = await createPy(kernel, id: "dur_send")
        let stream = kernel.ensureInbox(a)
        _ = await kernel.send(
            a,
            .object([
                "type": .string("start"), "python": .string(py),
                "code": .string(
                    "r = kernel.send('core', {'type': 'reflect'})\nprint('GOT', r.get('id'))"),
            ]))
        let events = await drainUntil(stream, timeout: 25) { $0["type"].asString == "job_done" }
        let done = events.first { $0["type"].asString == "job_done" }
        #expect(
            done?["stdout"].asString?.contains("GOT core") == true,
            "connector send did not round-trip: \(done?.serialize() ?? "none")")
    }

    /// `kernel.args` — this call's arguments, not something baked into the code.
    @Test func perCallArgsReachTheProgram() async throws {
        guard let py = py3() else { return }
        let kernel = freshKernel()
        let a = await createPy(kernel, id: "dur_args")
        let stream = kernel.ensureInbox(a)
        _ = await kernel.send(
            a,
            .object([
                "type": .string("start"), "python": .string(py),
                "code": .string("print('N', kernel.args['n'] + 1)"),
                "args": .object(["n": .integer(41)]),
            ]))
        let events = await drainUntil(stream, timeout: 25) { $0["type"].asString == "job_done" }
        let done = events.first { $0["type"].asString == "job_done" }
        #expect(done?["stdout"].asString?.contains("N 42") == true)
    }

    /// No running job ⇒ a loud error, never a silent drop.
    @Test func anUnknownVerbWithoutAJobErrorsLoudly() async throws {
        let kernel = freshKernel()
        let a = await createPy(kernel, id: "dur_nojob")
        let r = await kernel.send(a, .object(["type": .string("ping")]))
        #expect(r["error"].asString?.contains("no running job") == true)
    }
}
