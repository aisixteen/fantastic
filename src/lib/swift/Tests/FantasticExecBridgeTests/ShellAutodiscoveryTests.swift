// A PTY `start` with no `command` runs the CHILD's own `$SHELL` — twins of python
// `exec_bridge/tests/test_shell_autodiscovery.py`.
//
// The shell is AUTODISCOVERED from the environment the record passes down
// (`inherit_env`/`env`) — never from the kernel's env unless inherited, never from
// a fallback chain. The allowlist still decides: the record's `shell` field
// (`true` → `[$SHELL]`, `"login"` → `[$SHELL, "-l"]`) permits that exact line and
// nothing wider; every other argv, and every pipe job, still needs `allowed`.
//
// The "shell" here is a tiny executable script, so the tests read the same on
// every machine: it prints its argv and echoes one typed line back.
#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation
    import OrderedCollections
    import Testing

    @testable import FantasticExecBridge

    private let PTY: JSON = ["cols": 80, "rows": 24]

    /// An executable that stands in for a login shell: it announces its argv, then
    /// echoes one typed line — enough to prove WHICH line was spawned and that it
    /// sits on the terminal. One per test, in its own temp dir.
    private func fakeShell() throws -> String {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("fakesh_\(UUID().uuidString.prefix(8))")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let path = dir.appendingPathComponent("fakesh").path
        let script = "#!/bin/sh\necho \"SHELL_UP $*\"\nread line\necho \"GOT $line\"\n"
        try script.write(toFile: path, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: path)
        return path
    }

    private func shellKernel() -> Kernel {
        let registry = BundleRegistry()
        registry.register("exec_bridge.tools", ExecBridgeBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    /// Create an OPEN leg with NO `allowed` unless the test names one — the shell
    /// permission is the subject, so nothing else may quietly permit the line.
    @discardableResult
    private func mk(_ k: Kernel, _ id: String, _ meta: OrderedDictionary<String, JSON> = [:])
        async -> JSON
    {
        var payload: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": "exec_bridge.tools", "id": .string(id),
            "ingress_rule": "allow_all_ingress", "cwd": ".",
        ]
        for (key, v) in meta { payload[key] = v }
        return await k.send([Item(AgentId("kernel_state"), .object(payload))])[0].result
    }

    private func call(_ k: Kernel, _ id: String, _ p: OrderedDictionary<String, JSON>) async -> JSON
    {
        await k.send([Item(AgentId(id), .object(p))])[0].result
    }

    /// Drain `inbox` until `pred(output text)` holds; returns the accumulated
    /// output and every event seen on the way (the `job_started` among them).
    private func eventsUntil(_ inbox: Inbox, _ pred: (String) -> Bool) async -> (String, [JSON]) {
        var text = ""
        var events: [JSON] = []
        let deadline = Date().addingTimeInterval(15)
        while !pred(text) && Date() < deadline {
            if let item = inbox.tryTake() {
                events.append(item.payload)
                if item.payload["type"].asString == "output" {
                    text += item.payload["data"].asString ?? ""
                }
            } else {
                try? await Task.sleep(nanoseconds: 20_000_000)
            }
        }
        return (text, events)
    }

    @Suite("exec_bridge shell autodiscovery", .serialized)
    struct ShellAutodiscoveryTests {
        @Test func aPtyStartWithoutCommandRunsTheChildsShell() async throws {
            let fake = try fakeShell()
            let k = shellKernel()
            await mk(k, "sh", ["env": ["SHELL": .string(fake)], "shell": true])
            let inbox = k.ensureInbox(AgentId("sh"))
            let r = await call(k, "sh", ["type": "start", "pty": PTY])
            #expect(r["status"] == "running" && r["pty"] == true, "\(r.serialize())")
            let jid = r["job_id"].asString ?? ""
            let (text, events) = await eventsUntil(inbox) { $0.contains("SHELL_UP") }
            #expect(text.contains("SHELL_UP"), "\(text)")
            // The announcement names the RESOLVED shell, so a client that let the
            // kernel pick it can show it and re-issue it.
            let started = events.first { $0["type"] == "job_started" }
            #expect(
                started
                    == .object([
                        "type": "job_started", "job_id": .string(jid), "command": .string(fake),
                        "args": [],
                    ]), "\(started?.serialize() ?? "nil")")
            let snap = await call(k, "sh", ["type": "status", "job_id": .string(jid)])["jobs"][0]
            #expect(snap["command"].asString == fake, "\(snap.serialize())")
            #expect(snap["args"] == [] && snap["pty"] == true, "\(snap.serialize())")
            // It is a real terminal job: a typed line reaches the shell.
            _ = await call(k, "sh", ["type": "write", "job_id": .string(jid), "data": "hi\n"])
            let (got, _) = await eventsUntil(inbox) { $0.contains("GOT hi") }
            #expect(got.contains("GOT hi"), "\(got)")
            _ = await call(k, "sh", ["type": "stop"])
        }

        @Test func theShellComesFromTheChildsEnvNotTheKernels() async throws {
            let fake = try fakeShell()
            // The kernel HAS a SHELL — but the record passes nothing down, so the
            // child has none, and the start is refused naming where it would come
            // from. (Process-global env: restored on the way out.)
            let saved = ProcessInfo.processInfo.environment["SHELL"]
            setenv("SHELL", fake, 1)
            defer {
                if let saved { setenv("SHELL", saved, 1) } else { unsetenv("SHELL") }
            }
            let k = shellKernel()
            await mk(k, "sh", ["shell": true])
            let r = await call(k, "sh", ["type": "start", "pty": PTY])
            #expect(
                r["reason"] == "missing_arg" && r["arg"] == "SHELL", "\(r.serialize())")
            #expect(
                r["error"].asString
                    == "exec_bridge: start named no command and the child's environment has no SHELL to run — a PTY start without `command` runs the shell the record passes down, never a guessed one",
                "\(r.serialize())")
            #expect(
                r["hint"].asString
                    == #"update_agent sh inherit_env=["SHELL"] (or env={"SHELL": …}), or name `command`"#,
                "\(r.serialize())")
            // Passing it down by NAME is the whole repair.
            _ = await call(
                k, "kernel_state", ["type": "update_agent", "id": "sh", "inherit_env": ["SHELL"]])
            let ok = await call(k, "sh", ["type": "start", "pty": PTY])
            #expect(ok["status"] == "running", "\(ok.serialize())")
            _ = await call(k, "sh", ["type": "stop"])
        }

        @Test func anEmptyShellInTheChildEnvIsRefusedToo() async {
            let k = shellKernel()
            await mk(k, "sh", ["env": ["SHELL": ""], "shell": true])
            let r = await call(k, "sh", ["type": "start", "pty": PTY])
            #expect(
                r["reason"] == "missing_arg" && r["arg"] == "SHELL", "\(r.serialize())")
        }

        @Test func aPipeJobAlwaysNamesItsCommand() async throws {
            // Autodiscovery is PTY-only: a shell on a pipe has no terminal to talk to.
            let fake = try fakeShell()
            let k = shellKernel()
            await mk(k, "sh", ["env": ["SHELL": .string(fake)], "shell": true])
            let r = await call(k, "sh", ["type": "start"])
            #expect(
                r["reason"] == "missing_arg" && r["arg"] == "command", "\(r.serialize())")
            // …and an explicit empty command on a PTY is a typo, not "use the shell".
            let e = await call(k, "sh", ["type": "start", "command": "", "pty": PTY])
            #expect(
                e["reason"] == "missing_arg" && e["arg"] == "command", "\(e.serialize())")
        }

        @Test func shellTruePermitsOnlyTheBareShellLine() async throws {
            let fake = try fakeShell()
            let k = shellKernel()
            await mk(k, "sh", ["env": ["SHELL": .string(fake)], "shell": true])
            // extra args are a different line — `shell` does not cover it, and the
            // refusal says which line it DOES cover (python's exact suffix: the
            // value as JSON, the line as python's repr of `shlex.join`).
            let r = await call(k, "sh", ["type": "start", "args": ["-c", "id"], "pty": PTY])
            #expect(r["reason"] == "unauthorized", "\(r.serialize())")
            #expect(
                (r["error"].asString ?? "").hasSuffix(
                    " (`shell=true` permits only the PTY line \(pyRepr(fake)))"),
                "\(r.serialize())")
            // the same shell on a PIPE is not the permitted line either
            let pipe = await call(k, "sh", ["type": "start", "command": .string(fake)])
            #expect(pipe["reason"] == "unauthorized", "\(pipe.serialize())")
            // naming the shell explicitly on a PTY is the same line, so it is permitted
            let named = await call(
                k, "sh", ["type": "start", "command": .string(fake), "pty": PTY])
            #expect(named["status"] == "running", "\(named.serialize())")
            _ = await call(k, "sh", ["type": "stop"])
        }

        @Test func shellLoginPermitsExactlyTheLoginLine() async throws {
            let fake = try fakeShell()
            let k = shellKernel()
            await mk(k, "sh", ["env": ["SHELL": .string(fake)], "shell": "login"])
            let inbox = k.ensureInbox(AgentId("sh"))
            // the bare shell is NOT the login line
            let bare = await call(k, "sh", ["type": "start", "pty": PTY])
            #expect(bare["reason"] == "unauthorized", "\(bare.serialize())")
            #expect(
                (bare["error"].asString ?? "").hasSuffix(
                    " (`shell=\"login\"` permits only the PTY line \(pyRepr(fake + " -l")))"),
                "\(bare.serialize())")
            let r = await call(k, "sh", ["type": "start", "args": ["-l"], "pty": PTY])
            #expect(r["status"] == "running", "\(r.serialize())")
            let (text, _) = await eventsUntil(inbox) { $0.contains("SHELL_UP -l") }
            #expect(text.contains("SHELL_UP -l"), "\(text)")
            _ = await call(k, "sh", ["type": "stop"])
        }

        @Test func withoutShellTheResolvedLineStillNeedsAllowed() async throws {
            // No `shell` on the record: the autodiscovered line is judged by
            // `allowed` like any other — autodiscovery never widens the gate.
            let fake = try fakeShell()
            let k = shellKernel()
            await mk(k, "sh", ["env": ["SHELL": .string(fake)]])
            let r = await call(k, "sh", ["type": "start", "pty": PTY])
            #expect(r["reason"] == "unauthorized", "\(r.serialize())")
            _ = await call(
                k, "kernel_state",
                ["type": "update_agent", "id": "sh", "allowed": [.string(fake)]])
            let ok = await call(k, "sh", ["type": "start", "pty": PTY])
            #expect(ok["status"] == "running", "\(ok.serialize())")
            _ = await call(k, "sh", ["type": "stop"])
        }

        @Test func shellDoesNotDisableTheAllowlist() async throws {
            let fake = try fakeShell()
            let k = shellKernel()
            await mk(
                k, "sh",
                ["env": ["SHELL": .string(fake)], "shell": true, "allowed": ["/bin/echo .*"]])
            let r = await call(k, "sh", ["type": "start", "command": "/bin/echo", "args": ["x"]])
            #expect(r["status"] == "running", "\(r.serialize())")
        }

        @Test func shellIsRefusedAtWriteUnlessTrueFalseOrLogin() async {
            let k = shellKernel()
            let bad: [JSON] = ["bash", 1, "yes", ["login"]]
            for (i, v) in bad.enumerated() {
                let r = await mk(k, "bad_\(i)", ["shell": v])
                #expect(r["reason"] == "invalid_meta", "\(v.serialize()) → \(r.serialize())")
                #expect(
                    (r["error"].asString ?? "").contains(
                        "shell=\(pyRepr(v)) is not a shell permission — legal values: true "
                            + #"(the child's $SHELL, bare), "login" ($SHELL -l), false (none)"#),
                    "\(r.serialize())")
            }
            let good: [JSON] = [true, false, "login"]
            for (i, v) in good.enumerated() {
                let r = await mk(k, "ok_\(i)", ["shell": v])
                #expect(r["id"].asString == "ok_\(i)", "\(v.serialize()) → \(r.serialize())")
            }
        }

        @Test func reflectAdvertisesTheShellPermission() async {
            let k = shellKernel()
            await mk(k, "plain")
            await mk(k, "login", ["shell": "login"])
            #expect(await call(k, "plain", ["type": "reflect"])["shell"] == false)
            #expect(await call(k, "login", ["type": "reflect"])["shell"] == "login")
        }
    }

#endif
