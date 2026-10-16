// A terminal is a PTY JOB of exec_bridge (terminal_backend merged in) — twins of
// python `exec_bridge/tests/test_pty_jobs.py`, plus the record-owned environment.
#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation
    import OrderedCollections
    import Testing

    @testable import FantasticExecBridge

    private let CHILD = """
        import fcntl,os,struct,sys,termios,signal
        def size():
            r,c=struct.unpack('HHHH',fcntl.ioctl(0,termios.TIOCGWINSZ,b'\\0'*8))[:2]
            return c,r
        signal.signal(signal.SIGWINCH, lambda *a: print('WINCH %d %d' % size(), flush=True))
        print('TTY', os.isatty(0), os.environ.get('TERM'), flush=True)
        print('SIZE %d %d' % size(), flush=True)
        line = sys.stdin.readline()
        print('GOT', line.strip(), flush=True)
        """

    /// Floods past the high-water mark, then prints a marker and waits on a line.
    private let FLOOD = """
        import sys
        sys.stdout.write('x' * 150000 + '\\nDONE\\n')
        sys.stdout.flush()
        sys.stdin.readline()
        """

    private func py3() -> String? {
        for p in ["/usr/bin/python3", "/opt/homebrew/bin/python3", "/usr/local/bin/python3"]
        where FileManager.default.isExecutableFile(atPath: p) { return p }
        return nil
    }

    private func kernelWith(_ id: String, _ meta: OrderedDictionary<String, JSON> = [:]) async
        -> Kernel
    {
        let registry = BundleRegistry()
        registry.register("exec_bridge.tools", ExecBridgeBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        var payload: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": "exec_bridge.tools", "id": .string(id),
            "ingress_rule": "allow_all_ingress", "allowed": [".*"], "cwd": ".",
        ]
        for (k, v) in meta { payload[k] = v }
        _ = await kernel.send([Item(AgentId("kernel_state"), .object(payload))])
        return kernel
    }

    private func call(_ k: Kernel, _ id: String, _ p: OrderedDictionary<String, JSON>) async -> JSON
    {
        await k.send([Item(AgentId(id), .object(p))])[0].result
    }

    /// Collect `output` text from `inbox` until `pred`.
    private func outputUntil(_ inbox: Inbox, _ pred: (String) -> Bool) async -> String {
        var text = ""
        let deadline = Date().addingTimeInterval(15)
        while !pred(text) && Date() < deadline {
            if let item = inbox.tryTake() {
                if item.payload["type"].asString == "output" {
                    text += item.payload["data"].asString ?? ""
                }
            } else {
                try? await Task.sleep(nanoseconds: 20_000_000)
            }
        }
        return text
    }

    private func nextOf(_ inbox: Inbox, _ type: String) async -> JSON? {
        let deadline = Date().addingTimeInterval(15)
        while Date() < deadline {
            if let item = inbox.tryTake() {
                if item.payload["type"].asString == type { return item.payload }
            } else {
                try? await Task.sleep(nanoseconds: 20_000_000)
            }
        }
        return nil
    }

    /// The job's id — its kernel task id, an int on the wire.
    private func start(_ k: Kernel, _ id: String, _ py: String) async -> JSON {
        let r = await call(
            k, id,
            [
                "type": "start", "command": .string(py), "args": ["-c", .string(CHILD)],
                "pty": ["cols": 91, "rows": 37],
            ])
        #expect(r["pty"] == true, "\(r.serialize())")
        return r["job_id"]
    }

    @Suite("exec_bridge PTY jobs", .serialized)
    struct PtyJobTests {
        @Test func aPtyJobRunsOnATerminalAndTakesKeystrokes() async {
            guard let py = py3() else { return }
            let k = await kernelWith("term_a")
            let inbox = k.ensureInbox(AgentId("term_a"))
            let jid = await start(k, "term_a", py)
            let text = await outputUntil(inbox) { $0.contains("SIZE") }
            #expect(text.contains("TTY True xterm-256color"), "\(text)")
            #expect(text.contains("SIZE 91 37"), "\(text)")
            let w = await call(
                k, "term_a", ["type": "write", "job_id": jid, "data": "hello\n"])
            #expect(w == .object(["written": 6]), "\(w.serialize())")
            let got = await outputUntil(inbox) { $0.contains("GOT hello") }
            #expect(got.contains("GOT hello"), "\(got)")
            let done = await nextOf(inbox, "job_done")
            #expect(
                done
                    == .object([
                        "type": "job_done", "job_id": jid, "exit_code": 0, "ok": true,
                    ]), "\(done?.serialize() ?? "nil")")
        }

        /// A write or resize that comes after the job's terminal closed answers
        /// `.ended` and never touches a descriptor — the number the master held is
        /// free to be REUSED by the process, and a write through it would land in
        /// whatever file got it. Measured here by opening a file right after the
        /// close (the lowest free number — the master's own) and proving it stays
        /// empty. The race the lock closes is the same step taken mid-write: the
        /// read loop clears the fd under `fdLock` before closing, and a write
        /// or resize holds `fdLock` for its whole use of the fd.
        @Test func aWriteOrResizeAfterTheJobEndsTouchesNoDescriptor() async throws {
            let k = await kernelWith("term_end")
            let (child, master, parentFd) = try Spawn.pty(
                argv: ["/bin/sh", "-c", "exit 0"], selfId: "term_end",
                env: ["PATH": "/usr/bin:/bin"], cwd: "/", cols: 80, rows: 24)
            close(parentFd)
            let job = PtyJob(
                jobId: TaskId(1), owner: AgentId("term_end"), child: child, masterFd: master,
                cols: 80, rows: 24)
            _ = await job.run(kernel: k) { _ in }  // the terminal reached EOF and closed
            #expect(!job.isLive)
            let path = NSTemporaryDirectory() + "pty_reuse_\(UUID().uuidString)"
            let reused = open(path, O_CREAT | O_RDWR, 0o600)
            #expect(reused >= 0)
            defer {
                close(reused)
                unlink(path)
            }
            let w = await job.writeAll(Data("must not land\n".utf8))
            #expect(w == .failure(.ended), "\(w)")
            let r = job.resize(cols: 100, rows: 30)
            if case .failure(let f) = r { #expect(f == .ended) } else { Issue.record("resized a closed terminal") }
            #expect((job.cols, job.rows) == (80, 24), "a refused resize changes nothing")
            var st = stat()
            #expect(fstat(reused, &st) == 0 && st.st_size == 0, "the reused fd was written to")
        }

        /// Through the verb: the same end answers the job-ended refusal, not an errno.
        @Test func theVerbsAnswerJobEndedAfterTheTerminalCloses() async {
            let k = await kernelWith("term_end2", ["env": ["PATH": "/usr/bin:/bin"]])
            let inbox = k.ensureInbox(AgentId("term_end2"))
            let r = await call(
                k, "term_end2",
                [
                    "type": "start", "command": "/bin/sh", "args": ["-c", "exit 0"],
                    "pty": ["cols": 80, "rows": 24],
                ])
            let jid = r["job_id"]
            #expect(await nextOf(inbox, "job_done") != nil)
            for p: OrderedDictionary<String, JSON> in [
                ["type": "write", "job_id": jid, "data": "x"],
                ["type": "resize", "job_id": jid, "cols": 100, "rows": 30],
            ] {
                let out = await call(k, "term_end2", p)
                #expect(out["reason"] == "not_found", "\(out.serialize())")
                #expect(
                    out["error"].asString?.hasSuffix("has ended") ?? false, "\(out.serialize())")
            }
        }

        @Test func pasteImageWantsAMimeAndTypesASavedPath() async {
            guard let py = py3() else { return }
            let k = await kernelWith("term_p")
            let inbox = k.ensureInbox(AgentId("term_p"))
            let jid = await start(k, "term_p", py)
            _ = await outputUntil(inbox) { $0.contains("SIZE") }
            // The mime is the caller's to state — never guessed.
            let missing = await k.send([
                Item(
                    AgentId("term_p"), .object(["type": "paste_image", "job_id": jid]),
                    binary: Blob("data", Data([0x89, 0x50])))
            ])[0].result
            #expect(
                missing["reason"] == "missing_arg" && missing["arg"] == "mime",
                "\(missing.serialize())")
            let r = await k.send([
                Item(
                    AgentId("term_p"),
                    .object(["type": "paste_image", "job_id": jid, "mime": "image/png"]),
                    binary: Blob("data", Data([0x89, 0x50, 0x4E, 0x47, 0x2E, 0x2E, 0x2E, 0x2E])))
            ])[0].result
            #expect(r["bytes"] == 8, "\(r.serialize())")
            let path = r["path"].asString ?? ""
            #expect(path.hasSuffix(".png"), "\(r.serialize())")
            _ = await call(k, "term_p", ["type": "write", "job_id": jid, "data": "\n"])
            let text = await outputUntil(inbox) { $0.contains("GOT") && $0.contains(".png") }
            #expect(text.contains(path), "\(text)")
        }

        /// A shell puts each job it starts in its OWN process group inside the PTY
        /// session, so killing the leader alone (and the tty's hangup, which reaches
        /// the foreground group only) would leave `sleep &` and a `nohup` running,
        /// reparented to launchd. `stop` and `delete_agent` (the kernel's shutdown
        /// runs the same teardown) kill the whole session. Twin of python
        /// `test_every_teardown_kills_the_whole_session`.
        @Test func everyTeardownKillsTheWholeSession() async {
            for (aid, end) in [("sh_stop", "stop"), ("sh_delete", "delete_agent")] {
                let k = await kernelWith(
                    aid, ["allowed": ["/bin/sh -i"], "env": ["PATH": "/usr/bin:/bin"]])
                let inbox = k.ensureInbox(AgentId(aid))
                let r = await call(
                    k, aid,
                    [
                        "type": "start", "command": "/bin/sh", "args": ["-i"],
                        "pty": ["cols": 80, "rows": 24],
                    ])
                let jid = r["job_id"]
                _ = await call(
                    k, aid,
                    [
                        "type": "write", "job_id": jid,
                        "data":
                            "echo PID=$$\nsleep 300 &\nnohup sleep 301 >/dev/null 2>&1 &\nsleep 302\n",
                    ])
                let text = await outputUntil(inbox) {
                    $0.range(of: #"PID=\d+"#, options: .regularExpression) != nil
                }
                guard let m = text.range(of: #"PID=\d+"#, options: .regularExpression),
                    let leader = pid_t(text[m].dropFirst(4))
                else {
                    Issue.record("no shell pid in \(text)")
                    return
                }
                var members: [pid_t] = []
                for _ in 0..<100 {
                    members = Spawn.allPids().filter { $0 != leader && getsid($0) == leader }
                    if members.count >= 3 { break }
                    try? await Task.sleep(nanoseconds: 50_000_000)
                }
                #expect(members.count >= 3, "\(end): \(members)")
                if end == "stop" {
                    _ = await call(k, aid, ["type": "stop", "job_id": jid])
                } else {
                    _ = await call(k, "kernel_state", ["type": "delete_agent", "id": .string(aid)])
                }
                var alive = [leader] + members
                for _ in 0..<100 {
                    alive = alive.filter { kill($0, 0) == 0 }
                    if alive.isEmpty { break }
                    try? await Task.sleep(nanoseconds: 50_000_000)
                }
                #expect(alive.isEmpty, "\(end): survivors \(alive)")
            }
        }

        /// A plain (pipe) job leads its own session too, so `stop` and
        /// `delete_agent` take what it launched — a script's `sleep &` and a
        /// `nohup` never outlive it. Twin of python
        /// `test_stop_and_delete_kill_everything_a_job_launched`.
        @Test func stopAndDeleteKillEverythingAJobLaunched() async {
            let line = "echo PID=$$; sleep 303 & nohup sleep 304 >/dev/null 2>&1 & sleep 305"
            for (aid, end) in [("pipe_stop", "stop"), ("pipe_delete", "delete_agent")] {
                let k = await kernelWith(
                    aid, ["allowed": ["/bin/sh -c .*"], "env": ["PATH": "/usr/bin:/bin"]])
                let inbox = k.ensureInbox(AgentId(aid))
                let r = await call(
                    k, aid, ["type": "start", "command": "/bin/sh", "args": ["-c", .string(line)]])
                let jid = r["job_id"]
                var leader: pid_t?
                let deadline = Date().addingTimeInterval(15)
                while leader == nil && Date() < deadline {
                    if let item = inbox.tryTake(), item.payload["type"].asString == "progress",
                        let l = item.payload["line"].asString, l.hasPrefix("PID=")
                    {
                        leader = pid_t(l.dropFirst(4))
                    } else {
                        try? await Task.sleep(nanoseconds: 20_000_000)
                    }
                }
                guard let leader else {
                    Issue.record("\(end): no PID line")
                    return
                }
                var members: [pid_t] = []
                for _ in 0..<100 {
                    members = Spawn.allPids().filter { $0 != leader && getsid($0) == leader }
                    if members.count >= 3 { break }
                    try? await Task.sleep(nanoseconds: 50_000_000)
                }
                #expect(members.count >= 3, "\(end): \(members)")
                if end == "stop" {
                    _ = await call(k, aid, ["type": "stop", "job_id": jid])
                } else {
                    _ = await call(k, "kernel_state", ["type": "delete_agent", "id": .string(aid)])
                }
                var alive = [leader] + members
                for _ in 0..<100 {
                    alive = alive.filter { kill($0, 0) == 0 }
                    if alive.isEmpty { break }
                    try? await Task.sleep(nanoseconds: 50_000_000)
                }
                #expect(alive.isEmpty, "\(end): survivors \(alive)")
            }
        }

        @Test func resizeReachesTheProgramAndStatusReportsIt() async {
            guard let py = py3() else { return }
            let k = await kernelWith("term_r")
            let inbox = k.ensureInbox(AgentId("term_r"))
            let jid = await start(k, "term_r", py)
            _ = await outputUntil(inbox) { $0.contains("SIZE") }
            let r = await call(
                k, "term_r", ["type": "resize", "job_id": jid, "cols": 120, "rows": 40])
            #expect(r == .object(["resized": true, "cols": 120, "rows": 40]), "\(r.serialize())")
            let w = await outputUntil(inbox) { $0.contains("WINCH 120 40") }
            #expect(w.contains("WINCH 120 40"), "\(w)")
            let snap = await call(k, "term_r", ["type": "status", "job_id": jid])
            let s = snap["jobs"][0]
            #expect(s["pty"] == true && s["cols"] == 120 && s["rows"] == 40, "\(snap.serialize())")
            #expect(s["stdout"].isNull, "a PTY job has no stdout/stderr split")
            _ = await call(k, "term_r", ["type": "write", "job_id": jid, "data": "x\n"])
        }

        @Test func outputReplaysTheRing() async {
            guard let py = py3() else { return }
            let k = await kernelWith("term_o")
            let inbox = k.ensureInbox(AgentId("term_o"))
            let jid = await start(k, "term_o", py)
            _ = await outputUntil(inbox) { $0.contains("SIZE") }
            let ring =
                await call(k, "term_o", ["type": "output", "job_id": jid])["output"]
                .asString ?? ""
            #expect(ring.contains("TTY True") && ring.contains("SIZE 91 37"), "\(ring)")
            let tail =
                await call(
                    k, "term_o", ["type": "output", "job_id": jid, "max_bytes": 8])[
                    "output"
                ]
                .asString ?? ""
            #expect(tail.utf8.count <= 8 && ring.hasSuffix(tail), "\(tail)")
            _ = await call(k, "term_o", ["type": "write", "job_id": jid, "data": "x\n"])
        }

        @Test func aPtyJobIsGatedAndItsVerbsWantAPtyJob() async {
            guard let py = py3() else { return }
            let k = await kernelWith("term_g", ["allowed": .array([.string("\(py) -c .*")])])
            let r = await call(
                k, "term_g",
                [
                    "type": "start", "command": "/bin/sh", "args": ["-i"],
                    "pty": ["cols": 80, "rows": 24],
                ])
            #expect(r["reason"] == "unauthorized", "\(r.serialize())")
            let bad = await call(
                k, "term_g",
                ["type": "start", "command": .string(py), "args": ["-c", "0"], "pty": ["cols": 0]])
            #expect(bad["reason"] == "bad_arg", "\(bad.serialize())")
            let nf = await call(k, "term_g", ["type": "write", "job_id": 999999, "data": "x"])
            #expect(nf["reason"] == "not_found", "\(nf.serialize())")
            let pipe = await call(
                k, "term_g", ["type": "start", "command": .string(py), "args": ["-c", "input()"]])
            let w = await call(
                k, "term_g", ["type": "write", "job_id": pipe["job_id"], "data": "x"])
            #expect(w["reason"] == "bad_arg" && w["arg"] == "job_id", "\(w.serialize())")
            _ = await call(k, "term_g", ["type": "stop", "job_id": pipe["job_id"]])
        }

        /// The kernel's environment reaches a child only by NAME (`inherit_env`).
        @Test func theKernelEnvReachesAChildOnlyByName() async {
            setenv("FT_SECRET_SW", "s3cret", 1)
            setenv("FT_OPEN_SW", "open", 1)
            for (id, inherit, want) in [
                ("env_bare", JSON.null, "none/none"),
                ("env_named", JSON.array([.string("FT_OPEN_SW")]), "none/open"),
                ("env_all", JSON.array([.string("*")]), "s3cret/open"),
            ] {
                let k = await kernelWith(id, ["inherit_env": inherit])
                let inbox = k.ensureInbox(AgentId(id))
                _ = await call(
                    k, id,
                    [
                        "type": "start", "command": "/bin/sh",
                        "args": ["-c", "echo ${FT_SECRET_SW:-none}/${FT_OPEN_SW:-none}"],
                    ])
                let done = await nextOf(inbox, "job_done")
                #expect(
                    await printed(k, AgentId(id), done) == "\(want)\n", "\(id): \(done?.serialize() ?? "nil")")
            }
        }

        /// A per-call `env`/`cwd` is refused, and a leg with no `cwd` runs nothing.
        @Test func whereAndWithWhatARunIsTheRecords() async {
            let k = await kernelWith("rec_only")
            for (arg, val) in [
                ("env", JSON.object(["LD_PRELOAD": "/tmp/x"])), ("cwd", JSON.string("/")),
            ] {
                let r = await call(
                    k, "rec_only",
                    ["type": "start", "command": "/bin/sh", "args": ["-c", "true"], arg: val])
                #expect(r["reason"] == "bad_arg" && r["arg"] == .string(arg), "\(r.serialize())")
            }
            let k2 = await kernelWith("no_cwd", ["cwd": .null])
            let r = await call(
                k2, "no_cwd", ["type": "start", "command": "/bin/sh", "args": ["-c", "true"]])
            #expect(r["reason"] == "missing_arg" && r["arg"] == "cwd", "\(r.serialize())")
        }

        /// A viewer that streamed but never acked (a reloaded page) leaves `unacked`
        /// past HIGH and the reader paused. The new viewer's `output` replay hands it
        /// everything so far, so it zeroes the count and resumes the reader — python
        /// `test_a_replay_clears_the_backlog_a_gone_viewer_left`.
        @Test func aReplayClearsTheBacklogAGoneViewerLeft() async {
            guard let py = py3() else { return }
            let k = await kernelWith("term_f")
            let inbox = k.ensureInbox(AgentId("term_f"))
            let r = await call(
                k, "term_f",
                [
                    "type": "start", "command": .string(py), "args": ["-c", .string(FLOOD)],
                    "pty": ["cols": 80, "rows": 24],
                ])
            let jid = r["job_id"]
            let probe: OrderedDictionary<String, JSON> = ["type": "ack", "job_id": jid, "chars": 0]
            var st = JSON.object([:])
            for _ in 0..<250 {
                st = await call(k, "term_f", probe)
                if st["paused"] == true { break }
                try? await Task.sleep(nanoseconds: 20_000_000)
            }
            #expect(
                st["paused"] == true && (st["unacked"].asInt ?? 0) > 100_000, "\(st.serialize())")
            _ = await call(k, "term_f", ["type": "output", "job_id": jid])
            st = await call(k, "term_f", probe)
            // the count restarted at 0; the resumed reader may already have counted more
            #expect(
                st["paused"] == false && (st["unacked"].asInt ?? .max) < 100_000,
                "\(st.serialize())")
            // the rest of the flood (under HIGH) arrives with no further ack
            let text = await outputUntil(inbox) { $0.contains("DONE") }
            #expect(text.contains("DONE"))
            _ = await call(k, "term_f", ["type": "write", "job_id": jid, "data": "\n"])
        }
    }
#endif
