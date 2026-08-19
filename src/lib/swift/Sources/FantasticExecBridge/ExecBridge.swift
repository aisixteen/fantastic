// Python subprocess execution bundle (macOS only).
//
// Mirrors python's `python_runtime`: `start` spawns `python -u -c <connector +
// code>` as a BACKGROUND child and returns `{job_id, status:"running"}` AT ONCE.
// The program's output is NOT the reply — it arrives as `job_done` on this
// agent's inbox. The `code` is persisted on the agent record, so the program is
// a DURABLE tool that `boot` revives after a restart, and any verb this bundle
// does not own is MAIL for the running program. There is no run-and-wait verb
// (`exec` is GONE — a second spawn path could never reach a live job's
// `on_message`). The iOS sandbox forbids subprocess, so this is macOS-only.
//
// ## The child is a JOB, and the job is a kernel TASK
//
// A job OUTLIVES the dispatch that started it — `start` replies the moment the
// child is up — so the DISPATCH's row cannot stand for it. Each job takes its
// OWN row in the kernel task table (`ps` shows `{agent:<this>, verb:"start"}`
// for as long as the child lives), and cancelling that row must reach the real
// OS process.
//
// The handle that makes it reachable is the JOB TABLE below (`jobs`): the live
// `Process` of every in-flight job, keyed by the TASK it belongs to. It is what
// makes `cancel` real — cancelling a Swift `Task` only flips a cooperative flag;
// the `python3` child would keep running, parentless and unreachable (the ORPHAN
// bug that leaked 33 connector processes in this repo, and that rust hit with
// `kill_on_drop(false)`). `onCancel` — the substrate's SIGTERM half, awaited
// BEFORE the task is aborted — reaches into the table and SIGKILLs the child.
// One `teardown`, shared by every death: cancel, delete, and shutdown.
//
// `stop` (SIGKILL) / `interrupt` (SIGINT) stay what they are in rust: verbs
// that signal the PROGRAM, agent-scoped, addressed by the caller. Cancelling a
// CALL is the kernel's job (`ps` → `cancel`); signalling a program is the
// bundle's.
//
// The wait is asynchronous: the kernel's dispatch never sits on a blocking
// `waitUntilExit()` (that pinned a cooperative-pool thread AND could not be
// interrupted). The child's exit arrives through `terminationHandler`, its
// pipes drain concurrently through `readabilityHandler` (so a chatty script
// can't dead-lock on a full 64K pipe buffer), and the handler simply `await`s.
//
// ## Which interpreter — and a deliberate divergence from canonical
//
// `payload.python` → `payload.venv` → `record.python` → `record.venv` → REFUSE.
// Python's ladder ends in `sys.executable`; this one ends in an error naming both
// fields, because a Swift kernel is not a python process and holds no such fact,
// and "by default no defaults" forbids guessing a path in its place. The rule and
// the reasoning live in `Interpreter.swift`; the refusal is advertised in this
// bundle's readme and in `reflect`'s sentence.

// ## SECURITY — DEFERRED, AND A KNOWN HOLE (read before trusting the sandbox claim)
//
// An authored python program is **NOT sandboxed**. `python -u -c <code>` is a full CPython
// with the entire stdlib: `open()`, `socket`, `subprocess` all work. PROVEN 2026-08-05 — a
// job did `open('ESCAPED.txt','w')` and the file appeared on disk.
//
// So the self-extension doc's "your code's only capability is the kernel connector; it has
// no direct disk, net, or process access" is TRUE for `js_runtime` (no fs bindings exist in
// the engine) and TRUE for `lua_runtime` (loads only STRING|TABLE|MATH|COROUTINE, `io`/`os`
// omitted) — and FALSE here. The connector is the RECOMMENDED path, not the only one, and
// routing this bundle's data IO "through the loader" would be a convention a program can
// ignore in one line, not a gate.
//
// CONSEQUENCE FOR THE OPERATOR: registering a python executor GRANTS ARBITRARY LOCAL CODE
// EXECUTION to whatever authors a program — including an LLM. It is a conscious opt-in, and
// the risk is the operator's, deliberately (rust gates it behind the `full` feature; swift
// behind `#if os(macOS)`).
//
// RESOLUTION IS DEFERRED. Making the claim true needs an OS mechanism — `sandbox-exec`, a
// separate uid, or the container image — not anything reachable in-process. Until then this
// is understood and accepted, not overlooked.
//
#if os(macOS)

    import FantasticIoBridge
