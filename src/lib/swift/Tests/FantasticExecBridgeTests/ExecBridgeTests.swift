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
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
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
            "parent_id": .string("kernel_state"),
            "handler_module": .string("exec_bridge.tools"),
            "id": .string(id),
            "ingress_rule": .string("allow_all_ingress"),
            "allowed": .array([.string(".*")]),
        ]
        for (k, v) in meta { payload[k] = v }
        let r = await kernel.send([Item(AgentId("kernel_state"), .object(payload))])[0].result
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

    private func cancelTask(_ kernel: Kernel, _ taskId: Int64) async -> JSON {
        await kernel.send([Item(AgentId("kernel"), .object(["type": .string("cancel"), "task_id": .integer(taskId)]))])[0].result
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
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("exec_bridge.tools"), "id": .string(id),
                "ingress_rule": .string("allow_all_ingress"),
            ]
            if let allowed { payload["allowed"] = .array(allowed.map(JSON.string)) }
            let r = await kernel.send([Item(AgentId("kernel_state"), .object(payload))])[0].result
            return AgentId(r["id"].asString ?? id)
        }

        @Test func passwordIngressIsRefusedOnFsAndProcessLegs() {
            // The process half (the fs half lives in FantasticBundlesTests): a leg
            // with no wire envelope can never present a token, so both spellings
            // refuse at the write.
            let b = ExecBridgeBundle()
            let specs: [(String, JSON)] = [
                ("ingress_rule", "password_ingress"),
                ("ingress_rule", .object(["type": "password_ingress", "env": "X"])),
                ("auth", "password"),
            ]
            for (field, spec) in specs {
                let e = b.validateMeta([field: spec], record: [:])
                #expect(e?.contains("wire envelope") == true, "\(field) → \(String(describing: e))")
                #expect(e?.contains("allow_all_ingress") == true, "must teach the legal set")
            }
            #expect(b.validateMeta(["ingress_rule": "allow_all_ingress"], record: [:]) == nil)
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
            #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
            #expect(r["arg"].asString == "code", "\(r.serialize())")
            #expect(
                r["error"].asString == "start: code must be absent — run a command instead",
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
            let sealedR = await kernel.send([Item(AgentId("kernel_state"), .object([
                    "type": .string("create_agent"), "parent_id": .string("kernel_state"),
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

        /// A1.2 — a rule object with no `type` is a plain SEAL, python's
        /// `parse_spec` returning `(None, cfg)`: it used to throw out of `handle`
        /// (and the write was refused), so a leg configured `{env: "X"}` took the
        /// whole dispatch down instead of denying.
        @Test func aTypelessRuleObjectSealsLikePython() async {
            let kernel = freshKernel()
            let create: JSON = .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("exec_bridge.tools"), "id": .string("typeless"),
                "ingress_rule": .object(["env": .string("X")]),
            ])
            let made = await kernel.send([Item(AgentId("kernel_state"), create)])[0].result
            #expect(made["id"].asString == "typeless", "the write is accepted: \(made.serialize())")
            let a = AgentId("typeless")
            let start: JSON = .object(["type": .string("start"), "command": .string("ls")])
            let d = await kernel.send([Item(a, start)])[0].result
            #expect(d["reason"].asString == "unauthorized", "\(d.serialize())")
            #expect(d["error"].asString == "edge sealed by policy", "\(d.serialize())")
            // reflect still answers, and reports the seal + the egress default.
            let refl = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
            #expect(refl["ingress_rule"].asString == "deny_ingress", "\(refl.serialize())")
            #expect(refl["egress_rule"].asString == "silent_egress", "\(refl.serialize())")
            #expect(refl["sealed"].asBool == true, "\(refl.serialize())")
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
            #expect(row["id"].asInt != nil, "a task id is an int from the one counter: \(row.serialize())")

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
            let taskId = try #require(row["id"].asInt)
            let cancelled = await cancelTask(kernel, taskId)
            #expect(cancelled["cancelled"].asBool == true, "\(cancelled.serialize())")
            #expect(await waitProcessGone(pid), "pid \(pid) survived cancel — ORPHANED")

            let rows = await psRows(kernel)
            #expect(!rows.contains { $0["id"].asInt == taskId }, "the cancelled row is gone")
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
            let taskA = try #require(rows.first?["id"].asInt)

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

            _ = await kernel.send([Item(AgentId("kernel_state"), .object(["type": .string("delete_agent"), "id": .string(a.value)]))])[0].result
            #expect(await waitProcessGone(pid), "pid \(pid) survived delete — ORPHANED")
        }
    }

    // ─── the relay: denials, releases, the frame bound ───────────────
    //
    // The relay is the unit under test here, so these drive it over a socketpair
    // the way a spawned program would — no subprocess, and no race between the
    // child registering a callback and the frame coming back.

    /// The PROGRAM's end of a control socket, with a relay serving the other.
    private func wire(_ kernel: Kernel, owner: AgentId, jobId: String) -> (Int32, Relay) {
        var fds: [Int32] = [0, 0]
        precondition(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0, "socketpair")
        let relay = Relay(fd: fds[0], owner: owner, jobId: jobId, kernel: kernel)
        // A4.34 — the relay IS a kernel task; drive it the way `start` does.
        _ = kernel.spawnTask(agent: owner, verb: "relay") { [relay] in
            await relay.run(taskId: KernelTaskLocals.currentTask)
        }
        _ = fcntl(fds[1], F_SETFL, O_NONBLOCK)  // so a test can never block on read
        return (fds[1], relay)
    }

    private func wireWrite(_ fd: Int32, _ v: JSON) {
        var body = Data([UInt8(ascii: "T")])
        body.append(Data(v.serialize().utf8))
        wireWriteRaw(fd, UInt32(body.count), body)
    }

    /// Write a length prefix and whatever follows it — the length is a parameter so
    /// a test can state one the payload does not honour.
    private func wireWriteRaw(_ fd: Int32, _ len: UInt32, _ body: Data) {
        var frame = Data([
            UInt8((len >> 24) & 0xFF), UInt8((len >> 16) & 0xFF),
            UInt8((len >> 8) & 0xFF), UInt8(len & 0xFF),
        ])
        frame.append(body)
        frame.withUnsafeBytes { raw in
            var off = 0
            while off < raw.count {
                let w = write(fd, raw.baseAddress!.advanced(by: off), raw.count - off)
                if w <= 0 { return }
                off += w
            }
        }
    }

    /// `n` bytes, `nil` on EOF or timeout.
    private func wireReadExactly(_ fd: Int32, _ n: Int, timeout: Double) async -> Data? {
        var buf = Data()
        let deadline = Date().addingTimeInterval(timeout)
        while buf.count < n && Date() < deadline {
            var chunk = [UInt8](repeating: 0, count: n - buf.count)
            let r = read(fd, &chunk, chunk.count)
            if r > 0 {
                buf.append(contentsOf: chunk[0..<r])
            } else if r == 0 {
                return nil  // EOF
            } else if errno == EAGAIN || errno == EWOULDBLOCK {
                try? await Task.sleep(nanoseconds: 10_000_000)
            } else {
                return nil
            }
        }
        return buf.count == n ? buf : nil
    }

    private func wireRead(_ fd: Int32, timeout: Double = 5) async -> JSON? {
        guard let head = await wireReadExactly(fd, 4, timeout: timeout) else { return nil }
        let n = head.reduce(0) { Int($0) << 8 | Int($1) }
        guard let body = await wireReadExactly(fd, n, timeout: timeout) else { return nil }
        return try? JSON.parse(Data(body.dropFirst()))
    }

    /// True once the relay has closed its end of the socket.
    private func wireClosed(_ fd: Int32, timeout: Double = 5) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        var byte = [UInt8](repeating: 0, count: 1)
        while Date() < deadline {
            let r = read(fd, &byte, 1)
            if r == 0 { return true }
            if r < 0 && (errno == EAGAIN || errno == EWOULDBLOCK) {
                try? await Task.sleep(nanoseconds: 20_000_000)
                continue
            }
            return false
        }
        return false
    }

    @Suite("exec_bridge relay")
    struct ExecBridgeRelayTests {

        /// `kernel.watch` is gated exactly like a send and can REFUSE. Off the
        /// handler's task-local this relay watched as the OPERATOR — ungated, so the
        /// refusal was not merely discarded, it could never happen; the program held a
        /// subscription that simply never fired, deaf with nothing to diagnose.
        @Test func aDeniedWatchReachesTheProgramAndRegistersNothing() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "denied_w")
            _ = await createPy(kernel, id: "secret")  // no edge from the runner
            let (fd, relay) = wire(kernel, owner: a, jobId: "j1")
            defer {
                relay.stop()
                close(fd)
            }
            wireWrite(fd, .object(["op": .string("watch"), "src": .string("secret")]))
            let ev = try #require(await wireRead(fd), "no frame came back")
            #expect(ev["op"].asString == "event", "\(ev.serialize())")
            #expect(ev["src"].asString == "secret", "\(ev.serialize())")
            #expect(ev["payload"]["reason"].asString == "unauthorized", "\(ev.serialize())")
            // …and NOTHING was registered.
            let secret = try #require(kernel.agent(AgentId("secret")))
            #expect(secret.watcherIds().isEmpty, "a denied watch left a watcher")
        }

        /// Every proxy the relay held is given back when the job goes: the watcher
        /// rows on the source, and the drain tasks behind them. And the ids carry the
        /// bundle's OWN prefix — two spawner bundles in one kernel must not collide,
        /// and this one runs COMMANDS, so `_py…` was a lie as well as a hazard.
        @Test func stoppingAWatchingProgramReleasesItsProxies() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "relay_rel")
            let (fd, relay) = wire(kernel, owner: a, jobId: "j2")
            defer { close(fd) }
            // `$self` is a baseline edge, so both of these are permitted.
            wireWrite(fd, .object(["op": .string("watch"), "src": .string(a.value)]))
            wireWrite(fd, .object(["op": .string("onmessage")]))

            let proxies: Set<AgentId> = [AgentId("_xbw_j2_\(a.value)"), AgentId("_xbself_j2")]
            let owner = try #require(kernel.agent(a))
            let deadline = Date().addingTimeInterval(5)
            while Date() < deadline && !proxies.isSubset(of: owner.watcherIds()) {
                try await Task.sleep(nanoseconds: 20_000_000)
            }
            #expect(
                proxies.isSubset(of: owner.watcherIds()),
                "the `_xb` proxies were never registered: \(owner.watcherIds())")

            relay.stop()
            let gone = Date().addingTimeInterval(5)
            while Date() < gone && !owner.watcherIds().isDisjoint(with: proxies) {
                try await Task.sleep(nanoseconds: 20_000_000)
            }
            #expect(
                owner.watcherIds().isDisjoint(with: proxies),
                "a proxy leaked past teardown: \(owner.watcherIds())")
        }

        /// A length prefix over the bound is a WIRE VIOLATION — the framing itself is
        /// unusable, so there is nothing to reply TO and the socket closes.
        @Test func anOversizedFrameClosesTheControlSocket() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "big_frame")
            let (fd, relay) = wire(kernel, owner: a, jobId: "j3")
            defer {
                relay.stop()
                close(fd)
            }
            wireWriteRaw(fd, UInt32(16 * 1024 * 1024 + 1), Data())
            #expect(await wireClosed(fd), "the relay must close on a wire violation")
        }
    }

    @Suite("exec_bridge refusals")
    struct ExecBridgeRefusalTests {

        /// EVERY refusal on `start` carries a CODE. Sibling argument errors used to
        /// split — `args`/`env` answered a typed message while `code`/`command`/the
        /// policy wall answered a bare `{error}` with no `reason` at all.
        @Test func everyStartRefusalCarriesACode() async {
            let kernel = freshKernel()
            let a = await create(kernel, id: "codes", allowed: ["echo( .*)?"])
            func ask(_ p: OrderedDictionary<String, JSON>) async -> JSON {
                var payload = p
                payload["type"] = .string("start")
                return await kernel.send([Item(a, .object(payload))])[0].result
            }
            var r = await ask([:])
            #expect(r["reason"].asString == "missing_arg", "\(r.serialize())")
            #expect(r["arg"].asString == "command", "\(r.serialize())")
            r = await ask(["command": .string("echo"), "code": .string("x = 1")])
            #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
            #expect(r["arg"].asString == "code", "\(r.serialize())")
            r = await ask(["command": .string("echo"), "args": .array([.object(["no": .integer(1)])])])
            #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
            #expect(r["arg"].asString == "args", "\(r.serialize())")
            r = await ask(["command": .string("echo"), "env": .object(["K": .array([])])])
            #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
            #expect(r["arg"].asString == "env", "\(r.serialize())")
            // A POLICY wall, not a malformed argument.
            r = await ask(["command": .string("rm"), "args": .array([.string("-rf")])])
            #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
            #expect(r["hint"].asString?.contains("allowed") == true, "\(r.serialize())")
        }

        private func create(
            _ kernel: Kernel, id: String, allowed: [String]
        ) async -> AgentId {
            let r = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "handler_module": .string("exec_bridge.tools"), "id": .string(id),
                "ingress_rule": .string("allow_all_ingress"),
                "allowed": .array(allowed.map(JSON.string)),
            ]))])[0].result
            return AgentId(r["id"].asString ?? id)
        }

        /// `status`/`stop`/`interrupt` on a job_id naming nothing answer `not_found`
        /// with the id. `stop`/`interrupt` used to answer `{killed:0}` — the same
        /// reply as "it had already exited", so a typo read as success.
        @Test func anUnknownJobIdIsNotFoundOnEveryVerb() async {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "nf")
            for verb in ["status", "stop", "interrupt"] {
                let r = await kernel.send([Item(a, .object([
                    "type": .string(verb), "job_id": .string("deadbeef"),
                ]))])[0].result
                #expect(r["reason"].asString == "not_found", "\(verb): \(r.serialize())")
                #expect(r["job_id"].asString == "deadbeef", "\(verb): \(r.serialize())")
            }
        }

        /// `kernel.args` / FANTASTIC_ARGS was advertised by every runtime's howto and
        /// set by no spawner — a program written from the docs read `{}` and
        /// KeyError'd. A command runner takes its arguments as ARGV.
        @Test func theConnectorHasNoArgs() async {
            #expect(!CONNECTOR_SRC.contains("FANTASTIC_ARGS"))
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "noargs")
            let r = await kernel.send([Item(a, .object(["type": .string("connector")]))])[0].result
            let howto = r["howto"].asString ?? ""
            #expect(!howto.contains("`kernel.self_id`, `kernel.args`"), "\(howto)")
            #expect(howto.contains("ARGV"), "\(howto)")
        }
    }

    // ─── durable jobs, end to end ───────────────────────────────────

    private func drainUntil(
        _ stream: Inbox, timeout: Double = 25,
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
        let consumer = Task { for await ev in stream { sink.add(ev.payload) } }
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
                py, "r = kernel.send('kernel_state', {'type': 'reflect'})\nprint('GOT', r.get('id'))",
                connector: true))])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let done = events.first { $0["type"].asString == "job_done" }
            #expect(
                done?["stdout"].asString?.contains("GOT kernel_state") == true,
                "connector send did not round-trip: \(done?.serialize() ?? "none")")
        }

        /// `connector` hands over the reference wire client — a model pastes it instead
        /// of re-deriving the socket framing (measured 2026-09-01: ten passes lost).
        @Test func theConnectorVerbHandsOverTheReferenceClient() async {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "dur_conn")
            let r = await kernel.send([Item(a, .object(["type": .string("connector")]))])[0].result
            let src = r["source"].asString ?? ""
            #expect(src.contains("FANTASTIC_CTRL_FD") && src.contains("def send("), "\(r.serialize())")
            #expect(r["howto"].asString?.contains("can_call") == true)
        }

        /// PARITY WITH PYTHON: a program's sends carry the RUNNER's identity and are
        /// gated by its grants — a sibling the runner holds no `can_call` for refuses
        /// (`unauthorized`). Before 2026-09-01 the relay forwarded with NO sender —
        /// operator, ungated — and a spawned program could reach any agent
        /// (measured live: E30 subai, "privileged as the root").
        @Test func theProgramIsGatedAsItsRunnerNotAsTheOperator() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "dur_gate")
            _ = await createPy(kernel, id: "sib")  // sibling: no edge from `a`
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([Item(a, pyStart(
                py,
                "r = kernel.send('sib', {'type': 'reflect'})\nprint('SIB', r.get('reason'))\n"
                    + "r2 = kernel.send('kernel_state', {'type': 'reflect'})\nprint('CORE', r2.get('id'))",
                connector: true))])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let out = events.first { $0["type"].asString == "job_done" }?["stdout"].asString ?? ""
            #expect(out.contains("SIB unauthorized"), "the sibling must refuse: \(out)")
            #expect(out.contains("CORE kernel_state"), "the parent (baseline edge) still answers: \(out)")
        }

        /// PROGRESS IS LIVE. The pump used to buffer both pipes to EOF and emit every
        /// line in one burst at exit, so exactly the jobs that need `progress` — a
        /// durable tool that prints a line per request and never exits — produced no
        /// output events at all and reported `lines: 0` for their whole life. This
        /// program never exits, so a burst-at-exit pump can only time out here.
        @Test func progressArrivesWhileTheProcessIsStillRunning() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "live_prog")
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([Item(a, pyStart(
                py, "import sys, time\nprint('FIRST')\nsys.stdout.flush()\ntime.sleep(30)"))])[0].result
            let events = await drainUntil(stream, timeout: 20) { $0["type"].asString == "progress" }
            let p = events.first { $0["type"].asString == "progress" }
            #expect(p?["line"].asString == "FIRST", "no live progress: \(events.count) events")
            #expect(p?["stream"].asString == "stdout")
            // …and the process is STILL RUNNING: this is not the exit burst.
            let st = await kernel.send([Item(a, .object(["type": .string("status")]))])[0].result
            let row = st["jobs"].asArray?.first
            #expect(row?["status"].asString == "running", "\(st.serialize())")
            #expect(row?["lines"].asInt == 1, "the row must count the line LIVE: \(st.serialize())")
            #expect(row?["last_line"].asString == "FIRST", "\(st.serialize())")
            _ = await kernel.send([Item(a, .object(["type": .string("stop")]))])[0].result
        }

        /// A cancelled job's row finalises to `stopped` — not `failed` (the SIGKILL's
        /// own status, indistinguishable from a program that failed on its own) — and
        /// `clear` then forgets it. python/rust parity.
        @Test func aCancelledJobFinalisesToStoppedAndClearRemovesIt() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "dur_cancel")
            let file = pidFile()
            defer { try? FileManager.default.removeItem(at: file) }
            _ = await kernel.send([Item(a, pyStart(py, sleepForeverCode(file)))])[0].result
            let pid = try await waitForPid(file)
            let row = try await waitForRow(kernel, agent: a.value, verb: "start")
            _ = await cancelTask(kernel, try #require(row["id"].asInt))
            #expect(await waitProcessGone(pid), "pid \(pid) survived cancel")

            var status = ""
            let deadline = Date().addingTimeInterval(10)
            while Date() < deadline {
                let st = await kernel.send([Item(a, .object(["type": .string("status")]))])[0].result
                status = st["jobs"].asArray?.first?["status"].asString ?? ""
                if status != "running" { break }
                try await Task.sleep(nanoseconds: 50_000_000)
            }
            #expect(status == "stopped", "the cancelled row settled as \(status)")

            let cleared = await kernel.send([Item(a, .object(["type": .string("clear")]))])[0].result
            #expect(cleared["cleared"].asInt ?? 0 >= 1, "\(cleared.serialize())")
            let left = await kernel.send([Item(a, .object(["type": .string("status")]))])[0].result
            #expect(left["jobs"].asArray?.count == 0, "\(left.serialize())")
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
            let outData = sp.stdout.readDataToEndOfFile()
            _ = await sp.child.wait()
            close(sp.ctrlFd)
            let text = String(data: outData, encoding: .utf8) ?? ""
            #expect(text.contains("sock True"), "control fd is not a socket: \(text)")
            #expect(text.contains("self owner_agent"), "self_id not delivered: \(text)")
            #expect(text.contains("env hello"), "per-call env not delivered: \(text)")
        }
    }

    // MARK: - raw bytes on the wire

    /// A stream-speaking stand-in for `file_bridge`: `write_stream` keeps the blob,
    /// `read_stream` hands it back as the reply BODY — the binary channel only.
    private final class BlobStore: AgentBundle, @unchecked Sendable {
        let name = "blob"
        private let lock = NSLock()
        private var blob = Data()
        func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
            .object(["error": .string("text channel: \(payload["type"].asString ?? "")")])
        }
        func handleBinary(
            agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
        ) async throws -> (JSON?, Data?) {
            switch header["type"].asString ?? "" {
            case "write_stream":
                lock.withLock { self.blob = blob }
                return (.object(["written": .integer(Int64(blob.count))]), nil)
            case "read_stream":
                let b = lock.withLock { self.blob }
                return (.object(["eof": .bool(true), "size": .integer(Int64(b.count))]), b)
            default:
                return (.object(["error": .string(header["type"].asString ?? "")]), nil)
            }
        }
    }

    @Suite("exec_bridge wire bytes")
    struct ExecBridgeWireBytesTests {

        /// `write_stream`/`read_stream` from a program: the bytes ride the fd as the
        /// io_bridge binary frame (kind B) both ways — never base64, never None
        /// (measured 2026-09-02, E30 subai:python: a `read_stream` reply was
        /// unserializable on this wire and the worker got None).
        @Test func aStreamChunkCrossesTheWireRaw() async throws {
            guard let py = py3() else { return }
            let registry = BundleRegistry()
            registry.register("exec_bridge.tools", ExecBridgeBundle())
            registry.register("blob.tools", BlobStore())
            let kernel = Kernel(storage: .inMemory, bundles: registry)
            let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
            kernel.register(root)
            kernel.setRoot(root)
            let a = await createPy(
                kernel, id: "dur_bytes", meta: ["can_call": .object(["blob": .string("*")])])
            _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "id": .string("blob"), "handler_module": .string("blob.tools"),
                "callable_by": .object([a.value: .string("*")]),
            ]))])[0].result
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([Item(a, pyStart(
                py,
                "raw = bytes(range(256)) * 3\n"
                    + "w = kernel.send('blob', {'type': 'write_stream', 'path': 'b', 'bytes': raw})\n"
                    + "r = kernel.send('blob', {'type': 'read_stream', 'path': 'b', 'bytes': b''})\n"
                    + "print('WROTE', w['written'], 'READ', len(r['bytes']), r['bytes'] == raw, "
                    + "type(r['bytes']).__name__)",
                connector: true))])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let out = events.first { $0["type"].asString == "job_done" }?["stdout"].asString ?? ""
            #expect(out.contains("WROTE 768 READ 768 True bytes"), "bytes did not round-trip raw: \(out)")
        }
    }

    // ─── A4 python-parity twins ─────────────────────────────────────
    //
    // One test per plan item, each pinning the PYTHON text or behaviour named
    // there (`PB/io/exec_bridge/src/exec_bridge/`). Python is canonical; where a
    // line here disagrees with it, this file is wrong.

    /// python's `readme.md`, for the byte-for-byte pin.
    private func pythonExecReadme() throws -> String {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // FantasticExecBridgeTests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // swift
            .deletingLastPathComponent()  // lib
            .appendingPathComponent(
                "python/bundled_agents/io/exec_bridge/src/exec_bridge/readme.md")
        return try String(contentsOf: url, encoding: .utf8)
    }

    /// Echoes back whatever payload it was dispatched — the only way to see what
    /// the relay actually forwarded.
    private final class EchoBundle: AgentBundle, @unchecked Sendable {
        let name = "echo"
        func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
            .object(["got": payload])
        }
    }

    private func echoKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("exec_bridge.tools", ExecBridgeBundle())
        registry.register("echo.tools", EchoBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    @Suite("exec_bridge python parity")
    struct ExecBridgeParityTests {

        // MARK: A4.1 / A4.2 / A4.26 / A4.27 — reflect

        /// A4.1 — reflect carries the full auth posture: `ingress_rule`,
        /// `egress_rule` (default `silent_egress`) and `sealed`, in python's order.
        @Test func reflectReportsEgressPosture() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "egr")
            let r = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
            let keys = try #require(r.asObject).keys
            #expect(r["ingress_rule"].asString == "allow_all_ingress", "\(r.serialize())")
            #expect(r["egress_rule"].asString == "silent_egress", "\(r.serialize())")
            #expect(r["sealed"].asBool == false, "\(r.serialize())")
            let i = try #require(keys.firstIndex(of: "ingress_rule"))
            let e = try #require(keys.firstIndex(of: "egress_rule"))
            let s = try #require(keys.firstIndex(of: "sealed"))
            #expect(i < e && e < s, "python order is ingress_rule, egress_rule, sealed")
        }

        /// A4.2 + A4.26 + A4.27 — the prose blocks are python's, verbatim: the
        /// `authoring` tag example, the `wire` passage about the fd not being
        /// one-way, and `inbox`'s closing clause about a script that only defines
        /// something.
        @Test func reflectTextsMatchPythonVerbatim() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "texts")
            let r = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
            #expect(
                r["authoring"].asString
                    == "A general-purpose command runner: <type>start</type> <command>…</command> <args><item>…</item></args>, gated by the `allowed` regexes on the full command line.",
                "\(r["authoring"].serialize())")
            let wire = r["wire"].asString ?? ""
            #expect(
                wire.contains(
                    "{op:'reply', rid:<same id>, data} — the fd is not one-way. It is not inline, though: read frames in a loop and skip any whose rid isn't yours (event/inbox frames can arrive first) until you see your reply."
                ), "\(wire)")
            #expect(
                r["inbox"].asString
                    == "Any verb NOT listed in `verbs` is MAIL for the running program, delivered to its on_message and ACKed {queued:true}. With no live job it is an error — a script that only defines something exits before its first message arrives.",
                "\(r["inbox"].serialize())")
        }

        /// A4.28 — the vendored readme IS python's `readme.md`, byte for byte.
        @Test func vendoredReadmeMatchesPythonByteForByte() throws {
            #expect(ExecBridgeBundle().readme == (try pythonExecReadme()))
        }

        /// A4.10 — reflect coerces the way python does: a FALSY `cwd` is
        /// `<process default>` (truthiness, not presence), and `allowed` items go
        /// through `str(x)`, so `5` reads `5` and `true` reads `True`.
        @Test func reflectCoercesLikePython() async throws {
            let kernel = freshKernel()
            let a = await createPy(
                kernel, id: "coer",
                meta: ["cwd": .string(""), "allowed": .array([.integer(5), .bool(true)])])
            let r = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
            #expect(r["cwd"].asString == "<process default>", "\(r.serialize())")
            #expect(
                r["allowed"].asArray?.compactMap { $0.asString } == ["5", "True"],
                "\(r.serialize())")
        }

        // MARK: A4.3 — `shutdown` is mail

        /// A4.3 — python's VERBS table has no `shutdown`, so it is MAIL: ungated,
        /// `{queued:true}` with a live job and the unknown-type error without one.
        /// It is not advertised as a verb either.
        @Test func shutdownIsMailNotAVerb() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "sd_open")
            let refl = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
            #expect(refl["verbs"].asObject?["shutdown"] == nil, "\(refl["verbs"].serialize())")

            let r = await kernel.send([Item(a, .object(["type": .string("shutdown")]))])[0].result
            #expect(
                r["error"].asString
                    == "exec_bridge: unknown type 'shutdown' (no running job to receive it)",
                "\(r.serialize())")
            #expect(r["reason"].isNull, "mail is not a refusal: \(r.serialize())")

            // …and it is UNGATED: a sealed leg answers the same, not `unauthorized`.
            let sealedId = await kernel.send([
                Item(
                    AgentId("kernel_state"),
                    .object([
                        "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                        "handler_module": .string("exec_bridge.tools"),
                        "id": .string("sd_sealed"),
                    ]))
            ])[0].result
            #expect(sealedId["id"].asString == "sd_sealed", "\(sealedId.serialize())")
            let s = await kernel.send([
                Item(AgentId("sd_sealed"), .object(["type": .string("shutdown")]))
            ])[0].result
            #expect(s["reason"].isNull, "shutdown must not be gated: \(s.serialize())")
            #expect(s["error"].asString?.contains("no running job") == true, "\(s.serialize())")
        }

        // MARK: A4.4 / A4.5 — python repr in every refusal

        /// A4.4 — `{x!r}` everywhere: the refused command line, the job id, and the
        /// unknown mail type (absent ⇒ `None`). The allowlist is python's LIST repr.
        @Test func refusalTextsUsePythonRepr() async throws {
            let kernel = freshKernel()
            let rec = kernel.agent(
                await createPy(
                    kernel, id: "repr_leg",
                    meta: ["allowed": .array([.string("ls( .*)?")])]))
            #expect(
                commandPermitted(rec, "rm -rf /")
                    == "exec_bridge: command not allowed: 'rm -rf /' — this leg permits: ['ls( .*)?']",
                "\(commandPermitted(rec, "rm -rf /") ?? "nil")")

            let a = await createPy(kernel, id: "repr_job")
            let gone = await kernel.send([
                Item(a, .object(["type": .string("stop"), "job_id": .string("nope")]))
            ])[0].result
            #expect(gone["error"].asString == "exec_bridge: no job 'nope'", "\(gone.serialize())")
            #expect(gone["reason"].asString == "not_found", "\(gone.serialize())")

            // An absent `type` is python's `None`, not an empty string. The kernel
            // refuses a typeless payload at the batch (`send: payload.type
            // required`), so this branch is reached at the bundle.
            let bare = try await ExecBridgeBundle().handle(
                agentId: a, payload: .object(["note": .string("hi")]), kernel: kernel)
            #expect(
                bare?["error"].asString
                    == "exec_bridge: unknown type None (no running job to receive it)",
                "\(bare?.serialize() ?? "nil")")
        }

        /// A4.5 — `validate_meta` texts are python's: the tag-shaped `allowed`
        /// example, and the spec/pattern spelled with `{x!r}`. The engine's own
        /// wording past `': '` is the residual ICU cannot reproduce.
        @Test func validateMetaTextsMatchPython() {
            let b = ExecBridgeBundle()
            #expect(
                b.validateMeta(["allowed": .array([])], record: [:])
                    == "allowed must be a non-empty list of regexes, "
                    + "e.g. <allowed><item>ls( .*)?</item></allowed>")
            let broken = b.validateMeta(["allowed": .array([.string("(")])], record: [:]) ?? ""
            #expect(
                broken.hasPrefix("allowed contains a broken regex '(': "), "\(broken)")
            #expect(
                b.validateMeta(["ingress_rule": .string("nonsense")], record: [:])
                    == "ingress_rule='nonsense' is not a rule this bundle implements — "
                    + "legal values: allow_all_ingress, deny_ingress")
            #expect(
                b.validateMeta(["auth": .string("password")], record: [:])
                    == "auth='password' is not applicable to the process edge: "
                    + "a leg with no wire envelope can never present a token — "
                    + "legal values: allow_all_ingress, deny_ingress")
        }

        // MARK: A4.7 / A4.33 — the regex dialect

        /// A4.7 (D5) — `allowed` is python's `re`: named groups spelled `(?P<n>…)`,
        /// a `(?P=n)` backreference and lookaround all compile and match.
        @Test func allowedAcceptsPythonRegexSyntax() async throws {
            let kernel = freshKernel()
            let named = kernel.agent(
                await createPy(
                    kernel, id: "rx_named",
                    meta: ["allowed": .array([.string("(?P<w>\\w+) (?P=w)")])]))
            #expect(commandPermitted(named, "ls ls") == nil, "python named group + backref")
            #expect(commandPermitted(named, "ls cat") != nil, "the backref must bind")

            let look = kernel.agent(
                await createPy(
                    kernel, id: "rx_look",
                    meta: ["allowed": .array([.string("(?!rm).*")])]))
            #expect(commandPermitted(look, "ls -la") == nil)
            #expect(commandPermitted(look, "rm -rf /") != nil, "the lookahead must refuse")

            // …and the same source compiles at WRITE time.
            #expect(
                ExecBridgeBundle().validateMeta(
                    ["allowed": .array([.string("(?P<w>\\w+) (?P=w)")])], record: [:]) == nil)
        }

        /// A4.33 — a python `fullmatch` BACKTRACKS across alternation. ICU's
        /// leftmost `firstMatch` stops at `ls` and the old range check then refused
        /// `ls -la`; `\A(?:…)\z` makes the whole string the goal.
        @Test func fullmatchBacktracksAcrossAlternation() async throws {
            let kernel = freshKernel()
            let rec = kernel.agent(
                await createPy(
                    kernel, id: "rx_alt",
                    meta: ["allowed": .array([.string("ls|ls -la")])]))
            #expect(commandPermitted(rec, "ls -la") == nil, "python fullmatch accepts this")
            #expect(commandPermitted(rec, "ls") == nil)
            #expect(commandPermitted(rec, "ls -l") != nil)
        }

        // MARK: A4.8 — job_id truthiness

        /// A4.8 — python reads `job_id` under TRUTHINESS: a falsy one means every
        /// job of this agent; a truthy NON-string can never be a key, so it is
        /// `not_found` echoing the raw value.
        @Test func emptyJobIdMeansAllJobs() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "jid")
            for jid in [JSON.string(""), .null, .integer(0), .array([])] {
                let st = await kernel.send([
                    Item(a, .object(["type": .string("status"), "job_id": jid]))
                ])[0].result
                #expect(
                    st["jobs"].asArray?.count == 0,
                    "falsy \(jid.serialize()): \(st.serialize())")
            }
            let bad = await kernel.send([
                Item(a, .object(["type": .string("status"), "job_id": .integer(5)]))
            ])[0].result
            #expect(bad["error"].asString == "exec_bridge: no job 5", "\(bad.serialize())")
            #expect(bad["job_id"] == .integer(5), "the RAW value is echoed: \(bad.serialize())")
            // `clear` has no not_found guard in python — an unknown id clears nothing.
            let cleared = await kernel.send([
                Item(a, .object(["type": .string("clear"), "job_id": .integer(5)]))
            ])[0].result
            #expect(cleared["cleared"].asInt == 0, "\(cleared.serialize())")
        }

        // MARK: A4.9 / A4.31 / A4.32 — start

        /// A4.9 — python's coercions: an explicit null `args` is not a list (the
        /// KEY being absent is `[]`), scalars go through `str()` so a bool reads
        /// `True`, a falsy `env` is the empty block, and a command that is all
        /// whitespace is MISSING.
        @Test func startCoercesArgumentsLikePython() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "coerce")
            func start(_ extra: OrderedDictionary<String, JSON>) async -> JSON {
                var p: OrderedDictionary<String, JSON> = [
                    "type": .string("start"), "command": .string("/bin/echo"),
                ]
                for (k, v) in extra { p[k] = v }
                return await kernel.send([Item(a, .object(p))])[0].result
            }
            let nullArgs = await start(["args": .null])
            #expect(nullArgs["reason"].asString == "bad_arg", "\(nullArgs.serialize())")
            #expect(nullArgs["arg"].asString == "args", "\(nullArgs.serialize())")

            let objArgs = await start(["args": .object([:])])
            #expect(objArgs["reason"].asString == "bad_arg", "\(objArgs.serialize())")

            // A falsy `env` is `{}` — `payload.get("env") or {}`, not a type error.
            for falsy in [JSON.null, .array([]), .object([:]), .string(""), .integer(0)] {
                let r = await start(["env": falsy, "args": .array([.string("x")])])
                #expect(r["job_id"].asString != nil, "falsy env must spawn: \(r.serialize())")
                _ = await kernel.send([Item(a, .object(["type": .string("stop")]))])[0].result
            }
            let badEnv = await start(["env": .array([.integer(1)])])
            #expect(badEnv["arg"].asString == "env", "\(badEnv.serialize())")

            // `command.strip()` spans newlines and tabs.
            let blank = await kernel.send([
                Item(a, .object(["type": .string("start"), "command": .string(" \n\t ")]))
            ])[0].result
            #expect(blank["reason"].asString == "missing_arg", "\(blank.serialize())")
            #expect(blank["error"].asString == "start: command required", "\(blank.serialize())")
        }

        /// A4.9 (the argv half) — scalars reach the child as python's `str(x)`:
        /// `1`, `2.5`, `True`.
        @Test func startStringifiesScalarArgsLikePython() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "argv_str")
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([
                Item(
                    a,
                    .object([
                        "type": .string("start"), "command": .string(py),
                        "args": .array([
                            .string("-u"), .string("-c"),
                            .string("import sys; print(sys.argv[1:])"),
                            .integer(1), .double(2.5), .bool(true),
                        ]),
                    ]))
            ])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let out = events.first { $0["type"].asString == "job_done" }?["stdout"].asString ?? ""
            #expect(out.contains("['1', '2.5', 'True']"), "argv was not python-stringified: \(out)")
        }

        /// A4.31 — a command that never reaches `exec` is a TRANSPORT refusal whose
        /// text is python's `str(OSError)`.
        @Test func aMissingCommandIsASpawnFailedRefusal() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "nospawn")
            let r = await kernel.send([
                Item(
                    a,
                    .object([
                        "type": .string("start"),
                        "command": .string("/nonexistent/fantastic_nope"),
                    ]))
            ])[0].result
            #expect(r["reason"].asString == "transport_error", "\(r.serialize())")
            #expect(
                r["error"].asString
                    == "exec_bridge: spawn failed: [Errno 2] No such file or directory: "
                    + "'/nonexistent/fantastic_nope'", "\(r.serialize())")
        }

        /// A4.32 — `secrets.token_hex(4)` is ALWAYS 8 hex digits; a `radix: 16`
        /// conversion dropped the leading zeros of one job in sixteen.
        @Test func jobIdIsEightHexChars() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "jobid_hex")
            for _ in 0..<8 {
                let r = await kernel.send([Item(a, pyStart(py, "pass"))])[0].result
                let jid = try #require(r["job_id"].asString)
                #expect(jid.count == 8, "job_id `\(jid)` is not 8 chars")
                #expect(
                    jid.allSatisfy { $0.isHexDigit && !$0.isUppercase },
                    "job_id `\(jid)` is not hex")
            }
        }

        // MARK: A4.6 / A4.14 / A4.15 / A4.29 / A4.30 — the pump

        /// A4.6 — a signal death is python's `returncode`: `-signal`, `ok:false`.
        @Test func aKilledJobReportsNegativeSignalExitCode() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "sigkill")
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([Item(a, pyStart(py, "import time; time.sleep(60)"))])[0].result
            try await Task.sleep(nanoseconds: 300_000_000)
            _ = await kernel.send([Item(a, .object(["type": .string("stop")]))])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let done = events.first { $0["type"].asString == "job_done" }
            #expect(done?["exit_code"].asInt == -9, "\(done?.serialize() ?? "no job_done")")
            #expect(done?["ok"].asBool == false, "\(done?.serialize() ?? "none")")
        }

        /// A4.8, the other half of the `no job` guard — a job_id naming a job that
        /// DOES exist must fall THROUGH it. `known ? nil : noJob(want)` unified
        /// both branches at `JSON` (which is `ExpressibleByNilLiteral`), so the
        /// guard handed back `.some(.null)` for every LIVE job: `stop {job_id}`
        /// answered `null`, signalled nothing, and the process ran to completion
        /// with `job_done exit_code: 0`. Python answers `{killed: 1}` and the job
        /// dies with -9. The unnamed `stop` was unaffected, which is why this
        /// needs its own twin.
        @Test func aNamedLiveJobIsStoppedNotAnsweredNull() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "named_stop")
            let stream = kernel.ensureInbox(a)
            let started = await kernel.send([
                Item(a, pyStart(py, "import time; time.sleep(60)"))
            ])[0].result
            let jid = try #require(started["job_id"].asString)
            try await Task.sleep(nanoseconds: 300_000_000)
            let stopped = await kernel.send([
                Item(a, .object(["type": .string("stop"), "job_id": .string(jid)]))
            ])[0].result
            #expect(stopped["killed"].asInt == 1, "\(stopped.serialize())")
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let done = events.first { $0["type"].asString == "job_done" }
            #expect(done?["exit_code"].asInt == -9, "\(done?.serialize() ?? "no job_done")")
            #expect(done?["job_id"].asString == jid, "\(done?.serialize() ?? "none")")
        }

        /// A4.14 — a line past 64 KiB is read whole: python's reader limit is
        /// 16 MiB (2026-09-20), where it used to end the pipe with `[pump error]`.
        @Test func anOverlongLineIsReadWhole() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "overlong")
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([
                Item(a, pyStart(py, "import sys; sys.stdout.write('x' * 150000 + '\\n')"))
            ])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let done = events.first { $0["type"].asString == "job_done" }
            #expect(
                done?["stdout"].asString?.hasPrefix(String(repeating: "x", count: 150000)) == true,
                "\(done?.serialize().prefix(200) ?? "none")")
            #expect(done?["stderr"].asString == "", "\(done?["stderr"].serialize() ?? "none")")
        }

        /// A4.15 — the counter lives on the PUMP: python's `_read_stream` closes
        /// over the job dict, so `clear {job_id}` dropping a RUNNING row does not
        /// reset `progress.n`.
        @Test func progressCountSurvivesClear() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "clr_count")
            let stream = kernel.ensureInbox(a)
            let r = await kernel.send([
                Item(
                    a,
                    pyStart(
                        py,
                        "import sys, time\n"
                            + "for i in range(6):\n    print(i)\n    sys.stdout.flush()\n    time.sleep(0.3)\n"
                    ))
            ])[0].result
            let jid = try #require(r["job_id"].asString)
            // FORGET the row while it still runs — a NAMED clear drops a running one.
            try await Task.sleep(nanoseconds: 500_000_000)
            let cleared = await kernel.send([
                Item(a, .object(["type": .string("clear"), "job_id": .string(jid)]))
            ])[0].result
            #expect(
                cleared["cleared"].asInt == 1,
                "a named clear drops a running row: \(cleared.serialize())")
            // ONE consumer per stream: the inbox buffers, so this sees the whole run.
            let events = await drainUntil(stream, timeout: 20) { $0["n"].asInt == 6 }
            let ns = events.compactMap { $0["type"].asString == "progress" ? $0["n"].asInt : nil }
            #expect(ns.contains(6), "the count restarted or stopped after clear: \(ns)")
        }

        /// A4.29 — invalid UTF-8 is REPLACED, not dropped: python decodes with
        /// `errors="replace"`, so the rest of the output survives.
        @Test func invalidUtf8OutputIsReplacedNotDropped() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "bad_utf8")
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([
                Item(
                    a,
                    pyStart(py, "import sys; sys.stdout.buffer.write(b'\\xff' + b'ok\\n')"))
            ])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let out = events.first { $0["type"].asString == "job_done" }?["stdout"].asString ?? ""
            #expect(out.contains("ok"), "the valid tail was dropped: \(out.debugDescription)")
            #expect(
                out.contains("\u{FFFD}"),
                "the invalid byte was not replaced: \(out.debugDescription)")
        }

        /// A4.30 — a cancelled job emits NO `job_done`: python's pump re-raises
        /// `CancelledError` after labelling the row, so nothing announces a normal
        /// finish for a job the kernel killed.
        @Test func aCancelledJobEmitsNoJobDone() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "no_done")
            let file = pidFile()
            defer { try? FileManager.default.removeItem(at: file) }
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([Item(a, pyStart(py, sleepForeverCode(file)))])[0].result
            let pid = try await waitForPid(file)
            let row = try await waitForRow(kernel, agent: a.value, verb: "start")
            _ = await cancelTask(kernel, try #require(row["id"].asInt))
            #expect(await waitProcessGone(pid), "pid \(pid) survived cancel")
            let events = await drainUntil(stream, timeout: 3) { _ in false }
            #expect(
                !events.contains { $0["type"].asString == "job_done" },
                "a cancelled job announced job_done: \(events.map { $0.serialize() })")
        }

        // MARK: A4.12 — start order

        /// A4.12 — `status` lists jobs in START order, the way python's dict does.
        @Test func statusListsJobsInStartOrder() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "order")
            var started: [String] = []
            for i in 0..<5 {
                let r = await kernel.send([
                    Item(
                        a,
                        .object([
                            "type": .string("start"), "command": .string(py),
                            "args": .array([
                                .string("-u"), .string("-c"),
                                .string("import time; time.sleep(30)"),
                                .string("tag\(i)"),
                            ]),
                        ]))
                ])[0].result
                started.append(try #require(r["job_id"].asString))
            }
            let st = await kernel.send([Item(a, .object(["type": .string("status")]))])[0].result
            let listed = st["jobs"].asArray?.compactMap { $0["job_id"].asString } ?? []
            #expect(listed == started, "listed \(listed), started \(started)")
            _ = await kernel.send([Item(a, .object(["type": .string("stop")]))])[0].result
        }

        // MARK: A4.11 / A4.13 / A4.34 — the relay

        /// A4.11 — python coerces both relay frames: `m.get("payload") or {}` and
        /// `m.get("target") or owner_id`, so a program that omits either is not
        /// forwarding a null.
        @Test func relayCoercesFalsyPayloadAndTarget() async throws {
            let kernel = echoKernel()
            let a = await createPy(
                kernel, id: "relay_coer",
                meta: ["can_call": .object(["echo1": .string("*")])])
            _ = await kernel.send([
                Item(
                    AgentId("kernel_state"),
                    .object([
                        "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                        "id": .string("echo1"), "handler_module": .string("echo.tools"),
                        "callable_by": .object([a.value: .string("*")]),
                    ]))
            ])[0].result
            let (fd, relay) = wire(kernel, owner: a, jobId: "jc")
            defer {
                relay.stop()
                close(fd)
            }
            // No `payload` key at all — python forwards `{}`, not None.
            wireWrite(
                fd,
                .object([
                    "op": .string("send"), "rid": .integer(1), "target": .string("echo1"),
                ]))
            let reply = try #require(await wireRead(fd), "no reply frame")
            #expect(reply["op"].asString == "reply", "\(reply.serialize())")
            // The batch tells the two apart: a `null` payload is `send: payload
            // required`, an EMPTY OBJECT is `send: payload.type required`. Seeing the
            // second is seeing the `or {}`.
            #expect(
                reply["data"]["arg"].asString == "payload.type",
                "the falsy payload was not coerced to {}: \(reply.serialize())")

            // …and a payload that carries a type reaches the target intact.
            wireWrite(
                fd,
                .object([
                    "op": .string("send"), "rid": .integer(2), "target": .string("echo1"),
                    "payload": .object(["type": .string("ping")]),
                ]))
            let echoed = try #require(await wireRead(fd), "no second reply frame")
            #expect(
                echoed["data"]["got"]["type"].asString == "ping", "\(echoed.serialize())")

            // An emit with no target lands on the OWNER agent.
            let owner = kernel.ensureInbox(a)
            wireWrite(
                fd,
                .object([
                    "op": .string("emit"), "payload": .object(["type": .string("ping")]),
                ]))
            let seen = await drainUntil(owner, timeout: 5) { $0["type"].asString == "ping" }
            #expect(
                seen.contains { $0["type"].asString == "ping" },
                "\(seen.map { $0.serialize() })")
        }

        /// A4.13 — a watching program's drains are kernel TASKS (`watch_drain` /
        /// `inbox_drain`), parented to the relay, so `ps` shows what it holds.
        /// A4.34 — and the relay itself is one, verb `relay`.
        @Test func aWatchingProgramHoldsDrainTaskRows() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "drains")
            let (fd, relay) = wire(kernel, owner: a, jobId: "jd")
            defer {
                relay.stop()
                close(fd)
            }
            wireWrite(fd, .object(["op": .string("watch"), "src": .string(a.value)]))
            wireWrite(fd, .object(["op": .string("onmessage")]))
            let relayRow = try await waitForRow(kernel, agent: a.value, verb: "relay")
            let watchRow = try await waitForRow(kernel, agent: a.value, verb: "watch_drain")
            let inboxRow = try await waitForRow(kernel, agent: a.value, verb: "inbox_drain")
            #expect(watchRow["parent"].asInt == relayRow["id"].asInt, "\(watchRow.serialize())")
            #expect(inboxRow["parent"].asInt == relayRow["id"].asInt, "\(inboxRow.serialize())")
            // C36 — and each fork is a `task` ROW under the same id, so a cold
            // read of the message log says what the job held, not just `ps`.
            let stored = kernel.rows.forAgent(a).filter { $0["kind"]?.asString == "task" }
            let forked = stored.map { ($0["id"]?.asInt, $0["verb"]?.asString) }
            for row in [relayRow, watchRow, inboxRow] {
                #expect(
                    forked.contains { $0 == row["id"].asInt && $1 == row["verb"].asString },
                    "no task row for \(row["verb"].serialize()): \(forked)")
            }
        }

        /// A4.34 — a real job holds a `relay` row for as long as its control socket
        /// lives, so a program's sends are rowed under work that is still alive.
        @Test func aJobHoldsARelayTaskRow() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "relay_row")
            _ = await kernel.send([Item(a, pyStart(py, "import time; time.sleep(30)"))])[0].result
            let row = try await waitForRow(kernel, agent: a.value, verb: "relay")
            #expect(row["agent"].asString == a.value, "\(row.serialize())")
            _ = await kernel.send([Item(a, .object(["type": .string("stop")]))])[0].result
        }

        // MARK: A4.16 — the gate takes no credential from the record

        /// A4.16 — python builds the ingress action from the target and the payload
        /// ALONE. A record that stores the group token used to satisfy its own
        /// `password_ingress` — the leg letting itself in.
        @Test func theRecordAuthTokenIsNeverACredential() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "self_cred")
            let envName = "FANTASTIC_TEST_TOKEN_A416"
            setenv(envName, "s3cret", 1)
            defer { unsetenv(envName) }
            // Written straight onto the record: `validate_meta` refuses this spelling
            // at the WRITE, and the point here is the GATE, not the write.
            let agent = try #require(kernel.agent(a))
            _ = agent.updateMeta([
                "ingress_rule": .object([
                    "type": .string("password_ingress"), "env": .string(envName),
                ]),
                "auth_token": .string("s3cret"),
            ])
            let r = await kernel.send([Item(a, .object(["type": .string("status")]))])[0].result
            #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
            #expect(
                r["error"].asString?.contains("group token") == true, "\(r.serialize())")
        }
    }

#endif  // os(macOS)
