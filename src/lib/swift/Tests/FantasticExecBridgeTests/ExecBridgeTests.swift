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

    import FantasticIoBridge
    import FantasticJSON
    @testable import FantasticKernel
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
            // WHERE a command runs is the record's, required — never guessed
            "cwd": .string("."),
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

        @Test func validateMetaRefusesABadAllowed() {
            let b = ExecBridgeBundle()
            #expect(b.validateMeta(["allowed": .array([])], record: [:]) != nil)
            #expect(
                b.validateMeta(["allowed": .array([.string("(")])], record: [:])?
                    .contains("broken regex") == true)
            #expect(
                b.validateMeta(["allowed": .array([.string("ls( .*)?"), .string(".*")])], record: [:])
                    == nil)
        }

        @Test func reflectAdvertisesTheAllowlist() async throws {
            let kernel = freshKernel()
            let a = await create(kernel, id: "refl", allowed: ["ls( .*)?"])
            let r = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
            #expect(r["id"].asString == a.value)
            #expect(r["running"].asInt == 0)
            #expect(r["allowed"].asArray?.compactMap { $0.asString } == ["ls( .*)?"])
            #expect(r.asObject?["wire"] == nil && r.asObject?["verbs"] == nil)
            let d = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("docs"), "of": .string(a.value), "tier": .string("detailed")]))])[0].result
            #expect(d["notes"]["wire"].asString?.contains("FANTASTIC_CTRL_FD") == true)
            let verbs = try #require(d["verbs"].asObject)
            for v in ["reflect", "start", "status", "clear", "interrupt", "stop", "boot"] {
                #expect(verbs[v] != nil, "verb \(v) missing from docs")
            }
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
    private func wire(_ kernel: Kernel, owner: AgentId, jobId: TaskId) -> (Int32, Relay) {
        var fds: [Int32] = [0, 0]
        precondition(socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0, "socketpair")
        let relay = Relay(fd: fds[0], owner: owner, jobId: jobId, kernel: kernel)
        // A4.34 — the relay IS a kernel task; drive it the way `start` does:
        // spawned from the owner's own handler, so it runs AS the owner.
        _ = kernel.asSelfNow(owner) {
            kernel.spawnTask(agent: owner, verb: "relay") { [relay] in
                await relay.run(taskId: KernelTaskLocals.currentTask)
            }
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

    /// The `{op:"inbox", payload:{type}}` frame a program's on_message receives.
    private func inboxFrame(_ type: String) -> JSON {
        .object(["op": .string("inbox"), "payload": .object(["type": .string(type)])])
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
            let (fd, relay) = wire(kernel, owner: a, jobId: TaskId(9001))
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
            let (fd, relay) = wire(kernel, owner: a, jobId: TaskId(9002))
            defer { close(fd) }
            // `$self` is a baseline edge, so both of these are permitted.
            wireWrite(fd, .object(["op": .string("watch"), "src": .string(a.value)]))
            wireWrite(fd, .object(["op": .string("onmessage")]))

            let proxies: Set<AgentId> = [AgentId("_xbw_9002_\(a.value)"), AgentId("_xbself_9002")]
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

        /// Audit BV-8 — the seal covers every verb but `reflect`/`connector`, MAIL
        /// INCLUDED. The fanout reaches the program's `_xbself_` proxy BEFORE the
        /// handler, so the relay's inbox drain drops what the gate refuses: a sealed
        /// leg's program receives nothing, and receives again once reopened.
        @Test func aSealedLegRefusesMailAndRelaysNothing() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "sealed_mail")
            let (fd, relay) = wire(kernel, owner: a, jobId: TaskId(9006))
            defer {
                relay.stop()
                close(fd)
            }
            wireWrite(fd, .object(["op": .string("onmessage")]))
            let owner = try #require(kernel.agent(a))
            let deadline = Date().addingTimeInterval(5)
            while Date() < deadline && !owner.watcherIds().contains(AgentId("_xbself_9006")) {
                try await Task.sleep(nanoseconds: 20_000_000)
            }
            func rule(_ r: String) async -> JSON {
                await kernel.send([
                    Item(
                        AgentId("kernel_state"),
                        .object([
                            "type": .string("update_agent"), "id": .string(a.value),
                            "ingress_rule": .string(r),
                        ]))
                ])[0].result
            }
            #expect(await rule("deny_ingress")["updated"] == .bool(true))
            for p: JSON in [
                .object(["type": .string("status")]),
                .object(["type": .string("probe_mail"), "x": .string("TEST-VALUE-MAIL")]),
            ] {
                let out = await kernel.send([Item(a, p)])[0].result
                #expect(out["reason"].asString == "unauthorized", "\(p) → \(out.serialize())")
            }
            _ = await rule("allow_all_ingress")
            _ = await kernel.send([Item(a, .object(["type": .string("after")]))])
            // the first frame down the fd is the mail sent AFTER the reopen
            let first = try #require(await wireRead(fd), "no frame came back")
            #expect(first == inboxFrame("after"), "\(first.serialize())")
        }

        /// Audit BV-9 (python `test_the_inbox_relays_a_drive_verb_only_for_its_own_job`)
        /// — the fanout mirrors every payload sent to the leg into the owner inbox,
        /// so a program started by one caller saw another caller's `write`
        /// keystrokes, paste bytes and drive calls aimed at that caller's job. A drive
        /// verb whose `job_id` names ANOTHER job is dropped; one with no `job_id`
        /// (bulk) or naming this job is forwarded; other mail is unchanged.
        @Test func theInboxRelaysADriveVerbOnlyForItsOwnJob() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "bv9_drive")
            let (fd, relay) = wire(kernel, owner: a, jobId: TaskId(9008))
            defer {
                relay.stop()
                close(fd)
            }
            wireWrite(fd, .object(["op": .string("onmessage")]))
            let owner = try #require(kernel.agent(a))
            let deadline = Date().addingTimeInterval(5)
            while Date() < deadline && !owner.watcherIds().contains(AgentId("_xbself_9008")) {
                try await Task.sleep(nanoseconds: 20_000_000)
            }
            let payloads: [JSON] = [
                ["type": "write", "job_id": 9009, "data": "TEST-VALUE-KEYS\n"],
                ["type": "resize", "job_id": 9009, "cols": 10, "rows": 5],
                ["type": "paste_image", "job_id": 9009, "bytes": "TEST-VALUE-PNG"],
                ["type": "stop", "job_id": 9009],
                ["type": "interrupt", "job_id": 9009],
                ["type": "clear", "job_id": 9009],
                // the bulk forms name no job: forwarded
                ["type": "stop"],
                ["type": "interrupt"],
                ["type": "clear"],
                ["type": "write", "job_id": 9008, "data": "own"],
                ["type": "mail", "n": 1],
            ]
            for p in payloads { _ = await kernel.send([Item(a, p)]) }
            let expected: [JSON] = [
                ["type": "stop"],
                ["type": "interrupt"],
                ["type": "clear"],
                ["type": "write", "job_id": 9008, "data": "own"],
                ["type": "mail", "n": 1],
            ]
            for want in expected {
                let got = try #require(await wireRead(fd), "no frame for \(want.serialize())")
                #expect(got == ["op": "inbox", "payload": want], "\(got.serialize())")
            }
        }

        /// Audit W-9 (python `test_a_malformed_ctrl_frame_is_skipped_never_fatal`) —
        /// JSON that is no envelope, or a `src` that is no string, is skipped; the
        /// relay keeps serving and the next call is answered. Python ended the relay
        /// task here.
        @Test func aMalformedCtrlFrameIsSkippedNeverFatal() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "w9_junk")
            let (fd, relay) = wire(kernel, owner: a, jobId: TaskId(9007))
            defer {
                relay.stop()
                close(fd)
            }
            let junk: [JSON] = [
                .array([.integer(1)]), .string("str"),
                .object(["op": .string("watch"), "src": .array([.string("x")])]),
                .object(["op": .string("watch"), "src": .object(["a": .integer(1)])]),
                .object(["op": .string("unwatch"), "src": .array([.string("x")])]),
                .object(["op": .string("unwatch"), "src": .object(["a": .integer(1)])]),
            ]
            for j in junk { wireWrite(fd, j) }
            wireWrite(
                fd,
                .object([
                    "op": .string("send"), "rid": .string("r1"), "target": .string(a.value),
                    "payload": .object(["type": .string("status")]),
                ]))
            let reply = try #require(await wireRead(fd), "the relay stopped answering")
            #expect(reply["op"].asString == "reply", "\(reply.serialize())")
            #expect(reply["rid"].asString == "r1", "\(reply.serialize())")
            #expect(reply["data"]["jobs"].asArray != nil, "\(reply.serialize())")
        }

        /// Audit ARCH2-5 — the owner-inbox table is keyed PER KERNEL. Two kernels in
        /// one process, an exec agent of the same id in each: keyed by the bare id,
        /// kernel B's `onmessage` took kernel A's entry, and A's own `offmessage`
        /// then found B's job there and released nothing — A's proxy stayed watched.
        @Test func theOwnerInboxIsPerKernel() async throws {
            let ka = freshKernel()
            let kb = freshKernel()
            let a = await createPy(ka, id: "xb_twin")
            let b = await createPy(kb, id: "xb_twin")
            let (fa, ra) = wire(ka, owner: a, jobId: TaskId(9100))
            let (fb, rb) = wire(kb, owner: b, jobId: TaskId(9101))
            defer {
                ra.stop()
                rb.stop()
                close(fa)
                close(fb)
            }
            let oa = try #require(ka.agent(a))
            let ob = try #require(kb.agent(b))
            func until(_ what: String, _ pred: () -> Bool) async throws {
                let deadline = Date().addingTimeInterval(5)
                while Date() < deadline && !pred() {
                    try await Task.sleep(nanoseconds: 20_000_000)
                }
                #expect(pred(), "\(what) never held")
            }
            wireWrite(fa, .object(["op": .string("onmessage")]))
            try await until("A subscribed") { oa.watcherIds().contains(AgentId("_xbself_9100")) }
            wireWrite(fb, .object(["op": .string("onmessage")]))
            try await until("B subscribed") { ob.watcherIds().contains(AgentId("_xbself_9101")) }
            // each kernel's program receives its own kernel's mail
            _ = await ka.send([Item(a, .object(["type": .string("mail_a")]))])
            #expect(await wireRead(fa) == inboxFrame("mail_a"))
            _ = await kb.send([Item(b, .object(["type": .string("mail_b")]))])
            #expect(await wireRead(fb) == inboxFrame("mail_b"))
            // A gives its inbox back — its OWN entry, not B's
            wireWrite(fa, .object(["op": .string("offmessage")]))
            try await until("A released") { !oa.watcherIds().contains(AgentId("_xbself_9100")) }
            // …and B's subscription is untouched
            #expect(ob.watcherIds().contains(AgentId("_xbself_9101")))
            _ = await kb.send([Item(b, .object(["type": .string("mail_b2")]))])
            #expect(await wireRead(fb) == inboxFrame("mail_b2"))
        }

        /// A length prefix over the bound is a WIRE VIOLATION — the framing itself is
        /// unusable, so there is nothing to reply TO and the socket closes.
        @Test func anOversizedFrameClosesTheControlSocket() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "big_frame")
            let (fd, relay) = wire(kernel, owner: a, jobId: TaskId(9003))
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
                    "type": .string(verb), "job_id": .integer(999_999),
                ]))])[0].result
                #expect(r["reason"].asString == "not_found", "\(verb): \(r.serialize())")
                #expect(r["job_id"] == .integer(999_999), "\(verb): \(r.serialize())")
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
            #expect(r["job_id"].asInt != nil, "no job_id: \(r.serialize())")
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
            wire(kernel, "dur_send", "kernel_state", ["reflect"])
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
        /// (measured live: E30 subai, "as the root").
        @Test func theProgramIsGatedAsItsRunnerNotAsTheOperator() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "dur_gate")
            _ = await createPy(kernel, id: "sib")  // sibling: no edge from `a`
            wire(kernel, "dur_gate", "kernel_state", ["reflect"])  // the one granted edge
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([Item(a, pyStart(
                py,
                "r = kernel.send('sib', {'type': 'reflect'})\nprint('SIB', r.get('reason'))\n"
                    + "r2 = kernel.send('kernel_state', {'type': 'reflect'})\nprint('CORE', r2.get('id'))",
                connector: true))])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let out = events.first { $0["type"].asString == "job_done" }?["stdout"].asString ?? ""
            #expect(out.contains("SIB unauthorized"), "the sibling must refuse: \(out)")
            #expect(out.contains("CORE kernel_state"), "the granted edge still answers: \(out)")
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
                env: ["MYVAR": "hello"], cwd: ".")
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
            agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
        ) async throws -> (JSON?, Blob?) {
            // the chunk is the payload's `bytes` — python's `payload.get("bytes", b"")`
            let blob = bodyAt(body, "bytes") ?? Data()
            switch header["type"].asString ?? "" {
            case "write_stream":
                lock.withLock { self.blob = blob }
                return (.object(["written": .integer(Int64(blob.count))]), nil)
            case "read_stream":
                let b = lock.withLock { self.blob }
                return (
                    .object(["eof": .bool(true), "size": .integer(Int64(b.count))]),
                    Blob("bytes", b)
                )
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

        /// A TEXT send (kind T, no bytes in the call) whose reply holds bytes comes
        /// back kind B: python's relay `encode_frame`s the reply dict, with no list of
        /// verbs, so the program needs no `bytes: b''` to get its chunk raw.
        @Test func aTextSendsBytesReplyComesBackRaw() async throws {
            guard let py = py3() else { return }
            let registry = BundleRegistry()
            registry.register("exec_bridge.tools", ExecBridgeBundle())
            registry.register("blob.tools", BlobStore())
            let kernel = Kernel(storage: .inMemory, bundles: registry)
            let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
            kernel.register(root)
            kernel.setRoot(root)
            let a = await createPy(
                kernel, id: "dur_text_bytes", meta: ["can_call": .object(["blob": .string("*")])])
            _ = await kernel.send([Item(AgentId("kernel_state"), .object([
                "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                "id": .string("blob"), "handler_module": .string("blob.tools"),
                "callable_by": .object([a.value: .string("*")]),
            ]))])[0].result
            let stream = kernel.ensureInbox(a)
            _ = await kernel.send([Item(a, pyStart(
                py,
                "raw = bytes(range(256))\n"
                    + "kernel.send('blob', {'type': 'write_stream', 'path': 'b', 'bytes': raw})\n"
                    + "r = kernel.send('blob', {'type': 'read_stream', 'path': 'b'})\n"
                    + "print('READ', r.get('error'), type(r.get('bytes')).__name__, "
                    + "r.get('bytes') == raw)",
                connector: true))])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let out = events.first { $0["type"].asString == "job_done" }?["stdout"].asString ?? ""
            #expect(out.contains("READ None bytes True"), "a text send's bytes reply was not raw: \(out)")
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
            let r = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("docs"), "of": .string(a.value), "tier": .string("detailed")]))])[0].result
            #expect(r["notes"].asObject.map { Array($0.keys) } == ["wire", "inbox", "tasks"])
            let wire = r["notes"]["wire"].asString ?? ""
            #expect(
                wire.contains(
                    "{op:'reply', rid:<same id>, data}. Read frames in a loop and skip any whose rid isn't yours (event/inbox frames can arrive first)."
                ), "\(wire)")
            #expect(
                r["notes"]["inbox"].asString
                    == "Any verb this bundle does not answer is MAIL for the running program, delivered to its on_message and ACKed {queued:true}. With no live job it is refused — a script that only defines something exits before its first message arrives.",
                "\(r["notes"]["inbox"].serialize())")
        }

        /// A4.28 — the vendored readme IS python's `readme.md`, byte for byte.
        @Test func vendoredReadmeMatchesPythonByteForByte() throws {
            #expect(ExecBridgeBundle().readme == (try pythonExecReadme()))
        }

        /// A4.10 — reflect reports the record's `cwd` as written (null when unset —
        /// there is no process default any more) and the `inherit_env` it runs with;
        /// `allowed` items go through `str(x)`, so `5` reads `5` and `true` reads `True`.
        @Test func reflectCoercesLikePython() async throws {
            let kernel = freshKernel()
            let a = await createPy(
                kernel, id: "coer",
                meta: [
                    "cwd": .string("/wd"), "inherit_env": .array([.string("PATH")]),
                    "allowed": .array([.integer(5), .bool(true)]),
                ])
            let r = await kernel.send([Item(a, .object(["type": .string("reflect")]))])[0].result
            #expect(r["cwd"].asString == "/wd", "\(r.serialize())")
            #expect(r["inherit_env"] == .array([.string("PATH")]), "\(r.serialize())")
            #expect(
                r["allowed"].asArray?.compactMap { $0.asString } == ["5", "True"],
                "\(r.serialize())")
            let b = await createPy(kernel, id: "coer2", meta: ["cwd": .null])
            let r2 = await kernel.send([Item(b, .object(["type": .string("reflect")]))])[0].result
            #expect(r2["cwd"].isNull, "\(r2.serialize())")
            #expect(r2["inherit_env"] == .array([]), "\(r2.serialize())")
        }

        // MARK: A4.3 — `shutdown` is mail

        /// A4.3 — python's VERBS table has no `shutdown`, so it is MAIL:
        /// `{queued:true}` with a live job and the unknown-type error without one.
        /// It is not advertised as a verb either. Gated like every verb but
        /// `reflect`/`connector` (audit BV-8): a sealed leg refuses it.
        @Test func shutdownIsMailNotAVerb() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "sd_open")
            let refl = await kernel.send([Item(AgentId("kernel"), .object(["type": .string("docs"), "of": .string(a.value)]))])[0].result
            #expect(refl["verbs"].asObject?["shutdown"] == nil, "\(refl["verbs"].serialize())")

            let r = await kernel.send([Item(a, .object(["type": .string("shutdown")]))])[0].result
            #expect(
                r["error"].asString
                    == "exec_bridge: unknown type 'shutdown' (no running job to receive it)",
                "\(r.serialize())")
            #expect(r["reason"].asString == "unhandled", "no live job — a coded refusal: \(r.serialize())")

            // …and, like all mail, it is GATED: a sealed leg refuses it (BV-8).
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
            #expect(s["reason"].asString == "unauthorized", "mail on a sealed leg: \(s.serialize())")
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
                Item(a, .object(["type": .string("stop"), "job_id": .integer(999_999)]))
            ])[0].result
            // A job's id is an int, and python's repr of an int is its digits.
            #expect(gone["error"].asString == "exec_bridge: no job 999999", "\(gone.serialize())")
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

        // MARK: A4.8 — the job_id argument

        /// A job's id IS its kernel task id — an int (python `_job_arg`). Absent (or
        /// an explicit null) means every job of this agent; an int is looked up and
        /// `not_found` echoes it; ANY other value — a string, even a stringified
        /// int, a bool, a float, a list — is a `bad_arg` naming `job_id`, never
        /// coerced and never read as "all".
        @Test func theJobIdArgumentIsAnIntOrAbsent() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "jid")
            let all = await kernel.send([
                Item(a, .object(["type": .string("status"), "job_id": .null]))
            ])[0].result
            #expect(all["jobs"].asArray?.count == 0, "absent is every job: \(all.serialize())")
            for jid in [
                JSON.string(""), .string("5"), .bool(false), .double(5.0), .array([]),
            ] {
                for verb in ["status", "stop", "interrupt", "clear", "write", "output"] {
                    let r = await kernel.send([
                        Item(a, .object(["type": .string(verb), "job_id": jid]))
                    ])[0].result
                    #expect(
                        r["reason"].asString == "bad_arg" && r["arg"].asString == "job_id",
                        "\(verb) \(jid.serialize()): \(r.serialize())")
                }
            }
            for n in [Int64(0), 5] {
                let bad = await kernel.send([
                    Item(a, .object(["type": .string("status"), "job_id": .integer(n)]))
                ])[0].result
                #expect(bad["error"].asString == "exec_bridge: no job \(n)", "\(bad.serialize())")
                #expect(bad["job_id"] == .integer(n), "the id is echoed: \(bad.serialize())")
            }
            // `clear` has no not_found guard in python — an unknown id clears nothing.
            let cleared = await kernel.send([
                Item(a, .object(["type": .string("clear"), "job_id": .integer(5)]))
            ])[0].result
            #expect(cleared["cleared"].asInt == 0, "\(cleared.serialize())")
        }

        // MARK: A4.9 / A4.31 / A4.32 — start

        /// A4.9 — python's coercions: an explicit null `args` is not a list (the
        /// KEY being absent is `[]`), scalars go through `str()` so a bool reads
        /// `True`, `env`/`cwd` are the RECORD's (any per-call value refuses, only an
        /// explicit null is absent), and a command that is all whitespace is MISSING.
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

            // `env` and `cwd` are the RECORD's: any per-call value refuses (py `is not
            // None`), even an empty one; only an explicit null is "absent".
            for (arg, val) in [
                ("env", JSON.object([:])), ("env", .array([.string("A=1")])), ("cwd", .string("/")),
            ] {
                let r = await start([arg: val])
                #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
                #expect(r["arg"].asString == arg, "\(r.serialize())")
            }
            let nulls = await start(["env": .null, "cwd": .null, "args": .array([.string("x")])])
            #expect(nulls["job_id"].asInt != nil, "an explicit null is absent: \(nulls.serialize())")
            _ = await kernel.send([Item(a, .object(["type": .string("stop")]))])[0].result

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

        /// A bare command is found on the CHILD's `PATH` only (python `proc.resolve`):
        /// no `PATH` in the child env refuses naming it, and the kernel's own PATH is
        /// never searched — `posix_spawnp` would have.
        @Test func aBareCommandIsFoundOnTheChildsPathOnly() async throws {
            let fm = FileManager.default
            let bin = fm.temporaryDirectory.appendingPathComponent("ft-path-\(UUID().uuidString)")
            try fm.createDirectory(at: bin, withIntermediateDirectories: true)
            defer { try? fm.removeItem(at: bin) }
            let tool = bin.appendingPathComponent("ft_only_here")
            try "#!/bin/sh\necho found-by-child-path\n".write(
                to: tool, atomically: true, encoding: .utf8)
            try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: tool.path)
            let kernel = freshKernel()
            func start(_ id: AgentId, _ command: String) async -> JSON {
                await kernel.send([
                    Item(id, .object(["type": .string("start"), "command": .string(command)]))
                ])[0].result
            }
            let a = await createPy(kernel, id: "nopath")
            let r = await start(a, "echo")
            #expect(
                r["reason"].asString == "missing_arg" && r["arg"].asString == "PATH",
                "\(r.serialize())")
            let b = await createPy(
                kernel, id: "ownpath", meta: ["env": .object(["PATH": .string(bin.path)])])
            let stream = kernel.ensureInbox(b)
            let s = await start(b, "ft_only_here")
            #expect(s["job_id"].asInt != nil, "\(s.serialize())")
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let out = events.first { $0["type"].asString == "job_done" }?["stdout"].asString
            #expect(out == "found-by-child-path\n", "\(out ?? "nil")")
            // …and one only the kernel's PATH holds is not found.
            let miss = await start(b, "echo")
            #expect(
                miss["reason"].asString == "transport_error"
                    && (miss["error"].asString ?? "").contains("No such file"),
                "\(miss.serialize())")
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

        /// A MISSING `cwd` is named, not the command — CPython's `noexec:chdir`
        /// text. Measured 2026-10-06 (rust subai): `… 'python3'` for a leg whose cwd
        /// did not exist sent the model hunting an interpreter for twenty turns.
        @Test func aMissingCwdIsNamedNotTheCommand() async throws {
            let kernel = freshKernel()
            let gone = "/nonexistent/fantastic_no_dir_\(UUID().uuidString)"
            let a = await createPy(kernel, id: "nowhere", meta: ["cwd": .string(gone)])
            let r = await kernel.send([
                Item(a, .object(["type": .string("start"), "command": .string("/bin/echo"),
                                 "args": .array([.string("x")])]))
            ])[0].result
            #expect(r["reason"].asString == "transport_error", "\(r.serialize())")
            #expect(
                r["error"].asString
                    == "exec_bridge: spawn failed: [Errno 2] No such file or directory: '\(gone)'",
                "\(r.serialize())")
        }

        /// One id space: the job's id IS its kernel task id — `ps` lists it,
        /// `cancel` takes it, and the status snapshot carries no separate
        /// `task_id`. (The old random 8-hex ids came back from a model as ints or
        /// infinite floats — `04203581`, `458e5912` — and 3.7% of jobs could not
        /// be addressed again.) A string, even the stringified id, is a `bad_arg`.
        @Test func aJobIdIsTheJobsTaskIdAndNothingElseNamesIt() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "ids")
            let r = await kernel.send([Item(a, pyStart(py, "import time; time.sleep(30)"))])[0].result
            let jid = try #require(r["job_id"].asInt, "\(r.serialize())")
            let rows = await psRows(kernel)
            #expect(
                rows.contains { $0["id"].asInt == jid && $0["agent"].asString == "ids" },
                "ps lists the job under its id: \(rows)")
            for verb in ["status", "stop", "interrupt", "clear", "write", "output"] {
                let bad = await kernel.send([
                    Item(a, .object(["type": .string(verb), "job_id": .string(String(jid))]))
                ])[0].result
                #expect(
                    bad["reason"].asString == "bad_arg" && bad["arg"].asString == "job_id",
                    "\(verb): \(bad.serialize())")
            }
            let snap = await kernel.send([
                Item(a, .object(["type": .string("status"), "job_id": .integer(jid)]))
            ])[0].result["jobs"][0]
            #expect(snap["job_id"] == .integer(jid) && snap["task_id"].isNull, "\(snap.serialize())")
            _ = await kernel.send([
                Item(a, .object(["type": .string("stop"), "job_id": .integer(jid)]))
            ])
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
            let jid = try #require(started["job_id"].asInt)
            try await Task.sleep(nanoseconds: 300_000_000)
            let stopped = await kernel.send([
                Item(a, .object(["type": .string("stop"), "job_id": .integer(jid)]))
            ])[0].result
            #expect(stopped["killed"].asInt == 1, "\(stopped.serialize())")
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let done = events.first { $0["type"].asString == "job_done" }
            #expect(done?["exit_code"].asInt == -9, "\(done?.serialize() ?? "no job_done")")
            #expect(done?["job_id"].asInt == jid, "\(done?.serialize() ?? "none")")
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

        /// Audit BV-5 — a NAMED `clear` follows the bulk form's rule: a `running`
        /// (or `stopping`) row is refused. It used to drop the row whatever its
        /// status, and the row is the only handle `stop`, `cancel`, delete and
        /// shutdown kill a process through — the process outlived all four.
        @Test func aNamedClearRefusesARunningJob() async throws {
            guard let py = py3() else { return }
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "clr_named")
            let r = await kernel.send([Item(a, pyStart(py, "import time; time.sleep(30)"))])[0]
                .result
            let jid = try #require(r["job_id"].asInt)
            func clear() async -> JSON {
                await kernel.send([
                    Item(a, .object(["type": .string("clear"), "job_id": .integer(jid)]))
                ])[0].result
            }
            #expect(
                await clear().serialize()
                    == "{\"error\":\"clear: job_id must be a finished job (job \(jid) is running — "
                    + "`stop` it, then clear)\",\"reason\":\"bad_arg\",\"arg\":\"job_id\"}")
            _ = await kernel.send([
                Item(a, .object(["type": .string("stop"), "job_id": .integer(jid)]))
            ])
            var status = ""
            let deadline = Date().addingTimeInterval(10)
            while Date() < deadline {
                let st = await kernel.send([Item(a, .object(["type": .string("status")]))])[0]
                    .result
                status = st["jobs"].asArray?.first?["status"].asString ?? ""
                if status == "stopped" { break }
                try await Task.sleep(nanoseconds: 50_000_000)
            }
            #expect(status == "stopped", "the stopped row settled as \(status)")
            #expect(await clear() == .object(["cleared": .integer(1)]))
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
            var started: [Int64] = []
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
                started.append(try #require(r["job_id"].asInt))
            }
            let st = await kernel.send([Item(a, .object(["type": .string("status")]))])[0].result
            let listed = st["jobs"].asArray?.compactMap { $0["job_id"].asInt } ?? []
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
            let (fd, relay) = wire(kernel, owner: a, jobId: TaskId(9004))
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

        /// The relay serves a program's frames IN ORDER, one at a time — python's
        /// read loop awaits each `send` (and pushes its reply) and each `emit`
        /// before it reads the next frame. A detached Task per frame let emits
        /// and sends overtake one another.
        @Test func relayServesFramesInOrder() async throws {
            let kernel = echoKernel()
            let a = await createPy(
                kernel, id: "relay_order",
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
            let owner = kernel.ensureInbox(a)
            let (fd, relay) = wire(kernel, owner: a, jobId: TaskId(9006))
            defer {
                relay.stop()
                close(fd)
            }
            for i in 1...10 {
                wireWrite(
                    fd,
                    .object([
                        "op": .string("send"), "rid": .integer(Int64(i)),
                        "target": .string("echo1"), "payload": .object(["type": .string("ping")]),
                    ]))
                wireWrite(
                    fd,
                    .object([
                        "op": .string("emit"),
                        "payload": .object(["type": .string("seq"), "n": .integer(Int64(i))]),
                    ]))
            }
            var rids: [Int64] = []
            for _ in 1...10 {
                let reply = try #require(await wireRead(fd), "no reply frame")
                rids.append(reply["rid"].asInt ?? -1)
            }
            #expect(rids == Array(1...10), "replies out of order: \(rids)")
            let seen = await drainUntil(owner, timeout: 5) { $0["n"].asInt == 10 }
            #expect(
                seen.filter { $0["type"].asString == "seq" }.compactMap { $0["n"].asInt }
                    == Array(1...10), "\(seen.map { $0.serialize() })")
        }

        /// A4.13 — a watching program's drains are kernel TASKS (`watch_drain` /
        /// `inbox_drain`), parented to the relay, so `ps` shows what it holds.
        /// A4.34 — and the relay itself is one, verb `relay`.
        @Test func aWatchingProgramHoldsDrainTaskRows() async throws {
            let kernel = freshKernel()
            let a = await createPy(kernel, id: "drains")
            let (fd, relay) = wire(kernel, owner: a, jobId: TaskId(9005))
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
            let stored = kernel.rows.all().filter { $0["agent"]?.asString == a.value }.filter { $0["kind"]?.asString == "task" }
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


    // ─── the guest wire, read and written by hand ───────────────────

    /// Read exactly `n` bytes off a blocking fd, BOUNDED by `poll`: `nil` on EOF
    /// or after `timeout` seconds.
    private func readExactlyPolled(_ fd: Int32, _ n: Int, timeout: Double = 10) -> Data? {
        var buf = Data()
        let deadline = Date().addingTimeInterval(timeout)
        while buf.count < n {
            let left = deadline.timeIntervalSinceNow
            if left <= 0 { return nil }
            var p = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
            guard poll(&p, 1, Int32(left * 1000)) > 0 else { return nil }
            var chunk = [UInt8](repeating: 0, count: n - buf.count)
            let r = read(fd, &chunk, chunk.count)
            if r <= 0 { return nil }
            buf.append(contentsOf: chunk[0..<r])
        }
        return buf
    }

    /// One `[4B len | kind | rest]` frame off the parent end: `(kind, rest)`.
    private func readGuestFrame(_ fd: Int32) -> (UInt8, Data)? {
        guard let head = readExactlyPolled(fd, 4) else { return nil }
        let n = head.reduce(0) { Int($0) << 8 | Int($1) }
        guard let body = readExactlyPolled(fd, n), let kind = body.first else { return nil }
        return (kind, Data(body.dropFirst()))
    }

    private func writeGuestFrame(_ fd: Int32, _ v: JSON) {
        var body = Data([UInt8(ascii: "T")])
        body.append(Data(v.serialize().utf8))
        let n = UInt32(body.count)
        var frame = Data([
            UInt8((n >> 24) & 0xFF), UInt8((n >> 16) & 0xFF), UInt8((n >> 8) & 0xFF),
            UInt8(n & 0xFF),
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

    // ─── the guest wire: `_binary_path` is a list of steps ──────────────

    /// One kind-B frame onto the parent end: `[4B len | "B" | io_bridge frame]`.
    private func writeGuestBinary(_ fd: Int32, _ head: JSON, _ body: Data) {
        var rest = Data([UInt8(ascii: "B")])
        rest.append(Codec.encodeBinaryFrame(header: head, body: body))
        let n = UInt32(rest.count)
        var frame = Data([
            UInt8((n >> 24) & 0xFF), UInt8((n >> 16) & 0xFF), UInt8((n >> 8) & 0xFF),
            UInt8(n & 0xFF),
        ])
        frame.append(rest)
        frame.withUnsafeBytes { raw in
            var off = 0
            while off < raw.count {
                let w = write(fd, raw.baseAddress!.advanced(by: off), raw.count - off)
                if w <= 0 { return }
                off += w
            }
        }
    }

    /// Python `_echo` (`test_binary_path_steps.py`): answers the payload's keys —
    /// the body's own key among them — and hands the body at `a.b` back at the
    /// LITERAL reply key `x.y`, beside a nested `x` → `y`.
    private final class PathEcho: AgentBundle, @unchecked Sendable {
        let name = "path_echo"
        func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
            switch payload["type"].asString ?? "" {
            case "boot", "shutdown": return nil
            default: return .object(["channel": "text"])
            }
        }
        func handleBinary(
            agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
        ) async throws -> (JSON?, Blob?) {
            var keys = header.asObject.map { Array($0.keys) } ?? []
            if case .key(let k)? = body?.parts.first, body?.parts.count == 1 { keys.append(k) }
            return (
                .object([
                    "keys": .array(keys.sorted().map { .string($0) }),
                    "x.y": .null, "x": ["y": "kept"],
                ]), Blob("x.y", bodyAt(body, "a.b") ?? Data())
            )
        }
    }

    private let RAW = Data([0x00]) + Data("raw".utf8) + Data([0xFF])

    @Suite("exec_bridge guest wire — binary path steps")
    struct ExecBridgeGuestBinaryPathTests {

        /// The frame a program writes names the LITERAL key `a.b` as
        /// `["payload", "a.b"]`; a kind-B reply placed at `["data", "x.y"]` lands at
        /// the literal key in the program, beside a nested `x` → `y`. Python
        /// `test_the_guest_wire_carries_a_literal_key_both_ways`.
        @Test func theGuestWireCarriesALiteralKeyBothWays() async throws {
            guard let py = py3() else { print("skipping: no python3"); return }
            let code = CONNECTOR_SRC + """
                r = kernel.send('echo', {'type': 'take', 'a': {'b': 'kept'}, 'a.b': b'\\x00raw\\xff', 'items': [1]})
                print(sorted(r), r['x.y'], r['x'])

                """
            let s = try Spawn.job(argv: [py, "-u", "-c", code], selfId: "runner", env: [:], cwd: ".")
            defer { close(s.ctrlFd) }
            let (kind, rest) = try #require(readGuestFrame(s.ctrlFd), "a frame from the program")
            #expect(kind == UInt8(ascii: "B"))
            let (head, body) = try Codec.decodeBinaryFrame(rest)
            #expect(head["_binary_path"] == ["payload", "a.b"], "\(head.serialize())")
            #expect(
                head["payload"] == ["type": "take", "a": ["b": "kept"], "a.b": .null, "items": [1]],
                "\(head.serialize())")
            #expect(body == RAW)
            writeGuestBinary(
                s.ctrlFd,
                [
                    "op": "reply", "rid": head["rid"],
                    "data": ["x.y": .null, "x": ["y": "kept"]], "_binary_path": ["data", "x.y"],
                ], Data("back".utf8))
            let out = String(decoding: s.stdout.readDataToEndOfFile(), as: UTF8.self)
            _ = await s.child.wait()
            #expect(
                out.trimmingCharacters(in: .whitespacesAndNewlines)
                    == "['x', 'x.y'] b'back' {'y': 'kept'}", "\(out)")
        }

        /// A list slot is an INTEGER step. Python
        /// `test_the_guest_wire_carries_a_list_index_from_the_program`.
        @Test func theGuestWireCarriesAListIndexFromTheProgram() async throws {
            guard let py = py3() else { print("skipping: no python3"); return }
            let code =
                CONNECTOR_SRC
                + "kernel.emit({'type': 'chunks', 'items': [{'n': 0}, {'data': b'hi'}]})\n"
            let s = try Spawn.job(argv: [py, "-u", "-c", code], selfId: "runner", env: [:], cwd: ".")
            defer { close(s.ctrlFd) }
            let (kind, rest) = try #require(readGuestFrame(s.ctrlFd), "a frame from the program")
            #expect(kind == UInt8(ascii: "B"))
            let (head, body) = try Codec.decodeBinaryFrame(rest)
            #expect(head["_binary_path"] == ["payload", "items", 1, "data"], "\(head.serialize())")
            var env = head
            #expect(try Codec.placeBody(&env) == ["payload", "items", 1, "data"])
            #expect(
                env == ["op": "emit", "payload": ["type": "chunks", "items": [["n": 0], ["data": .null]]]],
                "\(env.serialize())")
            #expect(body == Data("hi".utf8))
            _ = s.stdout.readDataToEndOfFile()
            _ = await s.child.wait()
        }

        /// A kind-B EMIT carries its bytes to the owner's watchers. Python's relay
        /// emits the DECODED payload (`kernel.emit(owner_id, m.get("payload") or
        /// {})`), its bytes at their `_binary_path`; a watcher receives the payload
        /// with `null` in that slot and the body AT its steps — a list slot and a
        /// literal dotted key alike — not a head with the bytes dropped.
        @Test func aGuestEmitCarriesItsBytesToAWatcher() async throws {
            guard let py = py3() else { print("skipping: no python3"); return }
            let registry = BundleRegistry()
            registry.register("exec_bridge.tools", ExecBridgeBundle())
            let kernel = Kernel(storage: .inMemory, bundles: registry)
            let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
            kernel.register(root)
            kernel.setRoot(root)
            let runner = await createPy(kernel, id: "runner")
            let tap = AgentId("tap")
            let stream = kernel.ensureInbox(tap)
            #expect(kernel.watch(src: runner, watcher: tap) == nil)
            final class Sink: @unchecked Sendable {
                let lock = NSLock()
                var items: [InboxItem] = []
                func add(_ i: InboxItem) { lock.withLock { items.append(i) } }
                func all() -> [InboxItem] { lock.withLock { items } }
            }
            let sink = Sink()
            let consumer = Task { for await item in stream { sink.add(item) } }
            defer { consumer.cancel() }
            _ = await kernel.send([
                Item(
                    runner,
                    pyStart(
                        py,
                        "kernel.emit({'type': 'chunks', 'items': [{'n': 0}, {'data': b'hi'}]})\n"
                            + "kernel.emit({'type': 'blob', 'a.b': b'\\x00raw\\xff', 'a': {'b': 'kept'}})\n",
                        connector: true))
            ])[0].result
            let deadline = Date().addingTimeInterval(20)
            func emitted() -> [InboxItem] {
                sink.all().filter { ["chunks", "blob"].contains($0.payload["type"].asString ?? "") }
            }
            while emitted().count < 2 && Date() < deadline {
                try await Task.sleep(nanoseconds: 20_000_000)
            }
            let got = emitted()
            #expect(got.count == 2, "\(got.map { $0.payload.serialize() })")
            #expect(
                got.first?.payload == ["type": "chunks", "items": [["n": 0], ["data": .null]]],
                "\(got.first?.payload.serialize() ?? "nil")")
            #expect(got.first?.body == Blob(parts: ["items", 1, "data"], Data("hi".utf8)))
            #expect(
                got.last?.payload.serialize() == #"{"type":"blob","a.b":null,"a":{"b":"kept"}}"#,
                "\(got.last?.payload.serialize() ?? "nil")")
            #expect(got.last?.body == Blob(parts: ["a.b"], RAW))
        }

        /// The whole guest path: program → relay (decode) → a binary handler that
        /// finds its body at the literal key → relay (encode) → program. Python
        /// `test_a_job_round_trips_a_literal_key_through_the_kernel`.
        @Test func aJobRoundTripsALiteralKeyThroughTheKernel() async throws {
            guard let py = py3() else { print("skipping: no python3"); return }
            let registry = BundleRegistry()
            registry.register("exec_bridge.tools", ExecBridgeBundle())
            registry.register("path_echo.tools", PathEcho())
            let kernel = Kernel(storage: .inMemory, bundles: registry)
            let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
            kernel.register(root)
            kernel.setRoot(root)
            let runner = await createPy(
                kernel, id: "runner", meta: ["can_call": .object(["echo": .string("*")])])
            _ = await kernel.send([
                Item(
                    AgentId("kernel_state"),
                    .object([
                        "type": .string("create_agent"), "parent_id": .string("kernel_state"),
                        "id": .string("echo"), "handler_module": .string("path_echo.tools"),
                        "callable_by": .object([runner.value: .string("*")]),
                    ]))
            ])[0].result
            let stream = kernel.ensureInbox(runner)
            _ = await kernel.send([
                Item(
                    runner,
                    pyStart(
                        py,
                        "r = kernel.send('echo', {'type': 'take', 'a': {'b': 'kept'}, "
                            + "'a.b': b'\\x00raw\\xff'})\n"
                            + "print('GOT', r['keys'], r['x.y'] == b'\\x00raw\\xff', r['x'])",
                        connector: true))
            ])[0].result
            let events = await drainUntil(stream) { $0["type"].asString == "job_done" }
            let out = events.first { $0["type"].asString == "job_done" }?["stdout"].asString ?? ""
            // the reply's chunk lands in the program at the LITERAL key `x.y`
            #expect(out.contains("GOT ['a', 'a.b', 'type'] True {'y': 'kept'}"), "\(out)")
        }
    }

#endif  // os(macOS)
