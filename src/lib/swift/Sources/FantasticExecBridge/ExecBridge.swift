// Process edge of the io family — swift mirror of the canonical python
// `io/exec_bridge` (canonical; on disagreement this file is wrong). Rust's
// `fantastic-exec-bridge` is the second reference.
//
// Crossing kernel↔subprocess is gated like any io_bridge leg: SEALED BY DEFAULT
// (absent `ingress_rule` ⇒ every verb except `reflect` denies), and bound by a
// regex command ALLOWLIST — `allowed` on the record is a list of regexes, and
// `start {command, args}` refuses unless the shell-joined line fullmatches one.
// No per-call override, no default, no PATH fallback; an empty `allowed` runs
// nothing. The withdrawn `code`/`shim`/`interpreter`/`interpreters`/
// `argv_template` fields are refused at WRITE time (`validateMeta`) and at
// `start`.
//
// It is a GENERAL-PURPOSE COMMAND runner, NOT a code runner. A program is a FILE
// some process (or a `write_stream`) put on disk; running it is `command` +
// `args`, exactly like anything else — so the job table / output pump / connector
// relay / fd passing / task-table rows / mail contract are all language-blind.
//
// The bridge publishes a WIRE — the inherited `FANTASTIC_CTRL_FD`, framed
// `[4-byte BE length | kind | rest]` (T = JSON, B = raw bytes) — and nothing else. A
// program that wants a `kernel`
// global includes the reference wire client (`CONNECTOR_SRC`) in its own source;
// the bridge prepends nothing. `start` spawns in the BACKGROUND and returns
// `{job_id, status:"running"}` at once, streaming `progress`/`job_done`; `boot`
// is a no-op (nothing auto-restarts — a command is started by a caller, not the
// record). Any verb this bundle does not own is MAIL for the running program
// (`{queued:true}`); jobs are kernel TASKS (`ps`/`cancel`, `onCancel` SIGKILLs
// the real process).
//
// NOT a sandbox. An allowlisted command is a NAMED command, with all of that
// command's reach — a python it permits still has `open()`, `socket`,
// `subprocess`. The allowlist buys exactly one property: the OPERATOR chose WHICH
// commands may run, and a program cannot pick a different one. Anything stronger
// needs an OS mechanism (`sandbox-exec`, a separate uid, a container) and is
// DEFERRED. The iOS sandbox forbids subprocess, so this is macOS-only.
//
#if os(macOS)

    import FantasticIoBridge
    import FantasticJSON
    import FantasticKernel
    import Foundation
    import OrderedCollections

    public let HANDLER_MODULE = "exec_bridge.tools"

    // ── the command gate: a REGEX allowlist (py `_command_permitted`) ──

    /// This leg's `allowed` — regexes a command line must fullmatch to run. A bare
    /// string is a one-element list; empty ⇒ the leg runs nothing (a legal state).
    func allowedPatterns(_ record: Agent?) -> [String] {
        switch record?.metaValue(forKey: "allowed") {
        case .some(.string(let s)): return [s]
        case .some(.array(let a)): return a.map { $0.asString ?? $0.serialize() }
        default: return []
        }
    }

    /// `nil` when `line` fullmatches an allowed pattern; else the refusal text.
    ///
    /// The FULL command line (command + args, shell-joined) is matched ANCHORED —
    /// the equivalent of python's `re.fullmatch(pat, line, re.DOTALL)`: the match
    /// must span the WHOLE string (so `echo.*` cannot smuggle `echo; rm -rf /` past
    /// a prefix) and `.` spans newlines (an argv element may contain them). An empty
    /// allowlist runs nothing.
    func commandPermitted(_ record: Agent?, _ line: String) -> String? {
        let pats = allowedPatterns(record)
        if pats.isEmpty {
            return
                "exec_bridge: no commands are allowed on this leg — an operator must "
                + "set `allowed=[<regex>, ...]` naming what may run"
        }
        let full = NSRange(line.startIndex..<line.endIndex, in: line)
        for pat in pats {
            guard
                let re = try? NSRegularExpression(
                    pattern: pat, options: [.dotMatchesLineSeparators])
            else { continue }
            if let m = re.firstMatch(in: line, options: [], range: full), m.range == full {
                return nil
            }
        }
        return "exec_bridge: command not allowed: \(line.debugDescription) — this leg permits: \(pats)"
    }

    /// Shell-join argv the way python's `shlex.join` does — POSIX quoting, so the
    /// joined line the allowlist matches is the same one a shell would see.
    func shlexJoin(_ argv: [String]) -> String {
        argv.map(shlexQuote).joined(separator: " ")
    }

    /// One token, quoted per python's `shlex.quote`. Safe set is `[\w@%+=:,./-]`;
    /// anything else forces single-quoting with inner `'` escaped as `'"'"'`.
    func shlexQuote(_ s: String) -> String {
        if s.isEmpty { return "''" }
        let safe = s.utf8.allSatisfy { b in
            (b >= 48 && b <= 57) || (b >= 65 && b <= 90) || (b >= 97 && b <= 122)
                || b == 95 || b == 64 || b == 37 || b == 43 || b == 61 || b == 58
                || b == 44 || b == 46 || b == 47 || b == 45
        }
        if safe { return s }
        return "'" + s.replacingOccurrences(of: "'", with: "'\"'\"'") + "'"
    }

    public final class ExecBridgeBundle: AgentBundle, @unchecked Sendable {
        public let name = "exec_bridge"
        public init() {}

        // MARK: - the job table

        /// One in-flight (or finished) program — the REAL resource this bundle owns.
        ///
        /// Keyed by its public `jobId`; every verb filters by `agent` (owner), so a
        /// job_id from another agent is still "no job". `taskId` is the row in the
        /// kernel task table the pump holds; `onCancel` matches on it, so killing one
        /// job never touches another running on the same agent. The invocation
        /// (`command`/`args`/`cwd`) is recorded so `status` can report WHAT ran.
        private struct Job {
            let agent: AgentId
            var taskId: TaskId?
            let process: Process
            let pid: Int32
            var relay: Relay?
            let command: String
            let args: [String]
            let cwd: String?
            let startedAt: Double
            var status: String = "running"
            var exitCode: Int32? = nil
            var lines: Int = 0
            var lastLine: String = ""
            var stdout: String = ""
            var stderr: String = ""
        }

        /// Guards `jobs`. Never held across an `await`.
        private let lock = NSLock()
        /// Live/finished children, keyed by job id. Per-INSTANCE (one bundle per
        /// kernel), so two kernels in one process cannot see each other's children.
        private var jobs: [String: Job] = [:]

        public var readme: String? {
            """
            exec_bridge — the process edge of the io family, a general-purpose \
            COMMAND runner. Sealed by default (`ingress_rule=allow_all_ingress` \
            opens it); the LAW is the regex ALLOWLIST (`allowed=[<regex>, ...]` on \
            the record — no default, no PATH fallback; `start {command, args}` \
            refuses unless the shell-joined line fullmatches one). Spawns in the \
            BACKGROUND → {job_id, status:"running"} at once, streaming progress and \
            a final job_done. NOT a code runner: there is no `code`, no \
            `interpreter`, no shim — a program is a FILE some process put on disk, \
            run as `command` + `args` like anything else. The bridge publishes a \
            WIRE: the inherited FANTASTIC_CTRL_FD, framed [4-byte BE length | kind | rest] \
            (T = JSON, B = raw bytes). \
            A program that wants a `kernel` global includes the reference wire \
            client in its own source; the bridge prepends nothing — send this agent \
            {type:'connector'} and paste the reply's `source` verbatim at the top of \
            your program. A program's sends carry THIS runner's identity and grants \
            (its can_call; a target must list the runner in callable_by). Any verb this \
            bundle does not own is MAIL for the running program ({queued:true}); \
            jobs are kernel TASKS (ps/cancel reap the real OS process). Not a \
            sandbox: an allowlisted command is a NAMED command with all of that \
            command's reach.
            """
        }

        // MARK: - dispatch

        public func handle(
            agentId: AgentId,
            payload: JSON,
            kernel: Kernel
        ) async throws -> JSON? {
            let verb = payload["type"].asString ?? ""
            // AUTH GATE — the process edge is an io_bridge leg: SEALED by default,
            // kind "spawn". `reflect` is UNGATED (discovery works sealed); mail is
            // ungated too (py parity — the fanout already delivered).
            let owned = ["start", "boot", "shutdown", "status", "clear", "interrupt", "stop"]
            if owned.contains(verb), let agent = kernel.agent(agentId) {
                let rule = try resolveIngress(
                    ingressRule: agent.metaValue(forKey: "ingress_rule"),
                    auth: agent.metaValue(forKey: "auth"))
                let action = AuthAction(
                    kind: "spawn", target: agentId.value, verb: verb,
                    token: agent.metaValue(forKey: "auth_token")?.asString)
                // FORWARDED, not re-worded — the rule states the reason and the
                // repair; this hint was a second, shorter wording of the same
                // thing, so the same seal read differently per edge.
                if case .deny(let d) = rule.authorize(action) {
                    return d.asRefusal
                }
            }
            switch verb {
            case "reflect":
                return reflectReply(agentId, kernel: kernel)
            case "start":
                return await startReply(agentId: agentId, payload: payload, kernel: kernel)
            case "boot":
                return .null
            case "shutdown":
                return .null
            case "status":
                return statusReply(agentId, payload: payload)
            case "clear":
                return clearReply(agentId, payload: payload)
            case "connector":
                // The reference wire client SOURCE — a model pastes it at the top of
                // its program instead of re-deriving the socket framing (measured
                // 2026-09-01, E30 subai: ten turns lost before the first working
                // frame). Ungated like reflect.
                return .object([
                    "source": .string(CONNECTOR_SRC),
                    "howto": .string(
                        "Write `source` verbatim as the first lines of your program file, then "
                            + "your code below it. It opens FANTASTIC_CTRL_FD (a socket, length-prefixed "
                            + "frames; a payload carrying a bytes value rides the binary channel raw: "
                            + "`write_stream` puts its chunk in `bytes`, `read_stream` passes `bytes: b''` "
                            + "and gets its chunk back raw in the reply's `bytes`) and "
                            + "exposes `kernel.send(target, payload) -> reply` (raises if the relay could "
                            + "not deliver), "
                            + "`kernel.emit(payload)`, `kernel.watch(src, cb)`, `kernel.on_message(cb)`, "
                            + "`kernel.self_id`. Arguments arrive as ARGV, like any command — there is "
                            + "no `kernel.args`. One frame caps at 16 MiB; move more than that with "
                            + "`write_stream` chunks. Your sends carry THIS runner's identity "
                            + "and grants (its `can_call`; a target must list this runner in "
                            + "`callable_by`). Do not rewrite the wire by hand."),
                ])
            case "interrupt":
                if let gone = noSuchJob(agentId, payload) { return gone }
                return .object(["interrupted": .integer(Int64(signalJobs(agentId, payload, SIGINT)))])
            case "stop":
                if let gone = noSuchJob(agentId, payload) { return gone }
                return .object(["killed": .integer(Int64(signalJobs(agentId, payload, SIGKILL)))])
            default:
                // ADDRESSABLE INBOX. A verb this bundle does not own is MAIL for the
                // running program — the substrate's fanout has already delivered it,
                // so this only ACKs. Re-emitting would double-deliver.
                if hasRunningJob(agentId) {
                    return .object(["queued": .bool(true)])
                }
                return .object([
                    "error": .string(
                        "exec_bridge: unknown type \"\(verb)\" (no running job to receive it)")
                ])
            }
        }

        /// Any live process of this owner.
        private func hasRunningJob(_ agentId: AgentId) -> Bool {
            lock.withLock {
                jobs.values.contains { $0.agent == agentId && $0.process.isRunning }
            }
        }

        /// `start` — spawn the COMMAND in the BACKGROUND and return at once.
        ///
        /// args: command:str (req), args:list[str]? (argv, default []), env:dict?
        /// (extra env vars), cwd:str?. The FULL line (command + args, shell-joined)
        /// must fullmatch one of the record's `allowed` regexes or the start is
        /// refused naming the patterns. A `code` field is refused explicitly.
        private func startReply(agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
            let record = kernel.agent(agentId)
            // EVERY refusal on this verb carries a CODE. Sibling argument errors used
            // to split — `args`/`env` answered a typed message while `code`/`command`/
            // the policy wall answered a bare `{error}` with no `reason` at all — so
            // nothing could tell a typo from a policy refusal without matching prose.
            if !payload["code"].isNull {
                return Refusal.bad("start", "code", "absent — run a command instead")
            }
            guard let command = payload["command"].asString,
                !command.trimmingCharacters(in: .whitespaces).isEmpty
            else {
                return Refusal.missing("start", "command")
            }
            // `args` — a list of scalars (str/int/float), each stringified into argv.
            var args: [String] = []
            let rawArgs = payload["args"]
            if !rawArgs.isNull {
                guard let arr = rawArgs.asArray else {
                    return Refusal.bad("start", "args", "a list of strings")
                }
                for v in arr {
                    switch v {
                    case .string(let s): args.append(s)
                    case .integer(let i): args.append(String(i))
                    case .double(let d): args.append(numString(d))
                    default:
                        return Refusal.bad("start", "args", "a list of strings")
                    }
                }
            }
            // The FULL argv, shell-joined, matched against the allowlist BEFORE spawn.
            let argv = [command] + args
            let line = shlexJoin(argv)
            if let why = commandPermitted(record, line) {
                // An AUTHORIZATION denial, not a bad argument: the command line is
                // well-formed and the LEG refused it. Same code the ingress gate
                // answers, so one branch covers "the operator has not opened this".
                return Refusal.make(
                    Refusal.unauthorized, why,
                    hint: "the leg's `allowed` policy decides; an operator may widen it")
            }
            // The per-call `env` block — a dict of str -> scalar.
            var envExtra: [String: String] = [:]
            let rawEnv = payload["env"]
            if !rawEnv.isNull {
                guard let m = rawEnv.asObject else {
                    return Refusal.bad("start", "env", "a dict of str -> str")
                }
                for (k, v) in m {
                    switch v {
                    case .string(let s): envExtra[k] = s
                    case .integer(let i): envExtra[k] = String(i)
                    case .double(let d): envExtra[k] = numString(d)
                    default:
                        return Refusal.bad("start", "env", "a dict of str -> str")
                    }
                }
            }
            let cwd = payload["cwd"].asString ?? record?.metaValue(forKey: "cwd")?.asString

            let spawned: Spawn.Spawned
            do {
                spawned = try Spawn.job(
                    argv: argv, selfId: agentId.value, envExtra: envExtra, cwd: cwd)
            } catch {
                return Refusal.make(
                    Refusal.transportError, "exec_bridge: spawn failed: \(error)")
            }
            let jobId = String(UInt32.random(in: .min ... .max), radix: 16)
            let relay = Relay(
                fd: spawned.ctrlFd, owner: agentId, jobId: jobId, kernel: kernel)
            relay.start()

            let pid = spawned.process.processIdentifier
            // The job OUTLIVES this dispatch, so it needs its OWN task-table row —
            // otherwise a subprocess nobody can reach is the orphan bug.
            let taskId = kernel.spawnTask(agent: agentId, verb: "start") { [weak self] in
                await self?.pump(
                    agentId: agentId, jobId: jobId, spawned: spawned, kernel: kernel)
            }
            lock.withLock {
                jobs[jobId] = Job(
                    agent: agentId, taskId: taskId, process: spawned.process, pid: pid,
                    relay: relay, command: command, args: args, cwd: cwd,
                    startedAt: Date().timeIntervalSince1970)
            }
            await kernel.emit(
                agentId, .object(["type": .string("job_started"), "job_id": .string(jobId)]))
            return .object(["job_id": .string(jobId), "status": .string("running")])
        }

        /// The job's task-table body: drain both pipes LIVE (a `progress` per line as
        /// it arrives), await exit, update the row, emit `job_done`. The row STAYS
        /// after exit so `status`/`clear` can see it (python/rust parity).
        private func pump(
            agentId: AgentId, jobId: String, spawned: Spawn.Spawned, kernel: Kernel
        ) async {
            async let outD = pumpStream(
                spawned.stdout.fileHandleForReading, stream: "stdout",
                agentId: agentId, jobId: jobId, kernel: kernel)
            async let errD = pumpStream(
                spawned.stderr.fileHandleForReading, stream: "stderr",
                agentId: agentId, jobId: jobId, kernel: kernel)
            let o = await outD
            let e = await errD

            let exit = ExitBox()
            spawned.process.terminationHandler = { p in exit.finish(p.terminationStatus) }
            if !spawned.process.isRunning { exit.finish(spawned.process.terminationStatus) }
            let code = await exit.wait()
            relayFor(jobId)?.stop()
            lock.withLock {
                if var j = jobs[jobId] {
                    j.exitCode = code
                    j.stdout = o
                    j.stderr = e
                    j.status =
                        j.status == "stopping"
                        ? "stopped" : (code == 0 ? "done" : "failed")
                    jobs[jobId] = j
                }
            }
            await kernel.emit(
                agentId,
                .object([
                    "type": .string("job_done"), "job_id": .string(jobId),
                    "exit_code": .integer(Int64(code)), "ok": .bool(code == 0),
                    "stdout": .string(o), "stderr": .string(e),
                ]))
        }

        /// Emit one `progress` per output line; the counter is SHARED across stdout
        /// and stderr (it lives on the job row), so `n` is one monotonic sequence.
        private func emitLine(
            _ line: String, stream: String, agentId: AgentId, jobId: String, kernel: Kernel
        ) async {
            let n = lock.withLock { () -> Int in
                if var j = jobs[jobId] {
                    j.lines += 1
                    j.lastLine = line
                    jobs[jobId] = j
                    return j.lines
                }
                return 0
            }
            await kernel.emit(
                agentId,
                .object([
                    "type": .string("progress"), "job_id": .string(jobId),
                    "n": .integer(Int64(n)), "line": .string(line),
                    "stream": .string(stream),
                ]))
        }

        // MARK: - status / clear

        private func snap(_ jobId: String, _ j: Job, full: Bool) -> JSON {
            var o: OrderedDictionary<String, JSON> = [
                "job_id": .string(jobId),
                "task_id": j.taskId.map { JSON.string($0.value) } ?? .null,
                "status": .string(j.status),
                "exit_code": j.exitCode.map { JSON.integer(Int64($0)) } ?? .null,
                "started_at": .double(j.startedAt),
                "lines": .integer(Int64(j.lines)),
                "last_line": .string(j.lastLine),
                "command": .string(j.command),
                "args": .array(j.args.map(JSON.string)),
            ]
            if full {
                o["stdout"] = .string(j.stdout)
                o["stderr"] = .string(j.stderr)
                o["cwd"] = j.cwd.map { JSON.string($0) } ?? .null
            }
            return .object(o)
        }

        /// `status` — one job (full output) or all of THIS agent's jobs (summary).
        private func statusReply(_ agentId: AgentId, payload: JSON) -> JSON {
            lock.withLock {
                if let jid = payload["job_id"].asString {
                    guard let j = jobs[jid], j.agent == agentId else { return noJob(jid) }
                    return .object(["jobs": .array([snap(jid, j, full: true)])])
                }
                let rows = jobs.filter { $0.value.agent == agentId }
                    .map { snap($0.key, $0.value, full: false) }
                return .object(["jobs": .array(rows)])
            }
        }

        /// `clear` — drop finished rows of this agent (one, or all non-running).
        private func clearReply(_ agentId: AgentId, payload: JSON) -> JSON {
            let want = payload["job_id"].asString
            let doomed: [String] = lock.withLock {
                jobs.filter { (id, j) in
                    j.agent == agentId
                        && (want.map { $0 == id } ?? (j.status != "running" && j.status != "stopping"))
                }.map { $0.key }
            }
            lock.withLock { for id in doomed { jobs.removeValue(forKey: id) } }
            return .object(["cleared": .integer(Int64(doomed.count))])
        }

        /// The `not_found` refusal for a job_id naming nothing. Spelled once.
        private func noJob(_ jid: String) -> JSON {
            Refusal.make(
                Refusal.notFound, "exec_bridge: no job \"\(jid)\"",
                extra: [("job_id", .string(jid))])
        }

        /// …and the guard that decides. A `job_id` naming nothing used to answer
        /// `{killed:0}` / `{interrupted:0}` — the same reply as "it had already
        /// exited", so a typo read as success.
        private func noSuchJob(_ agentId: AgentId, _ payload: JSON) -> JSON? {
            guard let jid = payload["job_id"].asString else { return nil }
            let known = lock.withLock { jobs[jid]?.agent == agentId }
            return known ? nil : noJob(jid)
        }

        /// `interrupt` / `stop` — signal this agent's jobs, or one named `job_id`.
        /// Returns how many were actually signalled.
        private func signalJobs(_ agentId: AgentId, _ payload: JSON, _ sig: Int32) -> Int {
            let want = payload["job_id"].asString
            return lock.withLock {
                var n = 0
                for (id, var j) in jobs {
                    if j.agent != agentId || (want.map { $0 != id } ?? false) { continue }
                    if !j.process.isRunning { continue }  // already gone — never pretend
                    if sig == SIGKILL {
                        j.status = "stopping"
                        jobs[id] = j
                    }
                    if kill(j.pid, sig) == 0 { n += 1 }
                }
                return n
            }
        }

        // MARK: - lifecycle

        /// The relay serving a given job, if it is still registered.
        private func relayFor(_ jobId: String) -> Relay? {
            lock.withLock { jobs[jobId]?.relay }
        }

        /// The agent is going away — SIGKILL every child it still owns.
        public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
            teardownAgent(agentId)
        }

        public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
            teardownAgent(agentId)
        }

        /// THE CANCEL HOOK — the SIGTERM half. The kernel is about to abort
        /// `taskId`'s Swift `Task`, which frees no OS resource: the child of that
        /// dispatch would keep running, orphaned. We own the process, so we kill it,
        /// and the substrate awaits us BEFORE it aborts anything. TASK-scoped: a
        /// second job on the same agent is a different task and survives.
        public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
            let doomed = lock.withLock {
                jobs.filter { $0.value.agent == agentId && $0.value.taskId == taskId }
                    .map(\.key)
            }
            for id in doomed { teardown(id) }
        }

        /// Read one pipe to EOF, emitting a `progress` PER LINE as it arrives, and
        /// return the whole text for the final `job_done`/`status` snapshot.
        ///
        /// This used to buffer both pipes to EOF and emit every line in one burst at
        /// exit, which made `progress` useless for exactly the jobs that need it: a
        /// server-style program that prints a line per request and never exits
        /// produced NO output events at all, and `status` reported `lines: 0` for its
        /// whole life. `readabilityHandler` fires on Foundation's own queue (no thread
        /// pinned); it splits on '\n' and carries the partial line, and the carry is
        /// flushed at EOF so an unterminated last line is not lost.
        private func pumpStream(
            _ handle: FileHandle, stream: String, agentId: AgentId, jobId: String,
            kernel: Kernel
        ) async -> String {
            let box = DataBox()
            let carry = CarryBox()
            let (lines, cont) = AsyncStream<String>.makeStream()
            handle.readabilityHandler = { h in
                let chunk = h.availableData
                if chunk.isEmpty {
                    h.readabilityHandler = nil
                    if let tail = carry.flush() { cont.yield(tail) }
                    cont.finish()
                    return
                }
                box.append(chunk)
                for line in carry.push(chunk) { cont.yield(line) }
            }
            for await line in lines {
                await emitLine(
                    line, stream: stream, agentId: agentId, jobId: jobId, kernel: kernel)
            }
            return String(data: box.take(), encoding: .utf8) ?? ""
        }

        /// Free the REAL resource of ONE job: SIGKILL its child + stop its relay, and
        /// LABEL the row `stopping` so the pump finalises it to `stopped`.
        ///
        /// The label is not cosmetic. Without it a cancelled job finalised as
        /// `failed` — the SIGKILL's non-zero status, indistinguishable from a program
        /// that failed on its own — where python and rust both report `stopped`.
        /// Idempotent; never signals a pid it no longer owns.
        private func teardown(_ jobId: String) {
            let job = lock.withLock { () -> Job? in
                guard var j = jobs[jobId] else { return nil }
                if j.exitCode == nil {
                    j.status = "stopping"
                    jobs[jobId] = j
                }
                return j
            }
            guard let job else { return }
            if job.process.isRunning { kill(job.pid, SIGKILL) }
            job.relay?.stop()
        }

        private func teardownAgent(_ agentId: AgentId) {
            let doomed = lock.withLock {
                jobs.filter { $0.value.agent == agentId }.map(\.key)
            }
            for id in doomed { teardown(id) }
        }

        // MARK: - validate_meta

        /// Refuse a write this bundle cannot honour, at the WRITE (py `validate_meta`).
        public func validateMeta(
            _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
        ) -> String? {
            if let spec = meta["ingress_rule"],
                let rule = try? IngressRules.resolve(spec),
                let deny = rule as? IngressRules.DenyInbound, let bad = deny.unknown
            {
                return "ingress_rule=\"\(bad)\" is not a rule this bundle implements — "
                    + "legal values: allow_all_ingress, deny_ingress"
            }
            // `password_ingress` is meaningless on an in-process leg: the token is
            // read off a WIRE ENVELOPE and the process edge has none — no frame ever
            // presents one, so the rule could only refuse, or (until 2026-09-02) be
            // satisfied by a secret persisted ON THE RECORD. Refused at the write
            // (#116). Same rule as `FantasticFile.passwordRefusal`.
            for (field, stem) in [("ingress_rule", "password_ingress"), ("auth", "password")] {
                if let spec = meta[field], ruleName(spec, default: "") == stem {
                    return "\(field)=\(spec.serialize()) is not applicable to the process edge: "
                        + "a leg with no wire envelope can never present a token — "
                        + "legal values: allow_all_ingress, deny_ingress"
                }
            }
            // The withdrawn code-runner fields are refused at WRITE time.
            for gone in ["code", "shim", "interpreter", "interpreters", "argv_template"] {
                if let v = meta[gone], !v.isNull {
                    return
                        "exec_bridge: `\(gone)` is not a field — this is a general-purpose "
                        + "COMMAND runner; gate it with `allowed=[<regex>, ...]`"
                }
            }
            // `allowed` must be a non-empty list of compilable regexes.
            if let raw = meta["allowed"], !raw.isNull {
                let pats: [String]
                switch raw {
                case .string(let s): pats = [s]
                case .array(let a): pats = a.map { $0.asString ?? $0.serialize() }
                default: pats = []
                }
                if pats.isEmpty {
                    return "allowed must be a non-empty list of regexes, e.g. allowed=['ls( .*)?']"
                }
                for pat in pats {
                    if (try? NSRegularExpression(pattern: pat)) == nil {
                        return "allowed contains a broken regex \"\(pat)\""
                    }
                }
            }
            return nil
        }

        // MARK: - reflect

        private func reflectReply(_ agentId: AgentId, kernel: Kernel) -> JSON {
            let record = kernel.agent(agentId)
            let running = lock.withLock {
                jobs.values.filter { $0.agent == agentId && $0.process.isRunning }.count
            }
            let ingress = execIngressName(record)
            return .object([
                "id": .string(agentId.value),
                "sentence": .string(
                    "Process edge of the io family — a general-purpose COMMAND runner, sealed by default, gated by the `allowed` regexes. Spawns commands in the background; a child may speak the kernel over a private control fd."
                ),
                "cwd": record?.metaValue(forKey: "cwd") ?? .string("<process default>"),
                "allowed": .array(allowedPatterns(record).map(JSON.string)),
                "running": .integer(Int64(running)),
                "verbs": .object([
                    "reflect": .string("Identity + running job count + auth posture. No args."),
                    "start": .string(
                        "args: command:str (req), args:list[str]? (argv, default []), env:dict? (extra env vars), cwd:str?. The FULL line (command + args) must fullmatch one of the record's `allowed` regexes or the start is refused naming the patterns. Spawns in the BACKGROUND; returns {job_id, status:'running'} at once and streams progress/job_done."
                    ),
                    "status": .string(
                        "args: job_id:str?. One job (with full output) or all of this agent's jobs."
                    ),
                    "clear": .string("args: job_id:str?. Drop finished rows of this agent."),
                    "connector": .string(
                        "no args — the reference wire client SOURCE (python): paste `source` verbatim at the top of your program and it gets a `kernel` global (send/emit/watch/on_message); returns {source, howto}."),
                    "interrupt": .string(
                        "args: job_id:str?. SIGINT in-flight subprocesses. Returns {interrupted:int}."
                    ),
                    "stop": .string(
                        "args: job_id:str?. SIGKILL in-flight subprocesses. Returns {killed:int}."
                    ),
                    "boot": .string(
                        "No-op. Nothing auto-restarts: a command is started by a caller, not the record."
                    ),
                ]),
                "emits": .object([
                    "job_started": .string("{type:'job_started', job_id}"),
                    "progress": .string(
                        "{type:'progress', job_id, n, line, stream} — one per output line"),
                    "job_done": .string(
                        "{type:'job_done', job_id, exit_code, ok, stdout, stderr}"),
                ]),
                "wire": .string(
                    "A program speaks the kernel over the inherited fd FANTASTIC_CTRL_FD, a BIDIRECTIONAL socket (one end of a socketpair): frame is [4-byte BE length | kind | rest], both directions — kind T: a JSON envelope; kind B: [4-byte BE H | H-byte JSON header | raw body], the header's `_binary_path` naming where the body belongs (a stream chunk rides raw, never base64). To CALL another agent: write {op:'send', rid:<your id>, target, payload} and the reply COMES BACK on the SAME fd as {op:'reply', rid:<same id>, data}. ops: send/emit/reflect/watch/on_message. A command that never reads the fd runs exactly like any OS process."
                ),
                "inbox": .string(
                    "Any verb NOT listed in `verbs` is MAIL for the running program, delivered to its on_message and ACKed {queued:true}. With no live job it is an error."
                ),
                "tasks": .string(
                    "Every running job holds a row in the kernel TASK TABLE (`task_id` in each `status` snapshot; listed by the universal `ps`). `cancel task_id=<id>` kills it — the OS subprocess is SIGKILLed by this bundle's `on_cancel` hook before the kernel aborts the task, so nothing is orphaned."
                ),
                "authoring": .string(
                    "A general-purpose command runner: `start {command, args}`, gated by the `allowed` regexes on the full command line."
                ),
                "ingress_rule": .string(ingress),
                "sealed": .bool(ingress != "allow_all_ingress"),
            ])
        }
    }

    /// Stringify a JSON double the way python's `str(float)` roughly would.
    private func numString(_ d: Double) -> String {
        if d == d.rounded() && abs(d) < 1e15 {
            return String(format: "%.1f", d)
        }
        return String(d)
    }

    // MARK: - small thread-safe boxes

    /// The child's exit status, delivered by `terminationHandler` to whoever awaits.
    private final class ExitBox: @unchecked Sendable {
        private let lock = NSLock()
        private var status: Int32?
        private var waiter: CheckedContinuation<Int32, Never>?

        func finish(_ s: Int32) {
            lock.lock()
            guard status == nil else {
                lock.unlock()
                return
            }
            status = s
            let w = waiter
            waiter = nil
            lock.unlock()
            w?.resume(returning: s)
        }

        func wait() async -> Int32 {
            await withCheckedContinuation { (c: CheckedContinuation<Int32, Never>) in
                lock.lock()
                if let s = status {
                    lock.unlock()
                    c.resume(returning: s)
                } else {
                    waiter = c
                    lock.unlock()
                }
            }
        }
    }

    /// The partial line held between pipe reads. A chunk boundary falls wherever the
    /// OS put it, never on a newline, so the tail of every chunk waits here for the
    /// rest of its line. `[UInt8]` rather than `Data` because array indices are always
    /// zero-based — a sliced `Data`'s are not, which is a quiet way to read the wrong
    /// bytes back out.
    private final class CarryBox: @unchecked Sendable {
        private let lock = NSLock()
        private var buf: [UInt8] = []

        /// Append a chunk; return every COMPLETE line it closed.
        func push(_ chunk: Data) -> [String] {
            lock.lock()
            defer { lock.unlock() }
            buf.append(contentsOf: chunk)
            var out: [String] = []
            while let i = buf.firstIndex(of: UInt8(ascii: "\n")) {
                out.append(String(decoding: buf[..<i], as: UTF8.self))
                buf.removeFirst(i + 1)
            }
            return out
        }

        /// The unterminated tail at EOF, if there is one.
        func flush() -> String? {
            lock.lock()
            defer { lock.unlock() }
            guard !buf.isEmpty else { return nil }
            let s = String(decoding: buf, as: UTF8.self)
            buf.removeAll()
            return s
        }
    }

    /// Accumulates pipe chunks arriving on Foundation's reader queue.
    private final class DataBox: @unchecked Sendable {
        private let lock = NSLock()
        private var data = Data()

        func append(_ chunk: Data) {
            lock.lock()
            data.append(chunk)
            lock.unlock()
        }

        func take() -> Data {
            lock.lock()
            defer { lock.unlock() }
            return data
        }
    }

    /// The SCOPED ingress rule name for this leg — the same resolution the gate does.
    private func execIngressName(_ record: Agent?) -> String {
        ruleName(
            manifoldSpec(
                record?.metaValue(forKey: "ingress_rule"),
                record?.metaValue(forKey: "auth"),
                "_ingress"),
            default: "deny_ingress")
    }

#endif  // os(macOS)
