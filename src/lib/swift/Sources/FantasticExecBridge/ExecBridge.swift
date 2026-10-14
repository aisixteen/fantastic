// Process edge of the io family — swift mirror of the canonical python
// `io/exec_bridge` (canonical; on disagreement this file is wrong). Rust's
// `fantastic-exec-bridge` is the second reference.
//
// Crossing kernel↔subprocess is gated like any io_bridge leg: SEALED BY DEFAULT
// (absent `ingress_rule` ⇒ every verb except `reflect` denies), and bound by a
// regex command ALLOWLIST — `allowed` on the record is a list of regexes, and
// `start {command, args}` refuses unless the shell-joined line fullmatches one.
// No per-call override, no default, no PATH fallback; an empty `allowed` runs
// nothing. The one other permitted line is the record's `shell` (true = the
// child's own `$SHELL` bare, "login" = `$SHELL -l`, PTY only): a PTY `start` with
// no `command` runs that `$SHELL`, read from the child's environment —
// autodiscovery, never a default.
//
// It is a GENERAL-PURPOSE COMMAND runner. A program is a FILE
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

    import FantasticFile
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
    ///
    /// The BARE pattern compiles first: wrapped, an unbalanced one (`x)|(.*`)
    /// closes the group early and matches every line, where python's
    /// `re.fullmatch` raises — a broken pattern permits nothing (audit 2026-10-14).
    func pyFullmatch(_ pattern: String) -> NSRegularExpression? {
        guard (try? NSRegularExpression(pattern: pyRegexSource(pattern))) != nil else {
            return nil
        }
        return try? NSRegularExpression(
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
    /// Appended to an allowlist refusal when `command` holds several words and
    /// `args` is empty — py `COMMAND_ONE_PROGRAM`, the same bytes in every runtime.
    let commandOneProgram =
        " — `command` is ONE program and is quoted as one word; put its arguments "
        + "in `args` (a list), e.g. command=python3 args=[\"-c\", \"print(1)\"]"

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

    // ── the child's own shell (py `SHELL_VAR` / `_shell_tail` / `_shell_permits`) ──
    //
    // A PTY `start` that names no `command` runs the CHILD's `$SHELL`: a client
    // with no environment of its own (a browser page) cannot name the shell, but
    // the record can — `inherit_env`/`env` decide what the child's environment
    // holds, and `SHELL` is read from THAT, never from the kernel's own env unless
    // it was passed down. One read, no chain: a child env without `SHELL` is
    // refused loud.
    //
    // The ALLOWLIST still decides what may run. A record writer who does not know
    // the shell's path cannot write an exact `allowed` line for it, so the
    // record's `shell` field permits the resolved shell line itself — and nothing
    // wider:
    //
    //   shell: true     → exactly [$SHELL]        (the bare shell)
    //   shell: "login"  → exactly [$SHELL, "-l"]  (a login shell)
    //
    // It is a LINE, not a pattern: any other argv, and every pipe job, still has
    // to fullmatch `allowed`. Who may set `shell` is who may write the record —
    // the same authority that writes `allowed`.

    /// The env var a PTY start without `command` reads its shell from.
    let SHELL_VAR = "SHELL"

    /// The argv the record's `shell` permits AFTER `$SHELL`, or nil when the
    /// record permits no shell line. Only the JSON bool `true` counts — a stray
    /// `1` is not a permission (python compares `v is True`).
    func shellTail(_ record: Agent?) -> [String]? {
        switch record?.metaValue(forKey: "shell") {
        case .some(.bool(true)): return []
        case .some(.string("login")): return ["-l"]
        default: return nil
        }
    }

    /// True when `argv` is exactly the shell line the record's `shell` permits: a
    /// PTY job whose argv is the child's own `$SHELL` plus the tail its `shell`
    /// value names. Judged on the RESOLVED argv, so it holds whether the caller
    /// named the shell or left `command` out.
    func shellPermits(_ record: Agent?, env: [String: String], argv: [String], pty: Bool) -> Bool {
        guard pty, let tail = shellTail(record), let shell = env[SHELL_VAR], !shell.isEmpty
        else { return false }
        return argv == [shell] + tail
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
        public var verbs: [String] { Array(Self.verbDocs.keys) }
        public var verbDocTable: JSON { .object(Self.verbDocs) }
        public var emitsDoc: JSON? { .object(Self.emits) }
        public var notesDoc: JSON? { .object(Self.notes) }

        /// python's module-level `VERBS` — ONE table: reflect renders it and
        /// `advertises` reads it.
        /// What a job emits on this agent's inbox — served by `docs` (tier detailed).
        /// Python `EMITS`.
        static let emits: OrderedDictionary<String, JSON> = [
            "job_started": .string(
                "{type:'job_started', job_id, command, args} — the RESOLVED invocation (a PTY start without `command` names the shell it ran)"
            ),
            "progress": .string("{type:'progress', job_id, n, line, stream} — one per output line"),
            "job_done": .string(
                "{type:'job_done', job_id, exit_code, ok, lines?, last_line?, error?} — a pipe job's output is read with `tail`/`grep`; a PTY job carries exit_code and ok only"
            ),
            "output": .string("{type:'output', job_id, data} — a PTY job's output chunk (UTF-8)"),
        ]

        /// How a running program talks back — served by `docs` (tier detailed).
        /// Python `NOTES`.
        static let notes: OrderedDictionary<String, JSON> = [
            "wire": .string(
                "A program speaks the kernel over the inherited fd FANTASTIC_CTRL_FD, a BIDIRECTIONAL socket: frame is [4-byte BE length | kind | rest], both directions — kind T: a JSON envelope; kind B: [4-byte BE H | H-byte JSON header | raw body], the header's `_binary_path` naming where the body belongs as steps from the frame root — a key as a string (taken literally: ['payload', 'a.b'] is the key 'a.b'), a list index as an int (a stream chunk rides raw, never base64). To CALL another agent: write {op:'send', rid:<your id>, target, payload}; the reply COMES BACK on the SAME fd as {op:'reply', rid:<same id>, data}. Read frames in a loop and skip any whose rid isn't yours (event/inbox frames can arrive first). ops: send/emit/reflect/watch/on_message. A command that never reads the fd runs like any OS process. The `connector` verb returns a ready client."
            ),
            "inbox": .string(
                "Any verb this bundle does not answer is MAIL for the running program, delivered to its on_message and ACKed {queued:true}. With no live job it is refused — a script that only defines something exits before its first message arrives."
            ),
            "tasks": .string(
                "Every running job IS a row in the kernel TASK TABLE: its `job_id` is that row's id, listed by `ps`. `cancel task_id=<job_id>` kills it — the OS process is SIGKILLed by this bundle's `on_cancel` hook first, so nothing is orphaned."
            ),
        ]

        static let verbDocs: OrderedDictionary<String, JSON> = [
            "start": .string("args: command:str (req — the executable ALONE, e.g. `grep`; on a PTY start, absent = the child's $SHELL), args:list[str]? (its arguments, one item each — `<args><item>-rn</item><item>x</item></args>`), pty:{cols,rows}? (run on a terminal). The FULL line (command + args) must fullmatch one of the `allowed` regexes, or be the shell line the record's `shell` permits."),
            "status": .string("args: job_id:int? — one job (with its `cwd`; read its output with `tail`/`grep`), or all of this agent's jobs (summary). A job's id is its kernel task id: `ps` lists it, `cancel task_id=` kills it. Returns {jobs:[…]}."),
            "stop": .string(
                "args: job_id:int? — SIGKILL one job (its whole session: the command and everything it launched) or ALL running jobs on this agent."
            ),
            "interrupt": .string("args: job_id:int? — SIGINT one job or ALL running jobs on this agent."),
            "clear": .string("args: job_id:int? — drop finished jobs (one, or all non-running) from the RAM"),
            "write": .string("args: job_id:int (req), data:str or bytes:bytes (exactly one). Keystrokes/paste into a PTY job, written IN FULL and serialized per job (a bracketed paste is never truncated or interleaved). `bytes` is written VERBATIM — a trailing newline (Enter) survives; a `<call>` strips a scalar value like `data`, so send keys that end in Enter as `bytes`. Returns {written:int}."),
            "resize": .string("args: job_id:int (req), cols:int (req), rows:int (req). Sets a PTY job's window size (the tty sends SIGWINCH, so a TUI redraws). Returns {resized:true, cols, rows}."),
            "ack": .string("args: job_id:int (req), chars:int (req). Flow control from a streaming consumer (an xterm acks each `output` chunk after parsing it): the reader pauses past 100K unacked chars and resumes under 5K. Returns {unacked:int, paused:bool}."),
            "output": .string("args: job_id:int (req), max_bytes:int? (default the whole 256 KiB ring). The PTY job's recent output — what a client re-attaching replays before the live `output` events. Returns {job_id, output:str}."),
            "tail": .string("args: job_id:int (req), lines:int? (default 20, max 500), stream:str? (stdout|stderr|both, default both — a PTY job has one). The last lines a job has printed SO FAR — running or ended, pipe or PTY. Returns {job_id, status, lines:[{n, stream, line}], printed, dropped}."),
            "grep": .string("args: job_id:int (req), match:str (req — a regex, searched in each line), lines:int? (most matches returned, default 50, max 500 — the LAST ones), stream:str? (stdout|stderr|both, default both). Searches what a job has printed SO FAR — running or ended, pipe or PTY; lines older than the 256 KiB ring are gone (`dropped` says how many). Returns {job_id, status, matches:[{n, stream, line}], total, scanned, dropped}."),
            "paste_image": .string("args: job_id:int (req), data:bytes (req), mime:str (req — image/png|jpeg|gif|webp). Saves a pasted image to a per-job scratch file and types its path into the PTY (a drag-drop — a program like claude in a browser xterm cannot reach the browser clipboard). Returns {path:str, bytes:int}."),
            "connector": .string("no args — the reference wire client SOURCE (python): paste `source` verbatim at the top of your program and it gets a `kernel` global (send/emit/watch/on_message); returns {source, howto}."),
            "monitor": .string("args: job_id:int (req — a RUNNING job of this agent), notify:str (req — the agent told), match:str? (a regex; absent = every line), every:number? (seconds — at most one delivery per window; required without `match`), tail:int? (newest lines per delivery, default 100), label:str?. Watches the job's output AS IT ARRIVES and SENDS what matches to `notify` as a turn — it polls nothing. Returns {monitor_id, job_id, notify, match, every, tail, label}; kernel `cancel` (or `unmonitor`) ends it, and it ends with its job."),
            "monitors": .string("no args — this agent's monitors: {monitors:[{monitor_id, job_id, notify, match, every, tail, label, delivered, pending, status, error}]}. `status` is running, or dead once `notify` REFUSED a delivery (`error` says why; nothing more is sent) — `unmonitor` clears it."),
            "unmonitor": .string("args: monitor_id:int (req). End one of this agent's monitors (kernel `cancel` on its id does the same). Returns {unmonitored:true, monitor_id}."),
            "reflect": .string("Identity + running job count + auth posture. No args."),
            "boot": .string("No-op. Nothing auto-restarts: a command is started by a caller, not by the record."),
        ]
        public init() {}

        /// This instance's live monitors (`Monitor.swift`).
        let monitorTable = MonitorTable()

        // MARK: - the job table

        /// One in-flight (or finished) program — the REAL resource this bundle owns.
        ///
        /// Keyed by its `job_id`, which IS the kernel task id its pump runs under;
        /// every verb filters by `agent` (owner), so a job_id from another agent is
        /// still "no job". `onCancel` matches the KEY, so killing one job never
        /// touches another running on the same agent. The invocation
        /// (`command`/`args`/`cwd`) is recorded so `status` can report WHAT ran.
        private struct Job {
            let agent: AgentId
            let child: Child
            var relay: Relay?
            let command: String
            let args: [String]
            let cwd: JSON
            let startedAt: Double
            /// The PUMP holds this, not the table: python's `_read_stream` closes over
            /// the job dict.
            let counter: LineCounter
            var status: String = "running"
            var exitCode: Int32? = nil
            /// The terminal of a PTY job (`start pty={cols,rows}`); nil for a pipe job.
            var pty: PtyJob? = nil
        }

        /// Jobs whose TASK is being cancelled, marked by `onCancel` BEFORE it kills.
        /// The kernel aborts the Swift `Task` only after the hook returns, and the
        /// pump runs on its own thread: between the SIGKILL and the abort it can see
        /// the pipes close and the child exit while `Task.isCancelled` is still
        /// false — and announce a normal `job_done` for a job the kernel killed
        /// (measured: 1 full run in 15). This mark is set before the kill, so any
        /// exit the kill caused finds it. Python cannot interleave there: its cancel
        /// lands in the same event-loop turn as the hook. Guarded by `lock`; a set of
        /// its own, not a row field, because `clear` may drop the row mid-pump.
        private var cancelling: Set<TaskId> = []

        /// Whether `jobId`'s exit was its task's cancellation — and forget the mark.
        private func takeCancelled(_ jobId: TaskId) -> Bool {
            let marked = lock.withLock { cancelling.remove(jobId) != nil }
            return marked || Task.isCancelled
        }

        /// Guards `jobs`. Never held across an `await`.
        private let lock = NSLock()
        /// Live/finished children, keyed by job id (the job's task id), IN START
        /// ORDER — `status` lists them the way python's dict does. Per-INSTANCE (one
        /// bundle per kernel), so two kernels in one process cannot see each other's
        /// children.
        private var jobs: OrderedDictionary<TaskId, Job> = [:]

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
            // sealed); every other type is gated, MAIL INCLUDED (python `handler`,
            // audit BV-8): a sealed leg delivers nothing to its program — the
            // relay's inbox drain refuses the same payloads, since the fanout
            // reaches it before this handler runs.
            // `shutdown` is NOT a verb here: python's VERBS table has no such key, so
            // it is mail like any other unowned type.
            if verb != "reflect", verb != "connector",
                let denied = Self.gate(agentId, verb: verb, kernel: kernel)
            {
                return denied
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
                // 2026-09-01, E30 subai: ten passes lost before the first working
                // frame). Ungated like reflect.
                return .object([
                    "source": .string(CONNECTOR_SRC),
                    "howto": .string(
                        "Write `source` verbatim as the first lines of your program file, then "
                            + "your code below it. It opens FANTASTIC_CTRL_FD (a socket, length-prefixed "
                            + "frames; a payload carrying a bytes value rides the binary channel raw: "
                            + "`write_stream` puts its chunk in `bytes`, `read_stream` passes `bytes: b''` "
                            + "and gets its chunk back raw in the reply's `bytes`) and "
                            + "exposes `kernel.send(target, payload) -> reply` (raises if "
                            + "the relay could not deliver), "
                            + "`kernel.emit(payload)`, `kernel.watch(src, cb)`, `kernel.on_message(cb)`, "
                            + "`kernel.self_id`. Arguments arrive as ARGV, like any command — there is "
                            + "no `kernel.args`. One frame caps at 16 MiB; move more than that with "
                            + "`write_stream` chunks. Your sends carry THIS runner's identity "
                            + "and grants (its `can_call`; a target must list this runner in "
                            + "`callable_by`). Do not rewrite the wire by hand."),
                ])
            case "write":
                return await writeReply(agentId, payload)
            case "resize":
                return resizeReply(agentId, payload)
            case "ack":
                return ackReply(agentId, payload)
            case "output":
                return outputReply(agentId, payload)
            case "tail":
                return tailGrepReply("tail", agentId, payload)
            case "grep":
                return tailGrepReply("grep", agentId, payload)
            case "paste_image":
                return pasteImageText(agentId, payload)
            case "monitor":
                return monitorReply(agentId, payload, kernel: kernel)
            case "monitors":
                return monitorTable.listing(agentId)
            case "unmonitor":
                return await unmonitorReply(agentId, payload, kernel: kernel)
            case "interrupt", "stop":
                let want: TaskId?
                switch jobArg(verb, payload, required: false) {
                case .failure(let r): return r.json
                case .success(let j): want = j
                }
                if let gone = noSuchJob(verb, agentId, want) { return gone }
                let sig = verb == "stop" ? SIGKILL : SIGINT
                let key = verb == "stop" ? "killed" : "interrupted"
                return .object([key: .integer(Int64(signalJobs(agentId, want, sig)))])
            default:
                // ADDRESSABLE INBOX. A verb this bundle does not own is MAIL for the
                // running program — the substrate's fanout has already delivered it,
                // so this only ACKs. Re-emitting would double-deliver.
                if hasRunningJob(agentId) {
                    return .object(["queued": .bool(true)])
                }
                return Refusal.make(
                    Refusal.unhandled,
                    "exec_bridge: unknown type \(pyRepr(payload["type"])) "
                        + "(no running job to receive it)",
                    extra: [("verb", payload["type"])])
            }
        }

        /// THE LEG'S GATE on one payload, off the LIVE record — python `_gate`:
        /// nil when its `ingress_rule` admits it, else the rule's refusal. The
        /// handler asks it for every verb but `reflect`/`connector`, mail included;
        /// the relay's inbox drain asks it for every payload.
        static func gate(_ agentId: AgentId, verb: String, kernel: Kernel) -> JSON? {
            // No record is python's `kernel.get(id) or {}`: no rule, so sealed.
            let agent = kernel.agent(agentId)
            let rule = resolveIngress(
                ingressRule: agent?.metaValue(forKey: "ingress_rule"),
                auth: agent?.metaValue(forKey: "auth"))
            // NO CREDENTIAL FROM THE RECORD. Python builds the action from the
            // target and the payload alone (`gate_inbound(rule, "spawn",
            // {"target", "payload"})`) — a leg that stored `auth_token` on its own
            // record used to satisfy `password_ingress` with its own secret, which
            // is the leg letting itself in.
            let action = AuthAction(kind: "spawn", target: agentId.value, verb: verb, token: nil)
            // FORWARDED, not re-worded — the rule states the reason and the
            // repair; this hint was a second, shorter wording of the same
            // thing, so the same seal read differently per edge.
            if case .deny(let d) = rule.authorize(action) { return d.asRefusal }
            return nil
        }

        /// `monitor` — the job checks (python's order: the id, this agent's, alive),
        /// then `startMonitor` (`Monitor.swift`).
        private func monitorReply(_ agentId: AgentId, _ payload: JSON, kernel: Kernel) -> JSON {
            let jid: TaskId
            switch jobArg("monitor", payload, required: true) {
            case .failure(let r): return r.json
            case .success(let j): jid = j!
            }
            guard let job = lock.withLock({ jobs[jid] }), job.agent == agentId else {
                return noJob(jid)
            }
            if !job.child.isAlive {
                return Refusal.bad("monitor", "job_id", "a RUNNING job — this one has exited")
            }
            return startMonitor(agentId, jobId: jid, child: job.child, payload: payload, kernel: kernel)
        }

        /// Any live process of this owner.
        private func hasRunningJob(_ agentId: AgentId) -> Bool {
            lock.withLock {
                jobs.values.contains { $0.agent == agentId && $0.child.isAlive }
            }
        }

        /// `start` — spawn the COMMAND in the BACKGROUND and return at once.
        ///
        /// args: command:str (req — on a PTY start, absent = the child's `$SHELL`),
        /// args:list[str]? (argv, default []), pty:{cols,rows}?. The FULL line
        /// (command + args, shell-joined) must fullmatch one of the record's
        /// `allowed` regexes — or be the exact shell line the record's `shell`
        /// permits — or the start is refused naming the patterns. `cwd`/`env` are
        /// the RECORD's, never the call's.
        private func startReply(agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
            let record = kernel.agent(agentId)
            // EVERY refusal on this verb carries a CODE, so a model can tell a typo
            // from a policy refusal without reading the prose.
            // WHERE and WITH WHAT a command runs are the OPERATOR's: a per-call `env`
            // (`LD_PRELOAD`, `PATH`, …) or `cwd` changed what an allowlisted line
            // actually ran (audit 2026-09-28).
            for gone in ["env", "cwd"] where !payload[gone].isNull {
                return Refusal.bad(
                    "start", gone,
                    "absent — `\(gone)` is the record's (update_agent \(agentId.value) \(gone)=…)")
            }
            // A WAKE-UP is a monitor's, never the start's: a `notify` here used to be
            // dropped silently, and the caller waited for a message nothing would send.
            for gone in ["notify", "match", "every"] where !payload[gone].isNull {
                return Refusal.bad(
                    "start", gone,
                    "absent — `start` sends no one anything; to be told what the job "
                        + "prints (or when it ends), `monitor` it: job_id, notify, match, every")
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
            var dimsOut: (Int, Int)? = nil
            if !payload["pty"].isNull {
                switch dims("start", payload["pty"]) {
                case .failure(let r): return r.json
                case .success(let d): dimsOut = d
                }
            }
            // The child's WHOLE environment, from the record — built before the
            // command is resolved, because an absent `command` on a PTY start is
            // read from it. (python's order: args, pty, env, THEN command.)
            let env = childEnv(record)
            var rawCommand = payload["command"]
            if rawCommand.isNull, dimsOut != nil {
                // AUTODISCOVERY, not a default: the shell is whatever the record
                // passed down to the child as SHELL. No chain (zsh → bash → sh)
                // behind it — a child env without one is refused, naming where it
                // comes from.
                guard let shell = env[SHELL_VAR], !shell.isEmpty else {
                    return Refusal.make(
                        Refusal.missingArg,
                        "exec_bridge: start named no command and the child's environment "
                            + "has no \(SHELL_VAR) to run — a PTY start without `command` runs "
                            + "the shell the record passes down, never a guessed one",
                        hint: "update_agent \(agentId.value) inherit_env=[\"\(SHELL_VAR)\"] "
                            + "(or env={\"\(SHELL_VAR)\": …}), or name `command`",
                        extra: [("arg", .string(SHELL_VAR))])
                }
                rawCommand = .string(shell)
            }
            // A pipe job always names its command; so does a PTY start that passed
            // one explicitly (an empty string is a typo, not "use the shell").
            // python `command.strip()` — ALL whitespace, newlines included.
            guard let command = rawCommand.asString,
                !command.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            else {
                return Refusal.missing("start", "command")
            }
            // The FULL argv, shell-joined, matched BEFORE spawn. Two ways a line is
            // permitted, both written on the RECORD: the shell line its `shell`
            // field names, or a fullmatch against `allowed`. Checked in that order
            // only because the first is an exact comparison; neither widens the
            // other.
            let argv = [command] + args
            let line = shlexJoin(argv)
            if !shellPermits(record, env: env, argv: argv, pty: dimsOut != nil),
                var why = commandPermitted(record, line)
            {
                // An AUTHORIZATION denial, not a bad argument: the command line is
                // well-formed and the LEG refused it. Same code the ingress gate
                // answers, so one branch covers "the operator has not opened this".
                if let tail = shellTail(record) {
                    // Say what `shell` DOES permit, so a caller that passed extra args
                    // (or a pipe job) learns why the shell permission did not cover
                    // it. The value is JSON (`true`/`"login"`) — python's
                    // `json.dumps`, the wire's own spelling — so every runtime prints
                    // the same bytes.
                    let shell = env[SHELL_VAR].flatMap { $0.isEmpty ? nil : $0 } ?? "$" + SHELL_VAR
                    let shown = pyJsonDumps(record?.metaValue(forKey: "shell") ?? .null)
                    why +=
                        " (`shell=\(shown)` permits only the PTY line "
                        + "\(pyRepr(shlexJoin([shell] + tail))))"
                }
                if args.isEmpty
                    && command.split(whereSeparator: { $0.isWhitespace }).count > 1
                {
                    // A whole line in `command` is quoted as ONE word, so no pattern
                    // over `<program> <args>` can ever match it — say where the words go.
                    why += commandOneProgram
                }
                return Refusal.make(
                    Refusal.unauthorized, why,
                    hint: "the leg's `allowed` policy decides; an operator may widen it")
            }
            guard let cwdStr = record?.metaValue(forKey: "cwd")?.asString, !cwdStr.isEmpty else {
                return Refusal.make(
                    Refusal.missingArg,
                    "exec_bridge: this leg names no `cwd` — where a command runs is the "
                        + "operator's decision, never guessed",
                    hint: "update_agent \(agentId.value) cwd=<dir>", extra: [("arg", .string("cwd"))])
            }
            let cwd: JSON = .string(cwdStr)

            var spawned: Spawn.Spawned? = nil
            var ptyParts: (Child, Int32, Int32)? = nil
            do {
                if let (cols, rows) = dimsOut {
                    ptyParts = try Spawn.pty(
                        argv: argv, selfId: agentId.value, env: env, cwd: cwdStr, cols: cols,
                        rows: rows)
                } else {
                    spawned = try Spawn.job(argv: argv, selfId: agentId.value, env: env, cwd: cwdStr)
                }
            } catch is Spawn.NoPath {
                return Refusal.make(
                    Refusal.missingArg,
                    "exec_bridge: \(pyRepr(.string(command))) is a bare command and this leg "
                        + "gives the child no PATH to find it in — the kernel's own PATH is not "
                        + "the child's",
                    hint: "update_agent \(agentId.value) inherit_env=[\"PATH\"] "
                        + "(or env={\"PATH\": …}), or start an absolute path",
                    extra: [("arg", .string("PATH"))])
            } catch {
                // python's `str(OSError)`: `[Errno 2] No such file or directory: 'nope'`.
                // A MISSING `cwd` is named instead, as CPython names it (its
                // `noexec:chdir` path): measured 2026-10-06 (rust subai), a leg whose
                // cwd did not exist answered `… 'python3'` and the model spent twenty
                // turns hunting an interpreter that was there all along.
                var isDir: ObjCBool = false
                if let f = error as? Spawn.Failure, f.code == ENOENT,
                    !(FileManager.default.fileExists(atPath: cwdStr, isDirectory: &isDir)
                        && isDir.boolValue)
                {
                    return Refusal.make(
                        Refusal.transportError,
                        "exec_bridge: spawn failed: \(osErrorStr(ENOENT, cwdStr))")
                }
                return Refusal.make(
                    Refusal.transportError, "exec_bridge: spawn failed: \(error)")
            }
            let ctrlFd = spawned?.ctrlFd ?? ptyParts!.2
            let child = spawned?.child ?? ptyParts!.0
            let counter = LineCounter()

            // THE PUMP IS THE JOB'S TASK, and the job's id IS that task's id —
            // minted by the kernel's one counter when the pump is forked. One
            // number names the job everywhere: `status`/`stop`/`write` take it as
            // `job_id`, `ps` lists it, `cancel task_id=` kills it. (It used to be a
            // random 8-hex string, which a model's `<job_id>04203581</job_id>` or
            // `458e5912` coerced into an int or an infinite float — 3.7% of jobs
            // could not be addressed again.)
            //
            // The id exists only once the fork does, and a swift `Task` may start
            // running on another thread before `spawnTask` even returns — so the
            // pump waits on `ready` for its id (and, for a PTY job, the terminal
            // built around that id) and touches nothing until the job's row is
            // filed. A cancel that lands first ends the wait with nil: the pump
            // then does nothing, exactly as python's `await ready` raising would.
            let (ready, readyCont) = AsyncStream<PumpStart>.makeStream(
                bufferingPolicy: .bufferingNewest(1))
            let pipe = spawned
            let jobId = kernel.spawnTask(agent: agentId, verb: "start") { [weak self] in
                var it = ready.makeAsyncIterator()
                guard let start = await it.next() else { return }
                if let spawned = pipe {
                    await self?.pump(
                        agentId: agentId, jobId: start.jobId, spawned: spawned, counter: counter,
                        kernel: kernel)
                } else if let pty = start.pty {
                    await self?.ptyPump(
                        agentId: agentId, jobId: start.jobId, pty: pty, child: child,
                        kernel: kernel)
                }
            }
            let pty = ptyParts.map {
                PtyJob(
                    jobId: jobId, owner: agentId, child: $0.0, masterFd: $0.1,
                    cols: dimsOut!.0, rows: dimsOut!.1)
            }
            let relay = Relay(
                fd: ctrlFd, owner: agentId, jobId: jobId, kernel: kernel)
            lock.withLock {
                jobs[jobId] = Job(
                    agent: agentId, child: child,
                    relay: relay, command: command, args: args, cwd: cwd,
                    startedAt: Date().timeIntervalSince1970, counter: counter, pty: pty)
            }
            // THE RELAY IS THE JOB'S TASK TOO (py `spawn_task(id, "relay", …)`): a
            // program's `kernel.send` must be rowed under work that is still alive,
            // not under the `start` dispatch that settles the moment we return.
            // Forked AFTER the pump (so the ids read pump, relay — python's order)
            // but BEFORE the pump is released: a program that exits at once must
            // find a relay to stop.
            _ = kernel.spawnTask(agent: agentId, verb: "relay") { [relay] in
                await relay.run(taskId: KernelTaskLocals.currentTask)
            }
            readyCont.yield(PumpStart(jobId: jobId, pty: pty))
            readyCont.finish()
            // The RESOLVED invocation rides the announcement: a client that let the
            // kernel pick the shell learns which one ran, and can re-issue it
            // verbatim.
            await kernel.emit(
                agentId,
                .object([
                    "type": .string("job_started"), "job_id": .integer(jobId.value),
                    "command": .string(command), "args": .array(args.map(JSON.string)),
                ]))
            return .object([
                "job_id": .integer(jobId.value), "status": .string("running"),
                "pty": .bool(pty != nil),
            ])
        }

        /// What the pump waits for before it runs: its own id, known only once the
        /// fork exists, and — for a PTY job — the terminal built around that id.
        private struct PumpStart: Sendable {
            let jobId: TaskId
            let pty: PtyJob?
        }

        /// The job's task-table body: drain both pipes LIVE (a `progress` per line as
        /// it arrives), await exit, update the row, emit `job_done`. The row STAYS
        /// after exit so `status`/`clear` can see it (python/rust parity).
        private func pump(
            agentId: AgentId, jobId: TaskId, spawned: Spawn.Spawned, counter: LineCounter,
            kernel: Kernel
        ) async {
            // the job's monitors end with it, on every way out
            defer { monitorTable.jobEnded(agentId, jobId) }
            async let outD = pumpStream(
                spawned.stdout, stream: "stdout", agentId: agentId, jobId: jobId,
                counter: counter, kernel: kernel)
            async let errD = pumpStream(
                spawned.stderr, stream: "stderr", agentId: agentId, jobId: jobId,
                counter: counter, kernel: kernel)
            let o = await outD
            let e = await errD
            // A PUMP FAULT is one fault for the whole pump, python's `gather` shape:
            // the job's `error` (`[pump error] …`). The output itself lives in the
            // line ring only — the job's one copy (audit 2026-10-14).
            let fault = (o.fault ?? e.fault).map { "[pump error] \($0)" }
            let code = await spawned.child.wait()
            relayFor(jobId)?.stop()
            // CANCELLED: python's pump re-raises `CancelledError` after labelling the
            // row `stopped` — it emits NO `job_done`, so a killed job never looks like
            // a program that finished on its own.
            if takeCancelled(jobId) {
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
                    j.status =
                        j.status == "stopping"
                        ? "stopped" : (code == 0 ? "done" : "failed")
                    jobs[jobId] = j
                }
            }
            var done: OrderedDictionary<String, JSON> = [
                "type": .string("job_done"), "job_id": .integer(jobId.value),
                "exit_code": .integer(Int64(code)), "ok": .bool(code == 0),
                "lines": .integer(Int64(counter.lines)), "last_line": .string(counter.lastLine),
            ]
            if let fault { done["error"] = .string(fault) }
            await kernel.emit(agentId, .object(done))
        }

        /// THE JOB'S TASK for a PTY job (python `_pty_pump`): the terminal's own
        /// ordered stream carries every `output` chunk; at EOF the process is
        /// awaited, the row written, `job_done` emitted last.
        private func ptyPump(
            agentId: AgentId, jobId: TaskId, pty: PtyJob, child: Child, kernel: Kernel
        ) async {
            defer { monitorTable.jobEnded(agentId, jobId) }
            let table = monitorTable
            let code = await pty.run(kernel: kernel) { table.feedText(agentId, jobId, $0) }
            relayFor(jobId)?.stop()
            if takeCancelled(jobId) {
                lock.withLock {
                    if var j = jobs[jobId] {
                        j.status = "stopped"
                        j.exitCode = child.exitCode
                        jobs[jobId] = j
                    }
                }
                return
            }
            lock.withLock {
                if var j = jobs[jobId] {
                    j.exitCode = code
                    j.status = j.status == "stopping" ? "stopped" : (code == 0 ? "done" : "failed")
                    jobs[jobId] = j
                }
            }
            await kernel.emit(
                agentId,
                .object([
                    "type": .string("job_done"), "job_id": .integer(jobId.value),
                    "exit_code": .integer(Int64(code)), "ok": .bool(code == 0),
                ]))
        }

        /// Emit one `progress` per output line. The counter is SHARED across stdout
        /// and stderr and lives on the PUMP, not the table — python's `_read_stream`
        /// closes over the job dict.
        private func emitLine(
            _ line: String, stream: String, agentId: AgentId, jobId: TaskId,
            counter: LineCounter, kernel: Kernel
        ) async {
            let n = counter.bump(line, stream: stream)
            monitorTable.feedLine(agentId, jobId, line)
            await kernel.emit(
                agentId,
                .object([
                    "type": .string("progress"), "job_id": .integer(jobId.value),
                    "n": .integer(Int64(n)), "line": .string(line),
                    "stream": .string(stream),
                ]))
        }

        // MARK: - PTY jobs (a terminal is a job)

        /// A refusal carried through a `Result`.
        struct RefusalJSON: Error { let json: JSON }

        /// A live PTY job this agent owns, or the refusal (python `_pty_job`).
        private func ptyJob(_ verb: String, _ agentId: AgentId, _ payload: JSON)
            -> Result<PtyJob, RefusalJSON>
        {
            let jid: TaskId
            switch jobArg(verb, payload, required: true) {
            case .failure(let r): return .failure(r)
            case .success(let j): jid = j!  // required: absent is already refused
            }
            let found = lock.withLock { jobs[jid].flatMap { $0.agent == agentId ? $0 : nil } }
            guard let job = found else { return .failure(RefusalJSON(json: noJob(jid))) }
            guard let pty = job.pty else {
                return .failure(
                    RefusalJSON(json: Refusal.bad(verb, "job_id", "a PTY job (started with `pty`)")))
            }
            guard job.child.isAlive, pty.isLive else {
                return .failure(RefusalJSON(json: jobEnded(jid)))
            }
            return .success(pty)
        }

        /// The job ended — whether before the verb arrived or while it ran (a write
        /// or resize that lost the race to the terminal's close answers this too,
        /// never an errno from a descriptor it may no longer own).
        private func jobEnded(_ jid: TaskId) -> JSON {
            Refusal.make(
                Refusal.notFound, "exec_bridge: job \(jid.value) has ended",
                extra: [("job_id", .integer(jid.value))])
        }

        /// A positive int (python `_dims`: an int >= 1, a bool is not one).
        private func dim(_ verb: String, _ name: String, _ v: JSON) -> Result<Int, RefusalJSON> {
            if case .integer(let n) = v, n >= 1, n <= 65535 { return .success(Int(n)) }
            return .failure(RefusalJSON(json: Refusal.bad(verb, name, "a positive integer")))
        }

        /// `(cols, rows)` of a `pty` / `resize` object — both required, positive.
        private func dims(_ verb: String, _ raw: JSON) -> Result<(Int, Int), RefusalJSON> {
            guard case .object = raw else {
                return .failure(RefusalJSON(json: Refusal.bad(verb, "pty", "an object {cols, rows}")))
            }
            switch (dim(verb, "cols", raw["cols"]), dim(verb, "rows", raw["rows"])) {
            case (.failure(let r), _), (_, .failure(let r)): return .failure(r)
            case (.success(let c), .success(let r)): return .success((c, r))
            }
        }

        /// `write {job_id, data | bytes}` — `bytes` is written VERBATIM (a `<call>`
        /// strips a scalar like `data`, so a trailing Enter only survives as
        /// `bytes`); `raw` is the binary body when `bytes` rode the binary channel.
        private func writeReply(_ agentId: AgentId, _ payload: JSON, raw: Data? = nil) async -> JSON {
            let pty: PtyJob
            switch ptyJob("write", agentId, payload) {
            case .failure(let r): return r.json
            case .success(let p): pty = p
            }
            // `JSON` takes a nil literal, so presence is judged against `.null`
            // explicitly — never through an optional.
            let data = payload["data"]
            let bytesVal = payload["bytes"]
            let hasData = data != JSON.null
            let hasBytes = raw != nil || bytesVal != JSON.null
            if hasData == hasBytes {
                return Refusal.bad("write", "data", "exactly one of data (a string) or bytes (verbatim)")
            }
            let buf: Data
            if let raw {
                buf = raw
            } else if hasBytes {
                guard let s = bytesVal.asString else {
                    return Refusal.bad("write", "bytes", "bytes, or a string written verbatim")
                }
                buf = Data(s.utf8)
            } else {
                guard let s = data.asString else {
                    return Refusal.bad("write", "data", "a string")
                }
                buf = Data(s.utf8)
            }
            switch await pty.writeAll(buf) {
            case .success(let n): return .object(["written": .integer(Int64(n))])
            case .failure(.ended): return jobEnded(pty.jobId)
            case .failure(.os(let e)):
                return Refusal.make(
                    Refusal.transportError, "exec_bridge: write failed: \(osErrorStr(e))")
            }
        }

        private func resizeReply(_ agentId: AgentId, _ payload: JSON) -> JSON {
            let pty: PtyJob
            switch ptyJob("resize", agentId, payload) {
            case .failure(let r): return r.json
            case .success(let p): pty = p
            }
            switch dims("resize", payload) {
            case .failure(let r): return r.json
            case .success(let (cols, rows)):
                switch pty.resize(cols: cols, rows: rows) {
                case .success: break
                case .failure(.ended): return jobEnded(pty.jobId)
                case .failure(.os(let e)):
                    return Refusal.make(
                        Refusal.transportError, "exec_bridge: resize failed: \(osErrorStr(e))")
                }
                return .object([
                    "resized": .bool(true), "cols": .integer(Int64(cols)),
                    "rows": .integer(Int64(rows)),
                ])
            }
        }

        private func ackReply(_ agentId: AgentId, _ payload: JSON) -> JSON {
            let pty: PtyJob
            switch ptyJob("ack", agentId, payload) {
            case .failure(let r): return r.json
            case .success(let p): pty = p
            }
            guard case .integer(let chars) = payload["chars"], chars >= 0 else {
                return Refusal.bad("ack", "chars", "a non-negative integer")
            }
            let (unacked, paused) = pty.ack(Int(chars))
            return .object(["unacked": .integer(Int64(unacked)), "paused": .bool(paused)])
        }

        private func outputReply(_ agentId: AgentId, _ payload: JSON) -> JSON {
            let jid: TaskId
            switch jobArg("output", payload, required: true) {
            case .failure(let r): return r.json
            case .success(let j): jid = j!  // required: absent is already refused
            }
            let found = lock.withLock { jobs[jid].flatMap { $0.agent == agentId ? $0 : nil } }
            guard let job = found else { return noJob(jid) }
            guard let pty = job.pty else {
                return Refusal.bad(
                    "output", "job_id", "a PTY job (a pipe job's output is in `status`)")
            }
            var text = pty.ringText()
            let mb = payload["max_bytes"]
            if mb != .null {
                guard case .integer(let n) = mb, n >= 0 else {
                    return Refusal.bad("output", "max_bytes", "a non-negative integer")
                }
                let bytes = Array(text.utf8)
                if Int(n) < bytes.count {
                    // python slices the BYTES and decodes with errors="ignore": a char
                    // cut at the front is dropped, never shown as litter.
                    var start = bytes.count - Int(n)
                    while start < bytes.count && bytes[start] & 0xC0 == 0x80 { start += 1 }
                    text = String(decoding: bytes[start...], as: UTF8.self)
                }
            }
            return .object([
                "job_id": .integer(jid.value), "output": .string(text),
                "origin": .string(
                    "\(agentId.value) output {job_id: \(jid.value)} — the ring NOW; re-send, it moves"),
            ])
        }

        /// `tail` / `grep` — a job's output SO FAR (owner, 2026-10-11), running or
        /// ended, pipe or PTY; each line `{n, stream, line}`. Python `_tail` / `_grep`.
        private func tailGrepReply(_ verb: String, _ agentId: AgentId, _ payload: JSON) -> JSON {
            let jid: TaskId
            switch jobArg(verb, payload, required: true) {
            case .failure(let r): return r.json
            case .success(let j): jid = j!  // required: absent is already refused
            }
            var matcher: NSRegularExpression? = nil
            var pattern = ""
            if verb == "grep" {
                guard case .string(let s) = payload["match"], !s.isEmpty else {
                    return Refusal.bad("grep", "match", "a regex string")
                }
                do {
                    matcher = try NSRegularExpression(pattern: pyRegexSource(s))
                    pattern = s
                } catch {
                    return Refusal.bad("grep", "match", "a valid regex (\(error.localizedDescription))")
                }
            }
            var want = verb == "tail" ? 20 : 50
            if payload["lines"] != .null {
                guard case .integer(let n) = payload["lines"], n >= 1, n <= MAX_LINES else {
                    return Refusal.bad(verb, "lines", "an integer 1–\(MAX_LINES)")
                }
                want = Int(n)
            }
            var stream = "both"
            if payload["stream"] != .null {
                guard case .string(let s) = payload["stream"], ["stdout", "stderr", "both"].contains(s)
                else { return Refusal.bad(verb, "stream", "one of stdout, stderr, both") }
                stream = s
            }
            let found = lock.withLock { jobs[jid].flatMap { $0.agent == agentId ? $0 : nil } }
            guard let job = found else { return noJob(jid) }
            var rows: [(Int, String, String)]
            var dropped = 0
            if let pty = job.pty {
                rows = pySplitlines(pty.ringText()).enumerated().map { ($0.offset + 1, "pty", String($0.element)) }
            } else {
                (rows, dropped) = job.counter.snapshot()
                if stream != "both" { rows = rows.filter { $0.1 == stream } }
            }
            func row(_ r: (Int, String, String)) -> JSON {
                .object(["n": .integer(Int64(r.0)), "stream": .string(r.1), "line": .string(r.2)])
            }
            if verb == "tail" {
                return .object([
                    "job_id": .integer(jid.value), "status": .string(job.status),
                    "lines": .array(rows.suffix(want).map(row)),
                    "printed": .integer(Int64(job.pty == nil ? job.counter.lines : rows.count)),
                    "dropped": .integer(Int64(dropped)),
                    "origin": .string("\(agentId.value) tail {job_id: \(jid.value)} — the output NOW; re-send, it moves"),
                ])
            }
            let hits = rows.filter { r in
                let ns = r.2 as NSString
                return matcher!.firstMatch(in: r.2, range: NSRange(location: 0, length: ns.length)) != nil
            }
            return .object([
                "job_id": .integer(jid.value), "status": .string(job.status),
                "matches": .array(hits.suffix(want).map(row)),
                "total": .integer(Int64(hits.count)), "scanned": .integer(Int64(rows.count)),
                "dropped": .integer(Int64(dropped)),
                "origin": .string(
                    "\(agentId.value) grep {job_id: \(jid.value), match: \(pyRepr(pattern))} — the output NOW; re-send, it moves"),
            ])
        }

        /// `paste_image` with no image at `data` — `data` is bytes: on the text
        /// channel (or with a body under another key) there is none to paste. The
        /// job is checked first, as python's handler does.
        private func pasteImageText(_ agentId: AgentId, _ payload: JSON) -> JSON {
            switch ptyJob("paste_image", agentId, payload) {
            case .failure(let r): return r.json
            case .success: return Refusal.bad("paste_image", "data", "raw bytes")
            }
        }

        /// `paste_image` — the BINARY channel: the body at `data` IS the image.
        private func pasteImageReply(_ agentId: AgentId, _ header: JSON, _ data: Data) async -> JSON {
            let pty: PtyJob
            switch ptyJob("paste_image", agentId, header) {
            case .failure(let r): return r.json
            case .success(let p): pty = p
            }
            if data.count > MAX_PASTE_IMAGE {
                return Refusal.bad("paste_image", "data", "at most \(MAX_PASTE_IMAGE) bytes")
            }
            // The mime is the caller's to state — never guessed.
            guard let mimeRaw = header["mime"].asString, !mimeRaw.isEmpty else {
                return Refusal.missing("paste_image", "mime")
            }
            let mime = mimeRaw.lowercased()
            guard let ext = PTY_IMAGE_EXT.first(where: { $0.0 == mime })?.1 else {
                let legal = PTY_IMAGE_EXT.map { $0.0 }.sorted().map { pyRepr($0) }.joined(separator: ", ")
                return Refusal.bad("paste_image", "mime", "one of [\(legal)]")
            }
            guard let dir = pty.pasteDirectory() else {
                return Refusal.make(
                    Refusal.transportError, "exec_bridge: paste_image failed: \(osErrorStr(errno))")
            }
            let name = "paste_\(String(format: "%08x", UInt32.random(in: .min ... .max))).\(ext)"
            do {
                _ = try fsWriteChunk(dir, name, data, offset: 0, truncate: true, base: "/")
            } catch {
                return Refusal.make(
                    Refusal.transportError, "exec_bridge: paste_image failed: \(error.message)")
            }
            let path = (dir as NSString).appendingPathComponent(name)
            // the path at the cursor with a trailing space, no newline — pasting
            // must not submit
            switch await pty.writeAll(Data("\(path) ".utf8)) {
            case .success: break
            case .failure(.ended): return jobEnded(pty.jobId)
            case .failure(.os(let e)):
                return Refusal.make(
                    Refusal.transportError, "exec_bridge: paste_image failed: \(osErrorStr(e))")
            }
            return .object(["path": .string(path), "bytes": .integer(Int64(data.count))])
        }

        /// The BINARY channel: `paste_image` reads ITS key — the body at `data`
        /// (python's `payload.get("data")` is the bytes). A body under any other key
        /// is not the image: python's `data` is then absent, `bad_arg` "raw bytes".
        /// Every other verb takes the text path.
        public func handleBinary(
            agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
        ) async throws -> (JSON?, Blob?) {
            let verb = header["type"].asString
            guard verb == "paste_image" || verb == "write" else {
                return (try await handle(agentId: agentId, payload: header, kernel: kernel), nil)
            }
            if let agent = kernel.agent(agentId) {
                let rule = resolveIngress(
                    ingressRule: agent.metaValue(forKey: "ingress_rule"),
                    auth: agent.metaValue(forKey: "auth"))
                if case .deny(let d) = rule.authorize(
                    AuthAction(kind: "spawn", target: agentId.value, verb: verb ?? "", token: nil))
                {
                    return (d.asRefusal, nil)
                }
            }
            if verb == "write" {
                return (await writeReply(agentId, header, raw: bodyAt(body, "bytes")), nil)
            }
            guard let data = bodyAt(body, "data") else {
                return (pasteImageText(agentId, header), nil)
            }
            return (await pasteImageReply(agentId, header, data), nil)
        }

        /// The child's WHOLE environment, from the record (python `_child_env`): the
        /// kernel env vars named in `inherit_env` (nothing by default; `["*"]` = every
        /// one — the operator's wildcard), then the operator's `env` extras. Nothing
        /// else leaks in — the kernel's `.env` keys reach a child only by name.
        private func childEnv(_ record: Agent?) -> [String: String] {
            let names = (record?.metaValue(forKey: "inherit_env")?.asArray ?? []).compactMap {
                $0.asString
            }
            let host = ProcessInfo.processInfo.environment
            var env: [String: String] = [:]
            if names.contains("*") {
                env = host
            } else {
                for n in names { if let v = host[n] { env[n] = v } }
            }
            for (k, v) in record?.metaValue(forKey: "env")?.asObject ?? [:] { env[k] = pyStr(v) }
            return env
        }

        // MARK: - status / clear

        private func snap(_ jobId: TaskId, _ j: Job, full: Bool) -> JSON {
            var o: OrderedDictionary<String, JSON> = [
                "job_id": .integer(jobId.value),
                "status": .string(j.status),
                "exit_code": j.exitCode.map { JSON.integer(Int64($0)) } ?? .null,
                "started_at": .double(j.startedAt),
                "lines": .integer(Int64(j.counter.lines)),
                "last_line": .string(j.counter.lastLine),
                "command": .string(j.command),
                "args": .array(j.args.map(JSON.string)),
                "pty": .bool(j.pty != nil),
            ]
            if let p = j.pty {
                o["cols"] = .integer(Int64(p.cols))
                o["rows"] = .integer(Int64(p.rows))
            }
            if full {
                o["cwd"] = j.cwd
            }
            return .object(o)
        }

        /// The `job_id` argument (python `_job_arg`): `.success(id)`,
        /// `.success(nil)` when absent and optional, or the refusal.
        ///
        /// A job's id IS its kernel TASK id — an int. Anything else is refused as a
        /// bad argument naming that, never looked up: a string like "42" is not
        /// coerced, so a caller that stringified an id learns so here instead of
        /// reading `no job`. A bool is not an int (python's `isinstance` check
        /// excludes it explicitly; here `.bool` is its own case).
        private func jobArg(_ verb: String, _ payload: JSON, required: Bool)
            -> Result<TaskId?, RefusalJSON>
        {
            switch payload["job_id"] {
            case .null:
                return required
                    ? .failure(RefusalJSON(json: Refusal.missing(verb, "job_id"))) : .success(nil)
            case .integer(let n):
                return .success(TaskId(n))
            default:
                return .failure(
                    RefusalJSON(
                        json: Refusal.bad(
                            verb, "job_id",
                            "an int — the job's task id (from `start`, `status` or `ps`)")))
            }
        }

        /// `status` — one job (full output) or all of THIS agent's jobs (summary).
        private func statusReply(_ agentId: AgentId, payload: JSON) -> JSON {
            let want: TaskId?
            switch jobArg("status", payload, required: false) {
            case .failure(let r): return r.json
            case .success(let j): want = j
            }
            return lock.withLock {
                if let jid = want {
                    guard let j = jobs[jid], j.agent == agentId else { return noJob(jid) }
                    return .object([
                        "jobs": .array([snap(jid, j, full: true)]),
                        // HOW TO GET THIS AGAIN — python `_status`'s `origin`.
                        "origin": .string(
                            "\(agentId.value) status {job_id: \(jid.value)} — a live job; re-send "
                                + "for its output NOW, which may differ"),
                    ])
                }
                let rows = jobs.filter { $0.value.agent == agentId }
                    .map { snap($0.key, $0.value, full: false) }
                return .object([
                    "jobs": .array(rows),
                    "origin": .string("\(agentId.value) status — the job table NOW; re-send, it changes"),
                ])
            }
        }

        /// `clear` — drop finished rows of this agent (one, or all non-running). The
        /// ONE caller that wants the
        /// status LABEL rather than process liveness: it is deciding what is
        /// finished enough to forget. No `not_found` guard (python has none): an
        /// unknown id simply clears nothing.
        ///
        /// A NAMED job follows the bulk form's rule (python `_clear`, audit BV-5):
        /// a row still `running` or `stopping` is refused. That row is the only
        /// handle `stop`, `cancel`, delete and shutdown kill its process through —
        /// dropped, the process outlived all four.
        private func clearReply(_ agentId: AgentId, payload: JSON) -> JSON {
            let want: TaskId?
            switch jobArg("clear", payload, required: false) {
            case .failure(let r): return r.json
            case .success(let j): want = j
            }
            if let jid = want, let job = lock.withLock({ jobs[jid] }), job.agent == agentId {
                if job.status == "running" || job.status == "stopping" {
                    return Refusal.bad(
                        "clear", "job_id",
                        "a finished job (job \(jid) is \(job.status) — `stop` it, then clear)")
                }
            }
            let doomed: [TaskId] = lock.withLock {
                jobs.filter { (id, j) in
                    j.agent == agentId && (want.map { $0 == id } ?? true)
                        && j.status != "running" && j.status != "stopping"
                }.map { $0.key }
            }
            lock.withLock { for id in doomed { jobs.removeValue(forKey: id) } }
            return .object(["cleared": .integer(Int64(doomed.count))])
        }

        /// The `not_found` refusal for a job_id naming nothing. Spelled once; the id
        /// is echoed both in the text (python's repr of an int is its digits) and in
        /// the `job_id` field.
        private func noJob(_ jid: TaskId) -> JSON {
            Refusal.make(
                Refusal.notFound, "exec_bridge: no job \(jid.value)",
                extra: [("job_id", .integer(jid.value))])
        }

        /// …and the guard that decides. A `job_id` naming nothing used to answer
        /// `{killed:0}` / `{interrupted:0}` — the same reply as "it had already
        /// exited", so a typo read as success. `nil` = no job named ("every job").
        /// Python `_no_job`.
        private func noSuchJob(_ verb: String, _ agentId: AgentId, _ want: TaskId?) -> JSON? {
            guard let want else { return nil }
            let job = lock.withLock { jobs[want].flatMap { $0.agent == agentId ? $0 : nil } }
            let known = job != nil
            // NOT a ternary: `JSON` is `ExpressibleByNilLiteral`, so
            // `known ? nil : noJob(want)` unifies BOTH branches at `JSON` and the
            // "known" branch hands back `.some(.null)` — the caller's
            // `if let gone = noSuchJob(…)` then fired for every job that DOES exist
            // and answered `null` instead of stopping it (measured on the wire).
            if known { return nil }
            return noJob(want)
        }

        /// `interrupt` / `stop` — signal this agent's jobs, or the one named.
        /// Returns how many were actually signalled.
        private func signalJobs(_ agentId: AgentId, _ want: TaskId?, _ sig: Int32) -> Int {
            return lock.withLock {
                var n = 0
                for (id, var j) in jobs {
                    if j.agent != agentId || (want.map { $0 != id } ?? false) {
                        continue
                    }
                    if !j.child.isAlive { continue }  // already gone — never pretend
                    if sig == SIGKILL {
                        j.status = "stopping"
                        jobs[id] = j
                        if Self.kill(j) { n += 1 }
                    } else if j.child.signal(sig) {
                        n += 1
                    }
                }
                return n
            }
        }

        // MARK: - lifecycle

        /// The relay serving a given job, if it is still registered.
        private func relayFor(_ jobId: TaskId) -> Relay? {
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
            // A job's id IS its task id, so the table key is what matches.
            // Marked BEFORE the kill (see `cancelling`), in the same critical section
            // that finds them.
            let doomed = lock.withLock { () -> [TaskId] in
                let ids = jobs.filter { $0.value.agent == agentId && $0.key == taskId }.map(\.key)
                // a pump that already finished takes no mark (nothing would clear it)
                cancelling.formUnion(ids.filter { jobs[$0]?.exitCode == nil })
                return ids
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
            _ handle: FileHandle, stream: String, agentId: AgentId, jobId: TaskId,
            counter: LineCounter, kernel: Kernel
        ) async -> PumpOut {
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
                let (ls, overrun) = carry.push(chunk)
                for line in ls { cont.yield(line) }
                if overrun {
                    // python's `StreamReader.readline` raises
                    // `ValueError("Separator is not found, and chunk exceed the limit")`
                    // once its 64 KiB buffer holds no newline. The pump catches it and
                    // BOTH reads are lost.
                    cont.finish()
                }
            }
            for await line in lines {
                await emitLine(
                    line, stream: stream, agentId: agentId, jobId: jobId, counter: counter,
                    kernel: kernel)
            }
            if carry.overran {
                return PumpOut(fault: "Separator is not found, and chunk exceed the limit")
            }
            return PumpOut(fault: nil)
        }

        /// Free the REAL resource of ONE job: SIGKILL its child + stop its relay, and
        /// LABEL the row `stopping` so the pump finalises it to `stopped`.
        ///
        /// The label is not cosmetic. Without it a cancelled job finalised as
        /// `failed` — the SIGKILL's non-zero status, indistinguishable from a program
        /// that failed on its own — where python and rust both report `stopped`.
        /// Idempotent; never signals a pid it no longer owns.
        private func teardown(_ jobId: TaskId) {
            let job = lock.withLock { () -> Job? in
                guard var j = jobs[jobId] else { return nil }
                if j.exitCode == nil {
                    j.status = "stopping"
                    jobs[jobId] = j
                }
                return j
            }
            guard let job else { return }
            if job.child.isAlive { Self.kill(job) }
            job.relay?.stop()
        }

        /// SIGKILL one job: its whole SESSION — the command and everything it
        /// launched (a pipe job's `server &`, a PTY shell's background jobs). Every
        /// job leads its own session (`Spawn`), so this never reaches the kernel.
        /// Returns whether the job's own process was signalled.
        @discardableResult
        private static func kill(_ j: Job) -> Bool {
            Spawn.killSession(j.child.pid)
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
            // python names the SPEC AS WRITTEN (`{spec!r}`), not the stem it
            // resolved to — the operator has to see what they typed.
            if let bad = IngressRules.unknownRefusal(
                meta, legal: ["allow_all_ingress", "deny_ingress"])
            {
                return bad
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
            // WHERE and WITH WHAT a command runs is the record's — shaped at the write.
            if let c = meta["cwd"], !c.isNull, (c.asString ?? "").isEmpty {
                return "cwd must be a non-empty directory path"
            }
            if let e = meta["env"], !e.isNull {
                let ok =
                    e.asObject?.values.allSatisfy {
                        switch $0 {
                        case .string, .integer, .double, .bool: return true
                        default: return false
                        }
                    } ?? false
                if !ok { return "env must be a map of NAME -> value" }
            }
            if let n = meta["inherit_env"], !n.isNull {
                let ok = n.asArray?.allSatisfy { !($0.asString ?? "").isEmpty } ?? false
                if !ok { return #"inherit_env must be a list of env var names (["*"] = all)"# }
            }
            // `shell` names ONE exact line, so its legal set is closed: the JSON bool
            // (true → `$SHELL`, false → none) or "login" (`$SHELL -l`). A number is
            // not a bool here — python checks `isinstance(v, bool)`, so `1` refuses.
            if let v = meta["shell"], !v.isNull {
                let ok: Bool
                switch v {
                case .bool: ok = true
                case .string(let s): ok = s == "login"
                default: ok = false
                }
                if !ok {
                    return "shell=\(pyRepr(v)) is not a shell permission — legal values: true "
                        + #"(the child's $SHELL, bare), "login" ($SHELL -l), false (none)"#
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

        /// python's `readme.md`, verbatim, with the trailing newline.
        static let readmeText = """
            # exec_bridge — process edge

            Spawns a COMMAND in the BACKGROUND and hands it a kernel wire. Crossing kernel↔process is
            an IO edge, so this leg is gated exactly like `file_bridge`/`web_ws`: **sealed by default**,
            and the commands it may run come from an operator **allowlist** of regexes.

            It is a GENERAL-PURPOSE command runner. A program is a FILE some process (or a `write_stream`) put on disk;
            running it is `command` + `args`, exactly like anything else.

            Verbs: `start` · `status` · `stop` · `interrupt` · `clear` · `tail` · `grep` · `monitor` ·
            `monitors` · `unmonitor` · `reflect` · `boot`, and for a PTY job `write` · `resize` · `ack` ·
            `output` · `paste_image`.
            Any verb NOT in that list is **MAIL** for the running program (see below).

            WHERE and WITH WHAT a command runs is the RECORD's, never the call's: `cwd` (REQUIRED — a
            leg without one runs nothing), `env` (the operator's extra vars), `inherit_env` (which of
            the kernel's env vars pass through — none by default, `["*"]` = all; the kernel's `.env`
            keys reach a child only when named). A per-call `env`/`cwd` is refused. A bare `command`
            is found on the CHILD's `PATH` only — pass `PATH` through `inherit_env`/`env`, or start an
            absolute path; with no `PATH` the start is refused naming it.

            - `start(command, args?, pty?)` → `{job_id, status:'running', pty}` at once (`job_id` is an int) — non-blocking,
              many jobs in parallel. `command` is the program/binary; `args` is its argv (a list of
              strings). The FULL line (`command` + `args`, shell-joined) must fullmatch one of the
              record's `allowed` regexes or the start is refused, naming the patterns. There is no
              blocking run-and-wait verb: a pipe job's output streams as `progress` events on this
              agent's inbox (one per line, `{n, line, stream}`) with a final `job_done`
              (`{exit_code, ok, lines, last_line}`), after a `job_started`. The output itself is
              read with `tail`/`grep` — the job keeps its last 256 KiB of lines, nothing more.
            - **A terminal is a PTY job**: `start(command, args, pty={cols, rows})` runs the command on
              a pseudo-terminal (`TERM=xterm-256color`), same gate. **Leave `command` out** and it runs
              the CHILD's own `$SHELL` — read from the environment the record passes down
              (`inherit_env`/`env`), never guessed; a child env with no `SHELL` is refused naming it.
              `job_started` and `status` report the resolved `command`. It streams `output {job_id, data}`
              chunks and ends with `job_done {job_id, exit_code, ok}`. Drive it with
              `write(job_id, data)` (keystrokes, written in full, serialized), `resize(job_id, cols,
              rows)`, `ack(job_id, chars)` (flow control — the reader pauses past 100K unacked chars,
              resumes under 5K), `output(job_id, max_bytes?)` (the 256 KiB ring — replay it when you
              re-attach) and `paste_image(job_id, data, mime)` (saves the image, types its path).
              Each is a gated verb: a viewer granted only `status`/`output`/`ack` cannot type.
              Any caller the gate admits drives any job of this agent. A running
              program's `on_message` never gets a drive verb whose `job_id` names another job —
              another job's keystrokes, pastes and drive calls never reach it; a bulk call (no
              `job_id`) does.
              Nothing respawns a terminal — `boot` starts nothing; a client that attaches and finds no
              live PTY job `start`s one.
            - `status(job_id?)` → `{jobs:[…]}` — one job with its `cwd` (a PTY job: its
              `cols`/`rows`), or every job of this agent as summaries.
            - `stop(job_id?)` SIGKILL the job's whole session — the command and everything it
              launched (`server &`, a `nohup`, a shell's background jobs); deleting the agent or
              stopping the kernel does the same. A process that leaves the session on purpose
              (`setsid`) escapes — containment beyond that is the OS's ·
              `interrupt(job_id?)` SIGINT · `clear(job_id?)` forget finished rows. Omit `job_id` and
              it applies to every job on this agent.
            - `boot` — no-op. Nothing auto-restarts: a command is started by a caller, not by the record.

            Every running job IS a row in the kernel TASK TABLE: its `job_id` is that row's id (an int
            from the kernel's one counter), listed by the universal `ps`. `cancel task_id=<job_id>` kills
            it — the OS process is SIGKILLed before the task is aborted, so nothing is orphaned.
            `delete_agent` tears its jobs down too.

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

            **The child's own shell — `shell`.** A client that cannot name the shell (a browser page
            has no environment) cannot write an exact `allowed` line for it either. The record's `shell`
            field permits the resolved shell line itself, and nothing wider:

            ```
            update_agent <id> inherit_env='["SHELL", "PATH"]' shell=true     # PTY start → exactly $SHELL
            update_agent <id> inherit_env='["SHELL", "PATH"]' shell=login    # PTY start → exactly $SHELL -l
            ```

            It is ONE exact line on a PTY job, not a pattern: other args, and every pipe job, still need
            an `allowed` match. A shell on a terminal runs whatever is typed into it — `shell` states which
            program `start` spawns, not a limit on what that program can do.

            **Know what a `.*` tail grants.** `*` is allowed here like on every gate — whoever writes
            it owns it. But a pattern names a COMMAND, and a command can run others:
            - an interpreter with `-c`/`-e` (`python3 -c .*`, `node -e .*`), or any shell
              (`bash .*`, `zsh`), runs ANY code;
            - `find … -exec`, `git -c …` / git hooks / aliases, `xargs`, `env` run ANY command.
            Grant a fixed argv, or a program file, when the caller should get less than that.

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

            ## Look at a job's output NOW — `tail`, `grep`

            A job's output is read, running or ended, with `tail(job_id, lines?, stream?)` — the last lines printed so far — and
            `grep(job_id, match, lines?, stream?)` — the lines so far matching a regex (the last
            `lines` of them). Both work on any job, running or ended, pipe or PTY; each line comes as
            `{n, stream, line}`. The job keeps its last 256 KiB of lines; older ones are gone and
            `dropped` counts them. `stream` is `stdout`, `stderr` or `both` (the default).

            ## Monitor a job — early stop

            `monitor(job_id, notify, match?, every?, tail?, label?)` watches a RUNNING job's output as it
            arrives and SENDS what matches to `notify` (an agent — usually the AI that asked) as a turn.
            It polls nothing: an idle monitor wakes never. It is how a long run is stopped EARLY.

            - `match`: a regex; each output line that matches is delivered. Absent = every line — then
              `every` is required.
            - `every`: seconds. The first delivery goes at once; later matches are gathered and delivered
              at most once per window. `tail`: the newest lines per delivery (default 100; older ones are
              counted as dropped).
            - A delivery reads `[monitor <id> · job <id> · from your call <pid> · "<label>"]` with the
              lines below. The listener decides: show them, and stop the job early with kernel `cancel`
              (`task_id` = its `job_id`).
            - A monitor is a kernel task: `ps` lists it, `monitors` lists this agent's, kernel `cancel` (or
              `unmonitor`) ends it, and it ends with its job after delivering what still waited. Monitors
              on different jobs, at different cadences, run in parallel.

            Deliveries are this agent's own calls: the listener's `callable_by` must name this agent, and
            this agent's `can_call` the listener. A delivery the listener REFUSES (no grant, no such agent)
            ends the monitor: `monitors` keeps it with `status: "dead"` and the refusal as `error`, nothing
            more is sent, and `unmonitor` clears it. A source that does not push (a URL, a remote status) gets
            its cadence from the `scheduler`: a schedule fires a one-shot check job every
            `interval_seconds`.

            ## Mail — `{queued:true}`

            Send a running program a message by sending THIS agent any verb the bundle does not own; it is
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
            body]`, the header's `_binary_path` naming where the body belongs — a JSON array of
            steps from the frame root, a key as a string (taken literally: `["payload", "a.b"]` is
            the key `"a.b"`, never `a` → `b`), a list index as an int — so a `read_stream` chunk or
            a `write_stream` payload crosses raw (never base64). Child → parent, the six
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
            // python `rec.get("cwd")` — the record's, as written; null when unset.
            let cwd = record?.metaValue(forKey: "cwd") ?? .null
            var out: OrderedDictionary<String, JSON> = [
                "id": .string(agentId.value),
                "sentence": .string(
                    "Process edge of the io family — a general-purpose COMMAND runner, sealed by default, gated by the `allowed` regexes. Spawns commands in the background; a child may speak the kernel over a private control fd."
                ),
                "cwd": cwd,
                "allowed": .array(allowedPatterns(record).map(JSON.string)),
                "inherit_env": record?.metaValue(forKey: "inherit_env") ?? .array([]),
                // The shell line the record permits without an `allowed` entry:
                // false (none), true ($SHELL bare) or "login" ($SHELL -l) — python
                // `rec.get("shell") or False`.
                "shell": record?.metaValue(forKey: "shell").flatMap { pyTruthy($0) ? $0 : nil }
                    ?? .bool(false),
                "running": .integer(Int64(running)),
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
        let fault: String?
    }

    /// The job's output-line counter, held by the PUMP rather than by the job row.
    ///
    /// python's `_read_stream` closes over the job DICT (A4.15). Shared across
    /// stdout and stderr, so `n` is one monotonic sequence.
    /// A pipe job's live line ring, in bytes — the same bound as the PTY ring.
    let LINE_RING = 256 * 1024
    /// The most lines one `tail` / `grep` reply carries.
    let MAX_LINES: Int64 = 500

    final class LineCounter: @unchecked Sendable {
        private let lock = NSLock()
        private var count = 0
        private var last = ""

        /// THE LIVE LINE RING (owner, 2026-10-11): every line a pipe job prints, as
        /// (n, stream, line), the last `LINE_RING` bytes — what `tail`/`grep` read,
        /// running or ended. The job's only copy of its output.
        private var ring: [(Int, String, String)] = []
        private var ringHead = 0
        private var ringBytes = 0
        private var dropped = 0

        var lines: Int { lock.withLock { count } }
        var lastLine: String { lock.withLock { last } }

        func bump(_ line: String, stream: String) -> Int {
            lock.withLock {
                count += 1
                last = line
                ring.append((count, stream, line))
                ringBytes += line.utf8.count + 1
                while ringBytes > LINE_RING && ringHead < ring.count {
                    ringBytes -= ring[ringHead].2.utf8.count + 1
                    ringHead += 1
                    dropped += 1
                }
                if ringHead > 4096 {
                    ring.removeFirst(ringHead)
                    ringHead = 0
                }
                return count
            }
        }

        /// The ring NOW, oldest first, and how many lines it already dropped.
        func snapshot() -> ([(Int, String, String)], Int) {
            lock.withLock { (Array(ring[ringHead...]), dropped) }
        }
    }

    /// The partial line held between pipe reads. A chunk boundary falls wherever the
    /// OS put it, never on a newline, so the tail of every chunk waits here for the
    /// rest of its line. `[UInt8]` rather than `Data` because array indices are always
    /// zero-based — a sliced `Data`'s are not, which is a quiet way to read the wrong
    /// bytes back out.
    private final class CarryBox: @unchecked Sendable {
        /// python's `asyncio.StreamReader` limit — 16 MiB since 2026-09-20 (a
        /// line over 64 KiB used to end the pipe with `[pump error]`); past it
        /// with no separator in sight the read RAISES rather than buffering
        /// without bound.
        static let limit = 16 * 1024 * 1024

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

#endif  // os(macOS)
