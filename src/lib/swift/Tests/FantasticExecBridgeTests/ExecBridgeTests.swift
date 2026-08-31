// FantasticExecBridge tests — the general-purpose COMMAND runner, and THE ORPHAN
// GATE.
//
// `start {command, args}` spawns a BACKGROUND child that outlives the dispatch.
// So the job gets its OWN row in the task table — `ps` shows `{agent:<py>,
// verb:"start"}` while it runs — and `cancel` on that row must reach the real OS
// process (the orphan gate). Tests that must SPAWN find a real python and use it
// as the `command` with a permissive `allowed=[".*"]` standing in for the
// operator; they return early (skip) on a host with no python. The gate tests
// need no interpreter at all. Mirrors python's `test_exec_bridge_jobs.py` + rust's
// `tests.rs`.

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

    /// Create an exec_bridge agent with the leg OPEN (`ingress_rule=allow_all_ingress`)
    /// and a permissive `allowed=[".*"]` — the operator opening the leg to any command.
    /// The seal + the allowlist refusal are covered by dedicated tests.
    private func createPy(
        _ kernel: Kernel, id: String, meta: OrderedDictionary<String, JSON> = [:]
    ) async -> AgentId {
        var payload: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"),
            "parent_id": .string("core"),
            "handler_module": .string("exec_bridge.tools"),
            "id": .string(id),
            "ingress_rule": .string("allow_all_ingress"),
            "allowed": .array([.string(".*")]),
        ]
        for (k, v) in meta { payload[k] = v }
        let r = await kernel.send([Item(AgentId("core"), .object(payload))])[0].result
        return AgentId(r["id"].asString ?? id)
    }

    /// A real python interpreter on this machine, used as the `command`, or nil.
    private func py3() -> String? {
        for p in ["/usr/bin/python3", "/opt/homebrew/bin/python3", "/usr/local/bin/python3"]
        where FileManager.default.isExecutableFile(atPath: p) { return p }
        return nil
    }

    /// A `start` payload that runs `code` under `py`. `connector` prepends the vendored
    /// wire client, exactly as a program wanting a `kernel` global would.
    private func pyStart(_ py: String, _ code: String, connector: Bool = false) -> JSON {
        let program = connector ? CONNECTOR_SRC + "\n" + code : code
        return .object([
            "type": .string("start"), "command": .string(py),
            "args": .array([.string("-u"), .string("-c"), .string(program)]),
        ])
    }

    private func psRows(_ kernel: Kernel) async -> [JSON] {
        let r = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("ps")]))])[0].result
        return r["tasks"].asArray ?? []
    }

    private func cancelTask(_ kernel: Kernel, _ taskId: String) async -> JSON {
        await kernel.send([Item(AgentId("kernel"), .object(["type": .string("cancel"), "task_id": .string(taskId)]))])[0].result
    }

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

    private func processGone(_ pid: Int32) -> Bool {
        kill(pid, 0) == -1 && errno == ESRCH
    }

    private func waitProcessGone(_ pid: Int32, timeout: Double = 5) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if processGone(pid) { return true }
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        return processGone(pid)
    }

    private func pidFile() -> URL {
        FileManager.default.temporaryDirectory
            .appendingPathComponent("fantastic_pyrt_\(UUID().uuidString).pid")
    }

    private func sleepForeverCode(_ file: URL) -> String {
        """
        import os, time, pathlib
        pathlib.Path(\(file.path.debugDescription)).write_text(str(os.getpid()))
        while True:
            time.sleep(0.5)
        """
    }

    private func waitForPid(_ file: URL, timeout: Double = 10) async throws -> Int32 {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if let s = try? String(contentsOf: file, encoding: .utf8),
                let pid = Int32(s.trimmingCharacters(in: .whitespacesAndNewlines))
            {
                return pid
            }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        Issue.record("the child never published its pid to \(file.path)")
        throw CancellationError()
    }

    // MARK: - the bundle surface

    @Suite("FantasticExecBridge")
    struct ExecBridgeBundleTests {

        @Test func readmePresentAndTitled() {
            let bundle = ExecBridgeBundle()
            #expect(bundle.readme?.contains("exec_bridge") == true)
            #expect(bundle.name == "exec_bridge")
        }

        /// The readme teaches the general-purpose COMMAND model, gated by the
        /// allowlist, and is honest that it is not a sandbox.
        @Test func readmeStatesTheCommandModel() throws {
            let readme = try #require(ExecBridgeBundle().readme)
            #expect(readme.contains("COMMAND"), "\(readme)")
            #expect(readme.contains("ALLOWLIST") || readme.contains("allowlist"), "\(readme)")
            #expect(readme.lowercased().contains("not a sandbox"), "\(readme)")
        }

        // MARK: - the command gate: a REGEX allowlist

        @Test func shlexJoinQuotesLikePython() {
            #expect(shlexJoin(["ls", "-la"]) == "ls -la")
            #expect(shlexJoin(["echo", "a b"]) == "echo 'a b'")
            #expect(shlexJoin(["x", ""]) == "x ''")
            #expect(shlexJoin(["echo", "it's"]) == "echo 'it'\"'\"'s'")
        }

        @Test func commandPermittedFullmatchesAndRefuses() async {
            let kernel = freshKernel()
            // Empty allowlist ⇒ a loud refusal telling the operator what to set.
            let sealed = kernel.agent(await create(kernel, id: "cp_empty", allowed: nil))
            let e = commandPermitted(sealed, "ls")
            #expect(e?.contains("allowed=[") == true, "must teach the fix: \(e ?? "nil")")

            // A matching line is permitted; the match is ANCHORED (fullmatch).
            let rec = kernel.agent(await create(kernel, id: "cp_ls", allowed: ["ls( .*)?"]))
            #expect(commandPermitted(rec, "ls") == nil)
            #expect(commandPermitted(rec, "ls -la") == nil)
            #expect(commandPermitted(rec, "ls; rm -rf /")?.contains("not allowed") == true)
            #expect(commandPermitted(rec, "rm -rf /")?.contains("ls( .*)?") == true)

            // DOTALL: a multi-line arg (a `-c` program) is spanned by `.*`.
            let any = kernel.agent(await create(kernel, id: "cp_any", allowed: [".*"]))
            #expect(commandPermitted(any, "python -c 'a\nb\nc'") == nil)
        }

        private func create(
            _ kernel: Kernel, id: String, allowed: [String]?
        ) async -> AgentId {
            var payload: OrderedDictionary<String, JSON> = [
                "type": .string("create_agent"), "parent_id": .string("core"),
                "handler_module": .string("exec_bridge.tools"), "id": .string(id),
                "ingress_rule": .string("allow_all_ingress"),
            ]
            if let allowed { payload["allowed"] = .array(allowed.map(JSON.string)) }
            let r = await kernel.send([Item(AgentId("core"), .object(payload))])[0].result
            return AgentId(r["id"].asString ?? id)
        }

        @Test func validateMetaRefusesWithdrawnFieldsAndBadAllowed() {
            let b = ExecBridgeBundle()
            for gone in ["code", "shim", "interpreter", "interpreters", "argv_template"] {
                let e = b.validateMeta([gone: .string("whatever")], record: [:])
                #expect(e?.contains(gone) == true, "\(gone) must be refused: \(e ?? "nil")")
                #expect(e?.contains("allowed") == true, "refusal must point to `allowed`")
            }
            #expect(b.validateMeta(["allowed": .array([])], record: [:]) != nil)
            #expect(
                b.validateMeta(["allowed": .array([.string("(")])], record: [:])?
                    .contains("broken regex") == true)
            #expect(
                b.validateMeta(["allowed": .array([.string("ls( .*)?"), .string(".*")])], record: [:])
                    == nil)
        }

        @Test func reflectAdvertisesTheAllowlistNotInterpreters() async throws {
            let kernel = freshKernel()
            let a = await create(kernel, id: "refl", allowed: ["ls( .*)?"])
            let r = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
            #expect(r["id"].asString == a.value)
            #expect(r["running"].asInt == 0)
            #expect(r["allowed"].asArray?.compactMap { $0.asString } == ["ls( .*)?"])
            // The retired interpreter/shim keys are gone.
            #expect(r["interpreters"].isNull, "\(r.serialize())")
            #expect(r["shims"].isNull, "\(r.serialize())")
            #expect(r["wire"].asString?.contains("FANTASTIC_CTRL_FD") == true)
            let verbs = try #require(r["verbs"].asObject)
            for v in ["reflect", "start", "status", "clear", "interrupt", "stop", "boot"] {
                #expect(verbs[v] != nil, "verb \(v) missing from reflect")
            }
        }

        @Test func startRefusesACodeField() async {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "code_field")
            let r = await kernel.send([Item(a, .object(["type": .string("start"), "code": .string("x = 1")]))])[0].result
            #expect(
                r["error"].asString == "exec_bridge: `code` is not a field — run a command instead",
                "\(r.serialize())")
        }

        /// A command matching no `allowed` pattern is refused, naming the patterns, and
        /// nothing is spawned. And a SEALED leg denies `start` before the allowlist is
        /// even consulted, while `reflect` still answers.
        @Test func startRefusesUnlistedCommandAndSealedLegDenies() async {
            let kernel = freshKernel()
            let a = await create(kernel, id: "only_ls", allowed: ["ls( .*)?"])
            let r = await kernel.send([Item(a, .object([
                    "type": .string("start"), "command": .string("/bin/rm"),
                    "args": .array([.string("-rf"), .string("/")]),
                ]))])[0].result
            #expect(r["error"].asString?.contains("not allowed") == true, "\(r.serialize())")
            #expect(r["job_id"].isNull, "nothing may be spawned: \(r.serialize())")
            let st = await kernel.send([Item(a, .object(["type": .string("status")]))])[0].result
            #expect(st["jobs"].asArray?.count == 0, "a refused start left a job row: \(st.serialize())")

            // SEALED BY DEFAULT — a fresh leg (no ingress_rule) denies every owned verb
            // except reflect, and the denial teaches the fix.
            let sealedR = await kernel.send([Item(AgentId("core"), .object([
                    "type": .string("create_agent"), "parent_id": .string("core"),
                    "handler_module": .string("exec_bridge.tools"), "id": .string("sealed"),
                ]))])[0].result
            let sealed = AgentId(sealedR["id"].asString ?? "sealed")
            let d = await kernel.send([Item(sealed, .object(["type": .string("start"), "command": .string("ls")]))])[0].result
            #expect(d["reason"].asString == "unauthorized", "\(d.serialize())")
            #expect(
                d["hint"].asString?.contains("ingress_rule=allow_all_ingress") == true,
                "the denial must teach how to open: \(d.serialize())")
            let refl = await kernel.send([Item(sealed, .object(["type": .string("reflect")]))])[0].result
            #expect(refl["sealed"].asBool == true)
            #expect(refl["allowed"].asArray != nil, "discovery must work sealed")
        }

        // MARK: - the task table + the orphan gate

        /// A running job IS a process, and the kernel sees it: the job's row carries
        /// `verb:"start"`, and reflect counts the child.
        @Test func aRunningJobIsARowInPsAndIsCountedRunning() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "py_ps")
            let file = pidFile()
            defer { try? FileManager.default.removeItem(at: file) }

            _ = await kernel.send([Item(a, pyStart(py, sleepForeverCode(file)))])[0].result
            let pid = try await waitForPid(file)
            let row = try await waitForRow(kernel, agent: a.value, verb: "start")
            #expect(row["id"].asString?.hasPrefix(a.value + ":") == true, "\(row.serialize())")

            let reflect = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
            #expect(reflect["running"].asInt == 1, "\(reflect.serialize())")

            let stopped = await kernel.send([Item(a, .object(["type": .string("stop")]))])[0].result
            #expect(stopped["killed"].asInt == 1, "\(stopped.serialize())")
            #expect(await waitProcessGone(pid), "pid \(pid) survived `stop`")
        }

        /// THE REGRESSION GATE — the orphan bug. Cancelling a Swift `Task` kills NO OS
        /// process; the bundle's `onCancel` hook does, awaited BEFORE the abort. Assert
        /// the PROCESS is dead (kill(pid,0) → ESRCH), not merely that the call returned.
        @Test func cancelReapsTheRealOsSubprocess() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "py_orphan")
            let file = pidFile()
            defer { try? FileManager.default.removeItem(at: file) }

            _ = await kernel.send([Item(a, pyStart(py, sleepForeverCode(file)))])[0].result
            let pid = try await waitForPid(file)
            #expect(!processGone(pid), "the child must really be running before we cancel")

            let row = try await waitForRow(kernel, agent: a.value, verb: "start")
            let taskId = try #require(row["id"].asString)
            let cancelled = await cancelTask(kernel, taskId)
            #expect(cancelled["cancelled"].asBool == true, "\(cancelled.serialize())")
            #expect(await waitProcessGone(pid), "pid \(pid) survived cancel — ORPHANED")

            let rows = await psRows(kernel)
            #expect(!rows.contains { $0["id"].asString == taskId }, "the cancelled row is gone")
            let reflect = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
            #expect(reflect["running"].asInt == 0, "\(reflect.serialize())")
        }

        /// Cancel is TASK-scoped: two `start`s on one agent are two tasks, and killing
        /// one leaves the other's child running.
        @Test func cancelKillsOnlyItsOwnTasksChild() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "py_two")
            let fileA = pidFile(), fileB = pidFile()
            defer {
                try? FileManager.default.removeItem(at: fileA)
                try? FileManager.default.removeItem(at: fileB)
            }

            _ = await kernel.send([Item(a, pyStart(py, sleepForeverCode(fileA)))])[0].result
            let pidA = try await waitForPid(fileA)
            _ = await kernel.send([Item(a, pyStart(py, sleepForeverCode(fileB)))])[0].result
            let pidB = try await waitForPid(fileB)
            #expect(pidA != pidB)

            var rows: [JSON] = []
            for _ in 0..<300 {
                rows = await psRows(kernel).filter {
                    $0["agent"].asString == a.value && $0["verb"].asString == "start"
                }
                if rows.count == 2 { break }
                try await Task.sleep(nanoseconds: 10_000_000)
            }
            #expect(rows.count == 2, "both jobs must hold a row")
            let taskA = try #require(rows.first?["id"].asString)

            let cancelled = await cancelTask(kernel, taskA)
            #expect(cancelled["cancelled"].asBool == true)
            #expect(await waitProcessGone(pidA), "pid \(pidA) survived cancel — ORPHANED")
            #expect(!processGone(pidB), "cancelling A's task must not kill B's child")

            _ = await kernel.send([Item(a, .object(["type": .string("stop")]))])[0].result
            #expect(await waitProcessGone(pidB))
        }

        /// The agent's death is a cascade: `delete_agent` SIGKILLs the children it owns.
        @Test func deletingTheAgentReapsItsChildren() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "py_delete")
            let file = pidFile()
            defer { try? FileManager.default.removeItem(at: file) }

            _ = await kernel.send([Item(a, pyStart(py, sleepForeverCode(file)))])[0].result
            let pid = try await waitForPid(file)
            _ = try await waitForRow(kernel, agent: a.value, verb: "start")

            _ = await kernel.send([Item(AgentId("core"), .object(["type": .string("delete_agent"), "id": .string(a.value)]))])[0].result
            #expect(await waitProcessGone(pid), "pid \(pid) survived delete — ORPHANED")
        }
    }

    // ─── durable jobs, end to end ───────────────────────────────────

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

    @Suite("exec_bridge durable jobs")
    struct ExecBridgeJobsTests {

        /// `start` returns AT ONCE, and RECORDS what ran: `command`/`args` surface in
        /// `status` so a caller can re-issue verbatim.
        @Test func startReturnsAJobIdImmediatelyAndRecordsTheInvocation() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "dur_start")
            let t0 = Date()
            let r = await kernel.send([Item(a, pyStart(py, "import time; time.sleep(30)"))])[0].result
            #expect(r["job_id"].asString != nil, "no job_id: \(r.serialize())")
            #expect(r["status"].asString == "running")
            #expect(Date().timeIntervalSince(t0) < 5, "start blocked")
            let st = await kernel.send([Item(a, .object(["type": .string("status")]))])[0].result
            #expect(st["jobs"].asArray?.first?["command"].asString == py, "\(st.serialize())")
            #expect(st["jobs"].asArray?.first?["args"].asArray?.first?.asString == "-u")
            _ = await kernel.send([Item(a, .object(["type": .string("stop")]))])[0].result
        }

        /// The connector round-trip: a program that includes the wire client reaches
        /// ANOTHER agent by id and reads the reply.
        @Test func theProgramReachesAnotherAgentThroughTheConnector() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "dur_send")
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([Item(a, pyStart(
                py, "r = kernel.send('core', {'type': 'reflect'})\nprint('GOT', r.get('id'))",
                connector: true))])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let done = events.first { $0["type"].asString == "job_done" }
            #expect(
                done?["stdout"].asString?.contains("GOT core") == true,
                "connector send did not round-trip: \(done?.serialize() ?? "none")")
        }

        /// No running job ⇒ a loud error, never a silent drop.
        @Test func anUnknownVerbWithoutAJobErrorsLoudly() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "dur_nojob")
            let r = await kernel.send([Item(a, .object(["type": .string("ping")]))])[0].result
            #expect(r["error"].asString?.contains("no running job") == true)
        }
    }

    // ─── the control-socket handoff ─────────────────────────────────

    @Suite("exec_bridge spawn")
    struct ExecBridgeSpawnTests {

        /// THE decisive test for the durable design: does the control socket reach the
        /// child at the advertised number, and does the per-call env arrive?
        @Test func theChildReceivesTheControlSocketOnTheAdvertisedFd() async throws {
            guard let py = py3() else { return }
            let code = """
                import os, stat
                fd = int(os.environ["FANTASTIC_CTRL_FD"])
                print("fd", fd, "sock", stat.S_ISSOCK(os.fstat(fd).st_mode), "self", os.environ["FANTASTIC_SELF_ID"], "env", os.environ.get("MYVAR"))
                """
            let sp = try Spawn.job(
                argv: [py, "-u", "-c", code], selfId: "owner_agent",
                envExtra: ["MYVAR": "hello"], cwd: nil)
            let outData = sp.stdout.fileHandleForReading.readDataToEndOfFile()
            sp.process.waitUntilExit()
            close(sp.ctrlFd)
            let text = String(data: outData, encoding: .utf8) ?? ""
            #expect(text.contains("sock True"), "control fd is not a socket: \(text)")
            #expect(text.contains("self owner_agent"), "self_id not delivered: \(text)")
            #expect(text.contains("env hello"), "per-call env not delivered: \(text)")
        }
    }

#endif  // os(macOS)