import FantasticIoBridge
import FantasticJSON
    import FantasticKernel
    import Foundation

    public let HANDLER_MODULE = "exec_bridge.tools"

    public final class ExecBridgeBundle: AgentBundle, @unchecked Sendable {
        public let name = "exec_bridge"
        public init() {}

        // MARK: - the job table

        /// One in-flight child — the REAL resource this bundle owns.
        ///
        /// `taskId` is the row in the kernel's task table that the child belongs
        /// to (the job's own row, spawned by `start`). `onCancel` matches on it, so
        /// killing one job never touches another running concurrently on the same
        /// agent. `pid` is captured at spawn: it is what we signal, and it
        /// stays valid to compare even after `Process` has reaped the child.
        private struct Job {
            let agent: AgentId
            let taskId: TaskId?
            let process: Process
            let pid: Int32
            /// A live job owns its control relay and its public job id.
            var jobId: String? = nil
            var relay: Relay? = nil
            var status: String = "running"
            var exitCode: Int32? = nil
        }

        /// Guards `jobs` + `nextKey`. Never held across an `await` (only around
        /// dictionary work + the `kill(2)` calls, which do not block).
        private let lock = NSLock()
        /// Live children, keyed by an internal handle (a job id). Per-INSTANCE, not
        /// a global: one bundle instance is registered per kernel, so two kernels in
        /// one process (the test suite) cannot see each other's children.
        private var jobs: [UInt64: Job] = [:]
        private var nextKey: UInt64 = 0

        public var readme: String? {
            """
            exec_bridge — the process edge of the io family. Sealed by default \
            (`ingress_rule=allow_all` opens it); interpreter ALLOWLIST \
            (`interpreters=[…]` on the record — no default, no PATH fallback; an \
            unlisted `interpreter` refuses naming the legal set). `start {code?, \
            args?, interpreter?, shim?}` spawns in the BACKGROUND → {job_id, \
            status:"running"} at once. The record's persisted `code` is a durable \
            SCRIPT run top to bottom, revived by `boot` (whose error propagates): `def run(payload): ...` and nothing else defines `run`, exits, and does no work — put the work at module level, and to receive later sends keep running via kernel.on_message(callback). \
            The bridge publishes a WIRE: the inherited FANTASTIC_CTRL_FD, framed \
            [4-byte BE length | JSON]. `shim:"python"` prepends the wire client (a \
            `kernel` global: send/emit/reflect/watch/on_message + self_id + args); \
            name no shim and the program speaks the wire itself. Any verb this \
            bundle does not own is MAIL for the running program ({queued:true}); \
            jobs are kernel TASKS (ps/cancel reap the real OS process). NOT a \
            sandbox: an allowlisted interpreter is a NAMED interpreter with all of \
            that interpreter's reach.
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
            // deliberately ungated too (py parity — the fanout already delivered).
            let owned = ["start", "boot", "shutdown", "status", "clear", "interrupt", "stop"]
            if owned.contains(verb), let agent = kernel.agent(agentId) {
                let rule = try resolveIngress(
                    ingressRule: agent.metaValue(forKey: "ingress_rule"),
                    auth: agent.metaValue(forKey: "auth"))
                let action = AuthAction(
                    kind: "spawn", target: agentId.value, verb: verb,
                    token: agent.metaValue(forKey: "auth_token")?.asString)
                if case .deny(let reason) = rule.authorize(action) {
                    return .object([
                        "error": .string(reason),
                        "reason": .string("unauthorized"),
                        "hint": .string(
                            "the process edge is sealed; open it: "
                                + "update_agent <id> ingress_rule=allow_all"),
                    ])
                }
            }
            switch verb {
            case "reflect":
                return reflectReply(agentId, kernel: kernel)
            case "start":
                return await startReply(agentId: agentId, payload: payload, kernel: kernel)
            case "boot":
                return await bootReply(agentId: agentId, kernel: kernel)
            case "shutdown":
                return .object(["ok": .bool(true)])
            case "interrupt":
                return .object(["interrupted": .integer(Int64(signalAll(agentId, SIGINT)))])
            case "stop":
                return .object(["killed": .integer(Int64(signalAll(agentId, SIGKILL)))])
            default:
                // ADDRESSABLE INBOX. A verb this bundle does not own is MAIL for the
                // running program — the substrate's fanout has already delivered it to
                // the job's `_pyself_` proxy, so this only ACKs. Re-emitting would
                // double-deliver.
                if hasRunningJob(agentId) {
                    return .object(["queued": .bool(true)])
                }
                return .object([
                    "error": .string(
                        "exec_bridge: unknown type \"\(verb)\" (no running job to receive it)")
                ])
            }
        }

        /// Any live durable job of this agent.
        private func hasRunningJob(_ agentId: AgentId) -> Bool {
            lock.withLock {
                jobs.values.contains {
                    $0.agent == agentId && $0.jobId != nil && $0.process.isRunning
                }
            }
        }

        /// `start` — spawn the program in the BACKGROUND and return at once.
        ///
        /// The `code` is persisted onto the record so `boot` revives it after a reboot;
        /// the LOADER lays that down as `main.py` beside `agent.json`.
        private func startReply(agentId: AgentId, payload: JSON, kernel: Kernel) async -> JSON {
            let agent = kernel.agent(agentId)
            let code =
                payload["code"].asString
                ?? agent?.metaValue(forKey: "code")?.asString ?? ""
            if code.isEmpty {
                return .object([
                    "error": .string(
                        "exec_bridge: code (str) required (pass it or set it on the record)")
                ])
            }
            // PER-CALL ARGUMENTS. A missing `args` serializes to "null"; the script
            // contract is an object, so absent means empty rather than null.
            let rawArgs = payload["args"].serialize()
            let argsJSON = rawArgs == "null" ? "{}" : rawArgs
            if argsJSON.utf8.count > Self.maxArgsBytes {
                return .object([
                    "error": .string(
                        "exec_bridge: args too large (\(argsJSON.utf8.count) bytes > "
                            + "\(Self.maxArgsBytes)) — they ride the process env, which caps "
                            + "near 1 MB for the whole block. Put bulk data in an agent and "
                            + "pass its id.")
                ])
            }
            // WHICH interpreter — refused loudly rather than degraded to another one, and
            // validated BEFORE the spawn (see `Interpreter.swift`: a bad `$0` makes
            // /bin/sh exit 127 long after `start` has already answered "running").
            let interp: String
            do {
                interp = try Interpreter.resolve(payload: payload, record: agent)
            } catch {
                return .object(["error": .string("exec_bridge: \(error)")])
            }
            let cwd = payload["cwd"].asString ?? agent?.metaValue(forKey: "cwd")?.asString

            // The OPTIONAL shim — unknown names refuse, absent means the raw wire.
            let shimName = payload["shim"].asString ?? agent?.metaValue(forKey: "shim")?.asString
            let program: String
            switch shimName {
            case nil, "":
                program = code
            case "python":
                program = CONNECTOR_SRC + "\n" + code
            case .some(let other):
                return .object([
                    "error": .string(
                        "exec_bridge: shim \"\(other)\" is unknown — this bridge serves "
                            + "[\"python\"]")
                ])
            }
            let spawned: Spawn.Spawned
            do {
                spawned = try Spawn.job(
                    interp: interp, code: program, selfId: agentId.value, argsJSON: argsJSON,
                    cwd: cwd)
            } catch {
                return .object(["error": .string("exec_bridge: spawn failed: \(error)")])
            }
            let jobId = String(UInt32.random(in: .min ... .max), radix: 16)
            let relay = Relay(
                fd: spawned.ctrlFd, owner: agentId, jobId: jobId, kernel: kernel)
            relay.start()

            let pid = spawned.process.processIdentifier
            // The job OUTLIVES this dispatch, so it needs its OWN task-table row —
            // otherwise the kernel loses sight of it the moment we reply, and a
            // subprocess nobody can reach is the orphan bug.
            // The row key is only known after `insert`, so the pump reads it from a box
            // the insert fills — the task body may start on another thread first.
            let keyBox = KeyBox()
            let taskId = kernel.spawnTask(agent: agentId, verb: "start") { [weak self] in
                await self?.pump(
                    agentId: agentId, jobId: jobId, spawned: spawned, keyBox: keyBox,
                    kernel: kernel)
            }
            let key = insert(
                Job(
                    agent: agentId, taskId: taskId, process: spawned.process, pid: pid,
                    jobId: jobId, relay: relay))

            keyBox.set(key)
            // Durable authored agent: the record carries the program.
            if let agent {
                _ = agent.updateMeta(["code": .string(code)])
                await kernel.persistRecord(agent)
            }
            await kernel.emit(
                agentId, .object(["type": .string("job_started"), "job_id": .string(jobId)]))
            return .object(["job_id": .string(jobId), "status": .string("running")])
        }

        /// `boot` — idempotent rehydration. A record carrying `code` becomes a living
        /// program again, unless one is already running.
        ///
        /// A boot that could NOT revive the program says so. `{ok:true}` while nothing
        /// started is the silence the interpreter refusal exists to replace: a record
        /// with `code` but no `python`/`venv` cannot revive in this runtime (python's
        /// `_boot` persists its own `sys.executable` for exactly this case; there is
        /// nothing here to persist), and the operator has to be able to see that.
        private func bootReply(agentId: AgentId, kernel: Kernel) async -> JSON {
            guard let code = kernel.agent(agentId)?.metaValue(forKey: "code")?.asString,
                !code.isEmpty, !hasRunningJob(agentId)
            else { return .object(["ok": .bool(true)]) }
            // A rehydrate has no caller, so no arguments.
            let started = await startReply(
                agentId: agentId, payload: .object(["code": .string(code)]), kernel: kernel)
            if let err = started["error"].asString {
                return .object(["ok": .bool(false), "error": .string(err)])
            }
            return .object(["ok": .bool(true)])
        }

        /// The job's task-table body: stream both pipes as `progress`, then `job_done`.
        private func pump(
            agentId: AgentId, jobId: String, spawned: Spawn.Spawned, keyBox: KeyBox,
            kernel: Kernel
        ) async {
            // `drain` is readabilityHandler + continuation. `readDataToEndOfFile()` would
            // BLOCK a cooperative-pool thread, which starves the pool the moment two jobs
            // run at once — it passed alone and timed out in a full suite.
            async let out = drain(spawned.stdout.fileHandleForReading)
            async let err = drain(spawned.stderr.fileHandleForReading)
            let o = String(data: await out, encoding: .utf8) ?? ""
            let e = String(data: await err, encoding: .utf8) ?? ""
            let exit = ExitBox()
            spawned.process.terminationHandler = { p in exit.finish(p.terminationStatus) }
            let code = await exit.wait()
            // Retire the row — otherwise `in_flight` never falls back to 0 and a finished
            // job looks alive forever.
            if let k = keyBox.get() { remove(k) }
            relayFor(jobId)?.stop()
            await kernel.emit(
                agentId,
                .object([
                    "type": .string("job_done"), "job_id": .string(jobId),
                    "exit_code": .integer(Int64(code)), "ok": .bool(code == 0),
                    "stdout": .string(o), "stderr": .string(e),
                ]))
        }

        /// Per-call `args` ride the process env, which caps near 1 MB for the whole
        /// block — past it `execve` fails with a cryptic E2BIG. Refuse early, say why.
        private static let maxArgsBytes = 256 * 1024

        /// A job's registry key, published after `insert` and read by its pump. The
        /// pump body can begin on another thread before `insert` returns, so this is a
        /// handoff rather than a captured value.
        private final class KeyBox: @unchecked Sendable {
            private let lock = NSLock()
            private var key: UInt64?
            func set(_ k: UInt64) { lock.withLock { key = k } }
            func get() -> UInt64? { lock.withLock { key } }
        }

        /// The relay serving a given job, if it is still registered.
        private func relayFor(_ jobId: String) -> Relay? {
            lock.withLock { jobs.values.first { $0.jobId == jobId }?.relay }
        }

        /// The agent is going away — SIGKILL every child it still owns, so a
        /// deleted agent cannot leave a `python3` behind.
        public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
            teardownAgent(agentId)
        }

        /// Same, on kernel shutdown: the process is about to exit and orphan
        /// whatever it still holds.
        public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
            teardownAgent(agentId)
        }

        /// THE CANCEL HOOK — the SIGTERM half, and the whole reason this bundle
        /// keeps a job table.
        ///
        /// The kernel is about to abort `taskId`'s Swift `Task`. That frees NO OS
        /// resource: the `python3 -c ...` child of that dispatch would keep running
        /// with nothing pointing at it — an orphan. We own the process, so we kill
        /// it, and the substrate awaits us BEFORE it aborts anything.
        ///
        /// TASK-scoped, not agent-scoped (matching the python canonical): a second
        /// job running concurrently on the same agent is a DIFFERENT task and
        /// survives untouched. `stop` is the agent-scoped, kill-them-all verb.
        public func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
            let doomed = lock.withLock {
                jobs.values.filter { $0.agent == agentId && $0.taskId == taskId }
            }
            for job in doomed { teardown(job) }
        }

        // MARK: - pipes

        /// Read one pipe to EOF without pinning a thread: `readabilityHandler` fires
        /// on Foundation's own queue, and the empty read that means EOF resumes us.
        /// A killed child EOFs too (the kernel closes its write ends), so this always
        /// completes.
        private func drain(_ handle: FileHandle) async -> Data {
            let box = DataBox()
            return await withCheckedContinuation { (c: CheckedContinuation<Data, Never>) in
                handle.readabilityHandler = { h in
                    let chunk = h.availableData
                    if chunk.isEmpty {
                        h.readabilityHandler = nil
                        c.resume(returning: box.take())
                    } else {
                        box.append(chunk)
                    }
                }
            }
        }

        // MARK: - the table + the ONE teardown

        private func insert(_ job: Job) -> UInt64 {
            lock.withLock {
                nextKey += 1
                jobs[nextKey] = job
                return nextKey
            }
        }

        private func remove(_ key: UInt64) {
            lock.withLock { _ = jobs.removeValue(forKey: key) }
        }

        /// Free the REAL resource of ONE job: SIGKILL its child. The single teardown
        /// every death path shares — `onCancel` (the job's task is going away) and
        /// `onDelete` / `onShutdown` (the agent/kernel is going away). The caller-driven
        /// signals (`stop` / `interrupt`) go through `signalAll`, which reports a count.
        /// Idempotent, and never signals a pid it
        /// no longer owns: an exited child has been reaped by `Process`, and firing at
        /// its number could hit whatever the OS recycled it into.
        private func teardown(_ job: Job) {
            guard job.process.isRunning else { return }
            kill(job.pid, SIGKILL)
        }

        /// SIGKILL every child of `agentId` — the cascade the agent's death drives.
        private func teardownAgent(_ agentId: AgentId) {
            let doomed = lock.withLock { jobs.values.filter { $0.agent == agentId } }
            for job in doomed { teardown(job) }
        }

        /// Signal every in-flight child of `agentId` — the `stop` / `interrupt`
        /// verbs (agent-scoped, like rust's). Returns how many were actually
        /// signalled.
        private func signalAll(_ agentId: AgentId, _ sig: Int32) -> Int {
            let live = lock.withLock { jobs.values.filter { $0.agent == agentId } }
            var n = 0
            for job in live where job.process.isRunning {
                if kill(job.pid, sig) == 0 { n += 1 }
            }
            return n
        }

        private func inFlight(_ agentId: AgentId) -> Int {
            lock.withLock { jobs.values.filter { $0.agent == agentId }.count }
        }

        // MARK: - reflect

        /// EVERY key of `verbs` must be a verb `handle` really dispatches, and every
        /// verb it dispatches belongs here. The map used to advertise `exec` — a verb
        /// that has not existed since jobs became background+addressable — while
        /// omitting `start` and `shutdown`, the two that carry the work. There is no
        /// `status` here either (python has one; this runtime does not).
        ///
        /// `python` reports the interpreter `start` WOULD use, or `<unresolved: …>` —
        /// never the one a broken `venv` would have degraded to, because reflect is how
        /// a caller learns its wiring is wrong. The `sentence` states this runtime's
        /// deliberate divergence from canonical (no default interpreter) so a caller
        /// meets the refusal on the discovery surface, not on a failed job.
        private func reflectReply(_ agentId: AgentId, kernel: Kernel) -> JSON {
            let record = kernel.agent(agentId)
            return .object([
                "id": .string(agentId.value),
                "kind": .string("exec_bridge"),
                "sentence": .string(
                    "Process edge of the io family — sealed by default, allowlisted interpreters only. Spawns a program in the background; it gets a kernel wire over a private control fd."
                ),
                "cwd": record?.metaValue(forKey: "cwd") ?? .string("<process default>"),
                "interpreters": .array(
                    Interpreter.allowed(record: record).map(JSON.string)),
                "shims": .array([.string("python")]),
                "running": .integer(Int64(inFlight(agentId))),
                "ingress_rule": record?.metaValue(forKey: "ingress_rule")
                    ?? record?.metaValue(forKey: "auth") ?? .string("deny_inbound"),
                "sealed": .bool(
                    (record?.metaValue(forKey: "ingress_rule")?.asString
                        ?? record?.metaValue(forKey: "auth")?.asString) != "allow_all"),
                "verbs": .object([
                    "reflect": .string(
                        "Identity + interpreter + count of in-flight subprocesses. No args."),
                    "start": .string(
                        "args: code? (defaults to the record's `code`), args? (object, THIS call's kernel.args), interpreter? (one of the allowlist; optional when it lists exactly one), shim? (\"python\" or absent ⇒ the raw wire), cwd?. Spawns in the BACKGROUND and returns {job_id, status:\"running\"} at once — the program's output is NOT this reply."
                    ),
                    "boot": .string(
                        "No args. Idempotent rehydration: re-starts the record's `code` unless a job is already running. Returns {ok:true}, or {ok:false, error} when the program could not be revived (e.g. a non-allowlisted interpreter) — the error PROPAGATES rather than booting silently broken."
                    ),
                    "interrupt": .string(
                        "No args. SIGINT to every in-flight child of this agent. Returns {interrupted:int}."
                    ),
                    "stop": .string(
                        "No args. SIGKILL to every in-flight child of this agent. Returns {killed:int}."
                    ),
                    "shutdown": .string("No args. Returns {ok:true}."),
                ]),
                "inbox": .string(
                    "There is no `exec`: any verb NOT listed above is MAIL for the running program, delivered to its `kernel.on_message(callback)` and ACKed {queued:true}. With no live job it is an error — a script that only `def`s something exits before its first turn arrives."
                ),
                "tasks": .string(
                    "Each running job IS a row in the kernel TASK TABLE ({agent:<this id>, verb:\"start\"} in `ps`). `cancel task_id=<id>` kills it — this bundle's `on_cancel` hook SIGKILLs the real OS subprocess before the kernel aborts the task, so a cancelled call never orphans a `python3`. `stop`/`interrupt` are the signal-precise verbs (they signal the PROGRAM; `cancel` ends the CALL)."
                ),
            ])
        }
    }

    // MARK: - small thread-safe boxes

    /// The child's exit status, delivered by `terminationHandler` (which fires on
    /// a Foundation thread) to whoever `await`s it. Stores the status when nobody
    /// is waiting yet, so an instant exit cannot be missed. Same shape as the
    /// kernel's own `DispatchBox`.
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

#endif  // os(macOS)
