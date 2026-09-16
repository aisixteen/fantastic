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
    /// Items are python's `str(x)`, so a number reads `5` and a bool `True`.
    func allowedPatterns(_ record: Agent?) -> [String] {
        switch record?.metaValue(forKey: "allowed") {
        case .some(.string(let s)): return [s]
        case .some(.array(let a)): return a.map(pyStr)
        default: return []
        }
    }

    /// Python's `re` source in ICU's dialect: `(?P<n>…)` → `(?<n>…)` and
    /// `(?P=n)` → `\k<n>`, the two spellings ICU does not share (D5). Escapes and
    /// character classes are walked, so a literal `(?P<` inside `[...]` or after a
    /// `\` is left alone. Everything else — lookaround, backrefs, inline flags —
    /// is already common to both engines and passes through untouched.
    func pyRegexSource(_ pattern: String) -> String {
        let chars = Array(pattern)
        var out = ""
        var i = 0
        var inClass = false
        while i < chars.count {
            let c = chars[i]
            if c == "\\", i + 1 < chars.count {
                out.append(c)
                out.append(chars[i + 1])
                i += 2
                continue
            }
            if inClass {
                if c == "]" { inClass = false }
            } else if c == "[" {
                inClass = true
            } else if c == "(", i + 3 < chars.count, chars[i + 1] == "?", chars[i + 2] == "P" {
                if chars[i + 3] == "<" {
                    out += "(?<"
                    i += 4
                    continue
                }
                if chars[i + 3] == "=" {
                    var j = i + 4
                    var name = ""
                    while j < chars.count && chars[j] != ")" {
                        name.append(chars[j])
                        j += 1
                    }
                    if j < chars.count {
                        out += "\\k<\(name)>"
                        i = j + 1
                        continue
                    }
                }
            }
            out.append(c)
            i += 1
        }
        return out
    }

    /// python `re.fullmatch(pat, s, re.DOTALL)`, compiled: `\A(?:…)\z` so the match
    /// must span the WHOLE string AND the engine backtracks across alternation to
    /// find one (`a|ab` DOES fullmatch `ab` in python; ICU's leftmost `firstMatch`
    /// stops at `a` and would refuse it).
    func pyFullmatch(_ pattern: String) -> NSRegularExpression? {
        try? NSRegularExpression(
            pattern: "\\A(?:\(pyRegexSource(pattern)))\\z",
            options: [.dotMatchesLineSeparators])
    }

    /// `nil` when `line` fullmatches an allowed pattern; else the refusal text.
    ///
    /// The FULL command line (command + args, shell-joined) is matched ANCHORED —
    /// python's `re.fullmatch(pat, line, re.DOTALL)`: the match must span the WHOLE
    /// string (so `echo.*` cannot smuggle `echo; rm -rf /` past a prefix) and `.`
    /// spans newlines (an argv element may contain them). An empty allowlist runs
    /// nothing.
    func commandPermitted(_ record: Agent?, _ line: String) -> String? {
        let pats = allowedPatterns(record)
        if pats.isEmpty {
            return
                "exec_bridge: no commands are allowed on this leg — an operator must "
                + "set `allowed=[<regex>, ...]` naming what may run"
        }
        let full = NSRange(line.startIndex..<line.endIndex, in: line)
        for pat in pats {
            guard let re = pyFullmatch(pat) else { continue }
            if re.firstMatch(in: line, options: [], range: full) != nil { return nil }
        }
        return "exec_bridge: command not allowed: \(pyRepr(line)) — this leg permits: "
            + pyStr(.array(pats.map(JSON.string)))
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
            let child: Child
            var relay: Relay?
            let command: String
            let args: [String]
            let cwd: JSON
            let startedAt: Double
            /// The PUMP holds this, not the table: python's `_read_stream` closes over
            /// the job dict, so `clear` dropping the row does not stop the count and
            /// `progress.n` keeps rising for a job nobody lists any more.
            let counter: LineCounter
            var status: String = "running"
            var exitCode: Int32? = nil
            var stdout: String = ""
            var stderr: String = ""
        }

        /// Guards `jobs`. Never held across an `await`.
        private let lock = NSLock()
        /// Live/finished children, keyed by job id, IN START ORDER — `status` lists
        /// them the way python's dict does. Per-INSTANCE (one bundle per kernel), so
        /// two kernels in one process cannot see each other's children.
        private var jobs: OrderedDictionary<String, Job> = [:]

        public var readme: String? { Self.readmeText }

        // MARK: - dispatch

        public func handle(
            agentId: AgentId,
            payload: JSON,
            kernel: Kernel
        ) async throws -> JSON? {
            let verb = payload["type"].asString ?? ""
            // AUTH GATE — the process edge is an io_bridge leg: SEALED by default,
            // kind "spawn". `reflect` and `connector` are UNGATED (discovery works
            // sealed); mail is ungated too (py parity — the fanout already delivered).
            // `shutdown` is NOT a verb here: python's VERBS table has no such key, so
            // it is mail like any other unowned type.
            let owned = ["start", "boot", "status", "clear", "interrupt", "stop"]
            if owned.contains(verb), let agent = kernel.agent(agentId) {
                let rule = try resolveIngress(
                    ingressRule: agent.metaValue(forKey: "ingress_rule"),
                    auth: agent.metaValue(forKey: "auth"))
                // NO CREDENTIAL FROM THE RECORD. Python builds the action from the
                // target and the payload alone (`gate_inbound(rule, "spawn",
                // {"target", "payload"})`) — a leg that stored `auth_token` on its own
                // record used to satisfy `password_ingress` with its own secret, which
                // is the leg letting itself in.
                let action = AuthAction(
                    kind: "spawn", target: agentId.value, verb: verb, token: nil)
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
                        "exec_bridge: unknown type \(pyRepr(payload["type"])) "
                            + "(no running job to receive it)")
                ])
            }
        }

        /// Any live process of this owner.
        private func hasRunningJob(_ agentId: AgentId) -> Bool {
            lock.withLock {
                jobs.values.contains { $0.agent == agentId && $0.child.isAlive }
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
            // python `command.strip()` — ALL whitespace, newlines included.
            guard let command = payload["command"].asString,
                !command.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            else {
                return Refusal.missing("start", "command")
            }
            // `args` — python `payload.get("args", [])`: the KEY absent is `[]`, an
            // explicit null is not a list and refuses. Items are str/int/float — and a
            // bool IS an int in python, so `true` is legal and stringifies `True`.
            var args: [String] = []
            if let rawArgs = payload.asObject?["args"] {
                guard let arr = rawArgs.asArray else {
                    return Refusal.bad("start", "args", "a list of strings")
                }
                for v in arr {
                    switch v {
                    case .string, .integer, .double, .bool: args.append(pyStr(v))
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
            // python `payload.get("cwd") or rec.get("cwd") or None`: truthiness, not
            // presence, and the RAW value is what the job row records.
            var cwd = payload["cwd"]
            if !pyTruthy(cwd) { cwd = record?.metaValue(forKey: "cwd") ?? .null }
            if !pyTruthy(cwd) { cwd = .null }
            // The per-call `env` block — python `payload.get("env") or {}`, so a falsy
            // value (null, `{}`, `[]`, `""`, `0`) is the empty block, and anything
            // truthy that is not an object refuses. Values are str/int/float/bool.
            var envExtra: [String: String] = [:]
            let rawEnv = payload["env"]
            if pyTruthy(rawEnv) {
                guard let m = rawEnv.asObject else {
                    return Refusal.bad("start", "env", "a dict of str -> str")
                }
                for (k, v) in m {
                    switch v {
                    case .string, .integer, .double, .bool: envExtra[k] = pyStr(v)
                    default:
                        return Refusal.bad("start", "env", "a dict of str -> str")
                    }
                }
            }

            let spawned: Spawn.Spawned
            do {
                spawned = try Spawn.job(
                    argv: argv, selfId: agentId.value, envExtra: envExtra,
                    cwd: cwd.asString)
            } catch {
                // python's `str(OSError)`: `[Errno 2] No such file or directory: 'nope'`.
                return Refusal.make(
                    Refusal.transportError, "exec_bridge: spawn failed: \(error)")
            }
            // python `secrets.token_hex(4)` — ALWAYS 8 hex digits, leading zeros kept.
            let jobId = String(format: "%08x", UInt32.random(in: .min ... .max))
            let relay = Relay(
                fd: spawned.ctrlFd, owner: agentId, jobId: jobId, kernel: kernel)
            // THE RELAY IS THE JOB'S TASK TOO (py `spawn_task(id, "relay", …)`): a
            // program's `kernel.send` must be rowed under work that is still alive,
            // not under the `start` dispatch that settles the moment we return.
            _ = kernel.spawnTask(agent: agentId, verb: "relay") { [relay] in
                await relay.run(taskId: KernelTaskLocals.currentTask)
            }

            let counter = LineCounter()
            // The job OUTLIVES this dispatch, so it needs its OWN task-table row —
            // otherwise a subprocess nobody can reach is the orphan bug.
            let taskId = kernel.spawnTask(agent: agentId, verb: "start") { [weak self] in
                await self?.pump(
                    agentId: agentId, jobId: jobId, spawned: spawned, counter: counter,
                    kernel: kernel)
            }
            lock.withLock {
                jobs[jobId] = Job(
                    agent: agentId, taskId: taskId, child: spawned.child,
                    relay: relay, command: command, args: args, cwd: cwd,
                    startedAt: Date().timeIntervalSince1970, counter: counter)
            }
            await kernel.emit(
                agentId, .object(["type": .string("job_started"), "job_id": .string(jobId)]))
            return .object(["job_id": .string(jobId), "status": .string("running")])
        }

        /// The job's task-table body: drain both pipes LIVE (a `progress` per line as
        /// it arrives), await exit, update the row, emit `job_done`. The row STAYS
        /// after exit so `status`/`clear` can see it (python/rust parity).
        private func pump(
            agentId: AgentId, jobId: String, spawned: Spawn.Spawned, counter: LineCounter,
            kernel: Kernel
        ) async {
            async let outD = pumpStream(
                spawned.stdout, stream: "stdout", agentId: agentId, jobId: jobId,
                counter: counter, kernel: kernel)
            async let errD = pumpStream(
                spawned.stderr, stream: "stderr", agentId: agentId, jobId: jobId,
                counter: counter, kernel: kernel)
            var o = await outD
            var e = await errD
            // A PUMP FAULT is one fault for the whole pump, python's `gather` shape:
            // both reads are abandoned, `stdout` reads `''` and `stderr` carries the
            // `[pump error] …` line. A line over the 64 KiB frame is the only one
            // either runtime raises.
            if let fault = o.fault ?? e.fault {
                o = PumpOut(text: "", fault: nil)
                e = PumpOut(text: "[pump error] \(fault)", fault: nil)
            }
            let code = await spawned.child.wait()
            relayFor(jobId)?.stop()
            // CANCELLED: python's pump re-raises `CancelledError` after labelling the
            // row `stopped` — it emits NO `job_done`, so a killed job never looks like
            // a program that finished on its own.
            if Task.isCancelled {
                lock.withLock {
                    if var j = jobs[jobId] {
                        j.status = "stopped"
                        j.exitCode = spawned.child.exitCode
                        jobs[jobId] = j
                    }
                }
                return
            }
            lock.withLock {
                if var j = jobs[jobId] {
                    j.exitCode = code
                    j.stdout = o.text
                    j.stderr = e.text
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
                    "stdout": .string(o.text), "stderr": .string(e.text),
                ]))
        }

        /// Emit one `progress` per output line. The counter is SHARED across stdout
        /// and stderr and lives on the PUMP, not the table — python's `_read_stream`
        /// closes over the job dict, so `clear` dropping the row leaves `n` counting.
        private func emitLine(
            _ line: String, stream: String, agentId: AgentId, jobId: String,
            counter: LineCounter, kernel: Kernel
        ) async {
            let n = counter.bump(line)
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
                "task_id": j.taskId.map { JSON.integer($0.value) } ?? .null,
                "status": .string(j.status),
                "exit_code": j.exitCode.map { JSON.integer(Int64($0)) } ?? .null,
                "started_at": .double(j.startedAt),
                "lines": .integer(Int64(j.counter.lines)),
                "last_line": .string(j.counter.lastLine),
                "command": .string(j.command),
                "args": .array(j.args.map(JSON.string)),
            ]
            if full {
                o["stdout"] = .string(j.stdout)
                o["stderr"] = .string(j.stderr)
                o["cwd"] = j.cwd
            }
            return .object(o)
        }

        /// python `payload.get("job_id")` under python truthiness: nil = "every job of
        /// this agent". A truthy NON-string can never be a key in the table, so it is
        /// a `not_found` echoing the raw value, not an accidental "all".
        private func jobArg(_ payload: JSON) -> JSON? {
            let raw = payload["job_id"]
            // NOT a ternary: `JSON` is `ExpressibleByNilLiteral`, so `… ? raw : nil`
            // unifies at `JSON` and hands back `.some(.null)` — every caller then
            // reads an absent `job_id` as a named one.
            guard pyTruthy(raw) else { return nil }
            return raw
        }

        /// `status` — one job (full output) or all of THIS agent's jobs (summary).
        private func statusReply(_ agentId: AgentId, payload: JSON) -> JSON {
            lock.withLock {
                if let want = jobArg(payload) {
                    guard let jid = want.asString, let j = jobs[jid], j.agent == agentId
                    else { return noJob(want) }
                    return .object(["jobs": .array([snap(jid, j, full: true)])])
                }
                let rows = jobs.filter { $0.value.agent == agentId }
                    .map { snap($0.key, $0.value, full: false) }
                return .object(["jobs": .array(rows)])
            }
        }

        /// `clear` — drop finished rows of this agent (one, or all non-running). The
        /// ONE caller that wants the status LABEL rather than process liveness: it is
        /// deciding what is finished enough to forget. No `not_found` guard (python
        /// has none): an unknown id simply clears nothing.
        private func clearReply(_ agentId: AgentId, payload: JSON) -> JSON {
            let want = jobArg(payload)?.asString
            let doomed: [String] = lock.withLock {
                jobs.filter { (id, j) in
                    j.agent == agentId
                        && (want.map { $0 == id } ?? (j.status != "running" && j.status != "stopping"))
                }.map { $0.key }
            }
            lock.withLock { for id in doomed { jobs.removeValue(forKey: id) } }
            return .object(["cleared": .integer(Int64(doomed.count))])
        }

        /// The `not_found` refusal for a job_id naming nothing. Spelled once; the raw
        /// value is echoed both in python's repr and in the `job_id` field.
        private func noJob(_ jid: JSON) -> JSON {
            Refusal.make(
                Refusal.notFound, "exec_bridge: no job \(pyRepr(jid))",
                extra: [("job_id", jid)])
        }

        /// …and the guard that decides. A `job_id` naming nothing used to answer
        /// `{killed:0}` / `{interrupted:0}` — the same reply as "it had already
        /// exited", so a typo read as success.
        private func noSuchJob(_ agentId: AgentId, _ payload: JSON) -> JSON? {
            guard let want = jobArg(payload) else { return nil }
            let known = lock.withLock { want.asString.flatMap { jobs[$0] }?.agent == agentId }
            return known ? nil : noJob(want)
        }

        /// `interrupt` / `stop` — signal this agent's jobs, or one named `job_id`.
        /// Returns how many were actually signalled.
        private func signalJobs(_ agentId: AgentId, _ payload: JSON, _ sig: Int32) -> Int {
            let want = jobArg(payload)?.asString
            return lock.withLock {
                var n = 0
                for (id, var j) in jobs {
                    if j.agent != agentId || (want.map { $0 != id } ?? false) { continue }
                    if !j.child.isAlive { continue }  // already gone — never pretend
                    if sig == SIGKILL {
                        j.status = "stopping"
                        jobs[id] = j
                    }
                    if j.child.signal(sig) { n += 1 }
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
            counter: LineCounter, kernel: Kernel
        ) async -> PumpOut {
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
                // PAST THE FAULT THE PIPE IS STILL DRAINED, and discarded. python's
                // reader keeps being fed by its transport after the raise; stop
                // reading here and the pipe fills, the child blocks on `write`, and
                // the job never exits at all.
                if carry.overran { return }
                box.append(chunk)
                let (ls, overrun) = carry.push(chunk)
                for line in ls { cont.yield(line) }
                if overrun {
                    // python's `StreamReader.readline` raises
                    // `ValueError("Separator is not found, and chunk exceed the limit")`
                    // once its 64 KiB buffer holds no newline. The pump catches it and
                    // BOTH reads are lost.
                    box.clear()
                    cont.finish()
                }
            }
            for await line in lines {
                await emitLine(
                    line, stream: stream, agentId: agentId, jobId: jobId, counter: counter,
                    kernel: kernel)
            }
            if carry.overran {
                return PumpOut(
                    text: "", fault: "Separator is not found, and chunk exceed the limit")
            }
            // REPLACED, not dropped: python decodes `errors="replace"`, so an invalid
            // byte becomes U+FFFD and the rest of the output survives. `String(data:
            // encoding:)` answers nil for the whole buffer instead.
            return PumpOut(text: String(decoding: box.take(), as: UTF8.self), fault: nil)
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
            if job.child.isAlive { _ = job.child.signal(SIGKILL) }
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
                let deny = rule as? IngressRules.DenyInbound, deny.unknown != nil
            {
                // python names the SPEC AS WRITTEN (`{spec!r}`), not the stem it
                // resolved to — the operator has to see what they typed.
                return "ingress_rule=\(pyRepr(spec)) is not a rule this bundle implements — "
                    + "legal values: allow_all_ingress, deny_ingress"
            }
            // `password_ingress` is meaningless on an in-process leg: the token is
            // read off a WIRE ENVELOPE and the process edge has none — no frame ever
            // presents one, so the rule could only refuse, or (until 2026-09-02) be
            // satisfied by a secret persisted ON THE RECORD. Refused at the write
            // (#116). Same rule as `FantasticFile.passwordRefusal`.
            for (field, stem) in [("ingress_rule", "password_ingress"), ("auth", "password")] {
                if let spec = meta[field], ruleName(spec, default: "") == stem {
                    return "\(field)=\(pyRepr(spec)) is not applicable to the process edge: "
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
                case .array(let a): pats = a.map(pyStr)
                default: pats = []
                }
                if pats.isEmpty {
                    return "allowed must be a non-empty list of regexes, "
                        + "e.g. <allowed><item>ls( .*)?</item></allowed>"
                }
                for pat in pats {
                    do {
                        _ = try NSRegularExpression(pattern: pyRegexSource(pat))
                    } catch {
                        // python appends `: <re.error>`; the engine's own wording past
                        // `': '` is the residual ICU cannot reproduce (D4/D5).
                        return "allowed contains a broken regex \(pyRepr(pat)): "
                            + (error as NSError).localizedDescription
                    }
                }
            }
            return nil
        }

        /// python's `readme.md`, verbatim — 139 lines and the trailing newline.
        static let readmeText = """
            # exec_bridge — process edge

            Spawns a COMMAND in the BACKGROUND and hands it a kernel wire. Crossing kernel↔process is
            an IO edge, so this leg is gated exactly like `file_bridge`/`web_ws`: **sealed by default**,
            and the commands it may run come from an operator **allowlist** of regexes.

            It is a GENERAL-PURPOSE command runner, not a code runner: there is no `code` field, no
            `interpreters`, no shim. A program is a FILE some process (or a `write_stream`) put on disk;
            running it is `command` + `args`, exactly like anything else.

            Verbs: `start` · `status` · `stop` · `interrupt` · `clear` · `reflect` · `boot`.
            Any verb NOT in that list is **MAIL** for the running program (see below).

            - `start(command, args?, env?, cwd?)` → `{job_id, status:'running'}` at once — non-blocking,
              many jobs in parallel. `command` is the program/binary; `args` is its argv (a list of
              strings). The FULL line (`command` + `args`, shell-joined) must fullmatch one of the
              record's `allowed` regexes or the start is refused, naming the patterns. `env` adds
              environment vars; `cwd` overrides the record's. There is no blocking run-and-wait verb:
              output streams as `progress` events on this agent's inbox (one per line, `{n, line,
              stream}`) with a final `job_done` (`{exit_code, ok, stdout, stderr}`), after a
              `job_started`.
            - `status(job_id?)` → `{jobs:[…]}` — one job with its full stdout/stderr, or every job of
              this agent as summaries.
            - `stop(job_id?)` SIGKILL · `interrupt(job_id?)` SIGINT · `clear(job_id?)` forget finished
              rows. Omit `job_id` and it applies to all of this agent's jobs.
            - `boot` — no-op. Nothing auto-restarts: a command is started by a caller, not by the record.

            Every running job is a row in the kernel TASK TABLE (`task_id` in each `status` snapshot,
            listed by the universal `ps`). `cancel task_id=<id>` kills it — the OS process is SIGKILLed
            before the task is aborted, so nothing is orphaned. `delete_agent` tears its jobs down too.

            ## Sealed by default

            A freshly created leg denies every verb except `reflect` (`{reason:"unauthorized"}`).
            Open it consciously:

            ```
            update_agent <id> ingress_rule=allow_all_ingress      # open wide (local/dev)
            update_agent <id> ingress_rule=password_ingress       # require group token ($FANTASTIC_GROUP_TOKEN)
            ```

            ## The allowlist is the law

            Even open, this leg spawns ONLY a command line matching one of the operator's `allowed`
            regexes on the record:

            ```
            update_agent <id> allowed='["/usr/bin/python3 .*", "ffmpeg .*"]'
            ```

            `reflect` advertises the live list as `allowed` — read it, then send a `start` whose
            `command` + `args` shell-join into a line that fullmatches one. There is **no default** and
            no per-call override: a request that matches nothing is refused, with the patterns in the
            message. An empty list is a legal state — the leg then runs nothing and says so. The list is
            PER RECORD (nothing is inherited from a parent). Which interpreter/binary may run is the
            operator's decision, never the caller's — that is exactly why the gate is a record field a
            caller cannot edit.

            ## Build a durable program: a FILE plus a command

            A program that persists and re-runs is a FILE you write plus a command that runs it — with
            no `code` field at all:

            1. **Write the program to a file** on a `file_bridge` leg, its bytes carried over a STREAM so
               nothing is escaped: `write_stream {path, bytes, truncate:true}` — `bytes` is VERBATIM, written between
               `<bytes>` and `</bytes>` exactly as it should land — real newlines, real quotes, nothing escaped. A
               program squeezed into an escaped string is lost to one stray quote; the verbatim tag is why you never
               escape it.
            2. **Run it** here: `<type>start</type> <command>interp</command> <args><item>your file</item></args>`, past the `allowed`
               gate.
            3. **Re-run** the SAME command later — the file is durable on disk, so it re-executes the
               same program against whatever the inputs hold THEN. That is what "a thing that keeps
               itself up to date" means.

            A running program has the full wire, so it can itself `create_agent` more agents, read memory
            anywhere, call an AI, or spawn more jobs — tools compose into tools.

            ## Mail — `{queued:true}`

            Send a running program a turn by sending THIS agent any verb the bundle does not own; it is
            delivered to the program's `on_message` and ACKed `{queued:true}`. With no live job it is an
            ERROR, not a queue — a program that exited already has no inbox. Results come back as whatever
            the program emits on this agent's id (tag `say`/`done` with the caller's `client_id` and
            `watch` the id).

            ## The wire (a program speaks it, or ignores it)

            The published contract is the WIRE. The child inherits two env vars:

            - `FANTASTIC_CTRL_FD` — an already-open socket fd to the spawner.
            - `FANTASTIC_SELF_ID` — the OWNER agent's id (the tool's public id, not the job id).

            Arguments arrive as ARGV, like any command — there is no `FANTASTIC_ARGS`.

            Framing both directions: `[4-byte big-endian length | kind | rest]` — kind `T`: a JSON
            envelope; kind `B`: the io_bridge binary frame `[4-byte BE H | H-byte JSON header | raw
            body]`, the header's `_binary_path` naming where the body belongs, so a `read_stream`
            chunk or a `write_stream` payload crosses raw (never base64). Child → parent, the six
            ops: `{op:"send", rid, target, payload}` · `{op:"emit", target?, payload}` ·
            `{op:"watch", src}` · `{op:"unwatch", src}` · `{op:"onmessage"}` · `{op:"offmessage"}`.
            Parent → child: `{op:"reply", rid, data}` · `{op:"event", src, payload}` ·
            `{op:"inbox", payload}`. `rid` correlates a `send` with its `reply`; the parent echoes it
            back untouched. ONE FRAME CAPS AT 16 MiB in every runtime — over it the length prefix is a
            wire violation and the relay closes the socket, so move bulk with `write_stream` chunks
            (the reference client raises rather than writing such a frame). **EOF on the fd means the kernel is GONE** — exit, or become an orphan
            nothing can reach. Traffic goes child → spawner → kernel only: a job never dials a host and
            never knows a URL. A command that never reads the fd runs exactly like any OS process.

            A program that wants a `kernel` global rather than raw frames includes the reference wire
            client (`exec_bridge/_connector.py`, `CONNECTOR_SRC`) at the top of its own source — it is a
            library the program bundles, not something the bridge prepends. It exposes:

            ```python
            kernel.send(target, payload) -> reply      # request/reply to any agent by id
            kernel.emit(payload)                       # fire-and-forget on YOUR OWN agent
            kernel.emit(target, payload)               # …or on another
            kernel.reflect(target="kernel")
            off = kernel.watch(src, cb)                # PUSH: cb(payload) per event from src
            off = kernel.on_message(cb)                # PUSH: turns on THIS agent's inbox
            kernel.self_id                             # your agent id (arguments are ARGV)
            ```

            So a job is a first-class routine: read memory anywhere, call an AI, spawn another job, push
            to any peer — all by id. A step written as a program and a step written as an LLM call are
            substitutable.

            ## Not a sandbox

            An allowlisted command is a NAMED command, not a sandbox. A python it permits still has
            `open()`, `socket` and `subprocess`. What the allowlist buys is that the operator chose WHICH
            commands may run instead of the kernel guessing, and that a program cannot pick a different
            one. Anything stronger needs an OS mechanism (`sandbox-exec`, a separate uid, a container)
            and none of it is reachable in-process. Stated plainly rather than reassuringly: opening this
            leg grants local code execution to whoever can reach it.

            **Get the client instead of writing one:** send this agent `<type>connector</type>` — the
            reply's `source` IS the reference wire client; paste it verbatim at the top of your
            program file. A program's sends carry THIS runner's identity and grants (its `can_call`;
            a target must list the runner in `callable_by`).

            """

        // MARK: - reflect

        private func reflectReply(_ agentId: AgentId, kernel: Kernel) -> JSON {
            let record = kernel.agent(agentId)
            let running = lock.withLock {
                jobs.values.filter { $0.agent == agentId && $0.child.isAlive }.count
            }
            // python `rec.get("cwd") or "<process default>"` — TRUTHINESS, so an
            // empty string or a 0 reads as "not set", not as a directory.
            let cwd = record?.metaValue(forKey: "cwd") ?? .null
            var out: OrderedDictionary<String, JSON> = [
                "id": .string(agentId.value),
                "sentence": .string(
                    "Process edge of the io family — a general-purpose COMMAND runner, sealed by default, gated by the `allowed` regexes. Spawns commands in the background; a child may speak the kernel over a private control fd."
                ),
                "cwd": pyTruthy(cwd) ? cwd : .string("<process default>"),
                "allowed": .array(allowedPatterns(record).map(JSON.string)),
                "running": .integer(Int64(running)),
                "verbs": .object([
                    "start": .string("args: command:str (req), args:list[str]? (argv, default []), env:dict? (extra"),
                    "status": .string("args: job_id:str? — one job (with full stdout/stderr), or all of this agent's"),
                    "stop": .string("args: job_id:str? — SIGKILL one job or ALL of this agent's running jobs."),
                    "interrupt": .string("args: job_id:str? — SIGINT one job or ALL of this agent's running jobs."),
                    "clear": .string("args: job_id:str? — drop finished jobs (one, or all non-running) from the RAM"),
                    "connector": .string("no args — the reference wire client SOURCE (python): paste `source` verbatim at the top of your program and it gets a `kernel` global (send/emit/watch/on_message); returns {source, howto}."),
                    "reflect": .string("Identity + running job count + auth posture. No args."),
                    "boot": .string("No-op. Nothing auto-restarts: a command is started by a caller, not by the"),
                ]),
                "emits": .object([
                    "job_started": .string("{type:'job_started', job_id}"),
                    "progress": .string(
                        "{type:'progress', job_id, n, line, stream} — one per output line"),
                    "job_done": .string(
                        "{type:'job_done', job_id, exit_code, ok, stdout, stderr}"),
                ]),
                "wire": .string(
                    "A program speaks the kernel over the inherited fd FANTASTIC_CTRL_FD, a BIDIRECTIONAL socket (one end of a socketpair): frame is [4-byte BE length | kind | rest], both directions — kind T: a JSON envelope; kind B: [4-byte BE H | H-byte JSON header | raw body], the header's `_binary_path` naming where the body belongs (a stream chunk rides raw, never base64). To CALL another agent: write {op:'send', rid:<your id>, target, payload} and the reply COMES BACK on the SAME fd as {op:'reply', rid:<same id>, data} — the fd is not one-way. It is not inline, though: read frames in a loop and skip any whose rid isn't yours (event/inbox frames can arrive first) until you see your reply. ops: send/emit/reflect/watch/on_message. A command that never reads the fd runs exactly like any OS process."
                ),
                "inbox": .string(
                    "Any verb NOT listed in `verbs` is MAIL for the running program, delivered to its on_message and ACKed {queued:true}. With no live job it is an error — a script that only defines something exits before its first turn arrives."
                ),
                "tasks": .string(
                    "Every running job holds a row in the kernel TASK TABLE (`task_id` in each `status` snapshot; listed by the universal `ps`). `cancel task_id=<id>` kills it — the OS subprocess is SIGKILLed by this bundle's `on_cancel` hook before the kernel aborts the task, so nothing is orphaned."
                ),
                "authoring": .string(
                    "A general-purpose command runner: <type>start</type> <command>…</command> <args><item>…</item></args>, gated by the `allowed` regexes on the full command line."
                ),
            ]
            // The auth posture, python `**_describe(rec)` — last, all three keys.
            for (k, v) in describe(record?.meta ?? [:]) { out[k] = v }
            return .object(out)
        }
    }

    // MARK: - small thread-safe boxes

    /// One pipe's collected text, plus the fault that abandoned the read.
    ///
    /// python's pump wraps BOTH reads in one `gather`, so a fault on either pipe is
    /// a fault for the pump: `stdout` comes back `''` and `stderr` carries
    /// `[pump error] <str(exc)>`.
    struct PumpOut {
        let text: String
        let fault: String?
    }

    /// The job's output-line counter, held by the PUMP rather than by the job row.
    ///
    /// python's `_read_stream` closes over the job DICT, so `clear {job_id}` dropping
    /// the row from the table does not reset or stop the count — `progress.n` keeps
    /// rising for a job nothing lists any more. A counter on the row could not do
    /// that. Shared across stdout and stderr, so `n` is one monotonic sequence.
    final class LineCounter: @unchecked Sendable {
        private let lock = NSLock()
        private var count = 0
        private var last = ""

        var lines: Int { lock.withLock { count } }
        var lastLine: String { lock.withLock { last } }

        func bump(_ line: String) -> Int {
            lock.withLock {
                count += 1
                last = line
                return count
            }
        }
    }

    /// The partial line held between pipe reads. A chunk boundary falls wherever the
    /// OS put it, never on a newline, so the tail of every chunk waits here for the
    /// rest of its line. `[UInt8]` rather than `Data` because array indices are always
    /// zero-based — a sliced `Data`'s are not, which is a quiet way to read the wrong
    /// bytes back out.
    private final class CarryBox: @unchecked Sendable {
        /// python's `asyncio.StreamReader` limit: past 64 KiB with no separator in
        /// sight the read RAISES rather than buffering without bound.
        static let limit = 65536

        private let lock = NSLock()
        private var buf: [UInt8] = []
        private var blown = false

        var overran: Bool { lock.withLock { blown } }

        /// Append a chunk; return every COMPLETE line it closed (python emits those
        /// before it raises) and whether the carry then outgrew the limit with no
        /// newline in it.
        func push(_ chunk: Data) -> (lines: [String], overrun: Bool) {
            lock.lock()
            defer { lock.unlock() }
            if blown { return ([], true) }
            buf.append(contentsOf: chunk)
            var out: [String] = []
            while let i = buf.firstIndex(of: UInt8(ascii: "\n")) {
                out.append(String(decoding: buf[..<i], as: UTF8.self))
                buf.removeFirst(i + 1)
            }
            if buf.count > Self.limit {
                blown = true
                buf.removeAll()
                return (out, true)
            }
            return (out, false)
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

        /// Drop everything collected — a faulted pump reports `''`, not a prefix.
        func clear() {
            lock.lock()
            data.removeAll()
            lock.unlock()
        }
    }

#endif  // os(macOS)
