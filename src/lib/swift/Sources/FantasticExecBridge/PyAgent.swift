// py_agent — the BASE a folder bundle with a program extends.
// Python `py_agent.tools`, rust `fantastic-py-agent`, text for text. Lives in this
// module because it is the process edge too: it spawns through `Spawn` (the one
// spawner) and speaks the ONE relay (`Relay`, with two more ops).
//
// A folder bundle `<root>/bundles/<name>/bundle.yaml` with `base: py_agent` names
// its program (`command`, cwd = the folder) and its verbs (`verbs`: verb → doc, the
// one truth `docs` and `reflect bundles=all` read). Each instance is a FULL agent:
// the kernel starts the program when it boots and kills it on delete / stop;
// callers reach it only through verbs. Wire: child `hello {verbs}` first (must
// equal bundle.yaml), kernel `call {rid, payload}`, child `result {rid, result}`,
// plus the relay's own `send`/`emit`/`watch`. Nothing respawns. SECURITY: the
// program runs with the user's OS rights.

#if os(macOS)
    import FantasticJSON
    import FantasticKernel
    import Foundation
    import OrderedCollections

    /// The base's handler module.
    public let PY_AGENT_HANDLER_MODULE = "py_agent.tools"
    /// Said in every instance's reflect and in the docs notes.
    public let PY_AGENT_SECURITY =
        "a py_agent program runs with the user's OS rights — its own subprocesses, files and "
        + "network never pass a gate"
    /// Chars of the program's stderr kept for its instance's reflect.
    let PY_AGENT_STDERR_TAIL = 2000
    private let RESERVED: Set<String> = ["reflect", "boot", "shutdown"]
    private let NOTES =
        "A folder bundle `<root>/bundles/<name>/bundle.yaml` with `base: py_agent`: `sentence`, "
        + "`command` (argv, cwd = the folder), `verbs` (verb → doc), optional `emits`, `notes`, "
        + "`record` (defaults under each instance). "

    // MARK: - the base itself

    /// The `py_agent` bundle: a base only — an agent is created of a folder bundle
    /// that extends it.
    public struct PyAgentBundle: AgentBundle {
        public init() {}
        public var name: String { "py_agent" }
        public var folderBase: Bool { true }
        public var verbs: [String] { ["reflect"] }
        public var verbDocTable: JSON {
            .object(["reflect": .string("No args. → {id, sentence}.")])
        }
        public var notesDoc: JSON? { .string(NOTES + PY_AGENT_SECURITY + ".") }

        public func validateMeta(
            _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
        ) -> String? {
            "py_agent is a base — create an agent of a bundle folder whose bundle.yaml says "
                + "`base: py_agent` (handler_module = the folder name)"
        }

        public func folderBundle(name: String, folder: URL, spec: JSON) -> FolderBuild {
            PyFolderBundle.build(name: name, folder: folder, spec: spec)
        }

        public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
            let t = payload["type"]
            if t.asString == "reflect" {
                return .object([
                    "id": .string(agentId.value),
                    "sentence": .string(
                        "The base a folder bundle with a program extends — create an agent of "
                            + "the bundle, not of py_agent."),
                ])
            }
            return Refusal.make(
                Refusal.unhandled, "py_agent: no verb \(pyRepr(t))", extra: [("verb", t)])
        }
    }

    // MARK: - one instance's running program

    private enum PyPhase: String { case starting, running, stopped }

    private final class PyRun: @unchecked Sendable {
        let lock = NSLock()
        var phase: PyPhase = .starting
        var error: String?
        var code: Int32?
        var stderr = ""
        var pending: [String: CheckedContinuation<JSON, Never>] = [:]
        var helloWaiters: [CheckedContinuation<Void, Never>] = []
        var rid = 0
        var taskIds: Set<Int64> = []
        let child: Child
        var relay: Relay?

        init(child: Child) { self.child = child }

        var currentPhase: PyPhase { lock.withLock { phase } }

        func waitHello() async {
            await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
                lock.lock()
                if phase != .starting {
                    lock.unlock()
                    c.resume()
                } else {
                    helloWaiters.append(c)
                    lock.unlock()
                }
            }
        }

        func markRunning() {
            let waiters: [CheckedContinuation<Void, Never>] = lock.withLock {
                phase = .running
                defer { helloWaiters.removeAll() }
                return helloWaiters
            }
            for w in waiters { w.resume() }
        }

        /// Stop the program (kill its session unless it already exited), settle
        /// every in-flight call with `why`, release the waiters. Python `_stop`.
        func stop(_ why: String, exited: Bool) {
            let taken:
                (
                    pending: [CheckedContinuation<JSON, Never>],
                    hello: [CheckedContinuation<Void, Never>]
                )? =
                    lock.withLock {
                        guard phase != .stopped else { return nil }
                        phase = .stopped
                        error = why
                        defer {
                            pending.removeAll()
                            helloWaiters.removeAll()
                        }
                        return (Array(pending.values), helloWaiters)
                    }
            guard let taken else { return }
            if !exited && child.isAlive { _ = Spawn.killSession(child.pid) }
            for c in taken.pending {
                c.resume(returning: Refusal.make(Refusal.transportClosed, why))
            }
            for w in taken.hello { w.resume() }
            relay?.stop()
        }
    }

    /// kernel serial | instance id → its running program. Keyed by kernel: swift
    /// hosts many kernels per process.
    private final class PyRuns: @unchecked Sendable {
        static let shared = PyRuns()
        private let lock = NSLock()
        private var table: [String: PyRun] = [:]
        func get(_ k: Kernel, _ id: AgentId) -> PyRun? {
            lock.withLock { table["\(k.serial)|\(id.value)"] }
        }
        func put(_ k: Kernel, _ id: AgentId, _ r: PyRun) {
            lock.withLock { table["\(k.serial)|\(id.value)"] = r }
        }
    }

    // MARK: - a folder bundle that extends it

    final class PyFolderBundle: AgentBundle, @unchecked Sendable {
        let name: String
        let folder: URL
        let command: [String]
        let verbTable: OrderedDictionary<String, JSON>
        let sentence: String
        let emits: JSON?
        let notes: String
        let record: OrderedDictionary<String, JSON>

        private init(
            name: String, folder: URL, command: [String], verbs: OrderedDictionary<String, JSON>,
            sentence: String, emits: JSON?, notes: String, record: OrderedDictionary<String, JSON>
        ) {
            self.name = name
            self.folder = folder
            self.command = command
            self.verbTable = verbs
            self.sentence = sentence
            self.emits = emits
            self.notes = notes
            self.record = record
        }

        /// Python `folder_bundle`: the bundle a folder runs as, or the refusal text
        /// naming the field.
        static func build(name: String, folder: URL, spec: JSON) -> FolderBuild {
            let argv = spec["command"].asArray?.compactMap {
                $0.asString.flatMap { $0.isEmpty ? nil : $0 }
            }
            guard let argv, !argv.isEmpty, argv.count == spec["command"].asArray?.count else {
                return .refused(
                    "command: required — the program's argv as a list of strings, e.g. [python3, "
                        + "agent.py] (run with cwd = the bundle folder)")
            }
            guard let verbs = spec["verbs"].asObject, !verbs.isEmpty,
                verbs.allSatisfy({ !$0.key.isEmpty && $0.value.asString != nil })
            else {
                return .refused(
                    "verbs: required — a map of verb → its doc (the first sentence is the "
                        + "signature), e.g. {search: 'args: q:str (req). Returns {hits}.'}")
            }
            let taken = verbs.keys.filter { RESERVED.contains($0) }
            if !taken.isEmpty {
                return .refused(
                    "verbs: \(taken.joined(separator: ", ")) — answered by the kernel; name the "
                        + "program's verbs otherwise")
            }
            var record: OrderedDictionary<String, JSON> = [:]
            switch spec["record"] {
            case .null: break
            case .object(let m): record = m
            default:
                return .refused("record: must be a mapping of defaults for each instance's record")
            }
            let n = spec["notes"].asString.flatMap { $0.isEmpty ? nil : $0 }
            let notes = n.map { "\($0)\n\n\(PY_AGENT_SECURITY)." } ?? "\(PY_AGENT_SECURITY)."
            let emits: JSON? = spec["emits"].isNull ? nil : spec["emits"]
            return .built(
                PyFolderBundle(
                    name: name, folder: folder, command: argv, verbs: verbs,
                    sentence: (spec["sentence"].asString ?? "").trimmingCharacters(
                        in: .whitespacesAndNewlines),
                    emits: emits, notes: notes, record: record))
        }

        var verbs: [String] { Array(verbTable.keys) }
        var verbDocTable: JSON { .object(verbTable) }
        var emitsDoc: JSON? { emits }
        var notesDoc: JSON? { .string(notes) }

        func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
            let t = payload["type"]
            switch t.asString ?? "" {
            case "reflect":
                return instanceReflect(agentId, kernel)
            case "boot":
                return await start(agentId, kernel)
            case "shutdown":
                // a reload (`reload_bundle`: shutdown, then boot) or a stop
                let reason =
                    payload["reason"].asString.flatMap { $0.isEmpty ? nil : $0 }
                    ?? "the kernel stopped"
                PyRuns.shared.get(kernel, agentId)?.stop(
                    "\(agentId.value): \(reason)", exited: false)
                return nil
            case let v where verbTable[v] != nil:
                return await call(agentId, payload, kernel)
            default:
                return Refusal.make(
                    Refusal.unhandled, "\(name): no verb \(pyRepr(t))",
                    hint: "its verbs: \(verbTable.keys.joined(separator: ", "))",
                    extra: [("verb", t)])
            }
        }

        func onDelete(agentId: AgentId, kernel: Kernel) async throws {
            PyRuns.shared.get(kernel, agentId)?.stop("\(agentId.value): deleted", exited: false)
        }

        func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
            PyRuns.shared.get(kernel, agentId)?.stop(
                "\(agentId.value): the kernel stopped", exited: false)
        }

        func onCancel(agentId: AgentId, taskId: TaskId, kernel: Kernel) async throws {
            // the program's own task (relay / wait / output) cancelled: the process goes too
            guard let r = PyRuns.shared.get(kernel, agentId) else { return }
            if r.lock.withLock({ r.taskIds.contains(taskId.value) }) {
                r.stop("\(agentId.value): cancelled", exited: false)
            }
        }

        private func instanceReflect(_ id: AgentId, _ kernel: Kernel) -> JSON {
            let run = PyRuns.shared.get(kernel, id)
            var out: OrderedDictionary<String, JSON> = [
                "id": .string(id.value),
                "sentence": .string(sentence),
                "base": .string("py_agent"),
                "bundle": .string(name),
                "folder": .string(folder.path),
                "program": .string(run.map { $0.currentPhase.rawValue } ?? "not started"),
                "security": .string(PY_AGENT_SECURITY),
            ]
            if let run {
                let (code, error, tail) = run.lock.withLock { (run.code, run.error, run.stderr) }
                if let code { out["exit_code"] = .integer(Int64(code)) }
                if let error { out["program_error"] = .string(error) }
                if !tail.isEmpty { out["stderr_tail"] = .string(tail) }
            }
            return .object(out)
        }

        /// The child's whole environment from the merged record: `inherit_env`
        /// names (`["*"]` = all), then `env` — python `_child_env`.
        private func childEnv(_ rec: OrderedDictionary<String, JSON>) -> [String: String] {
            let names = (rec["inherit_env"]?.asArray ?? []).compactMap { $0.asString }
            let host = ProcessInfo.processInfo.environment
            var env: [String: String] = [:]
            if names.contains("*") {
                env = host
            } else {
                for n in names { if let v = host[n] { env[n] = v } }
            }
            for (k, v) in rec["env"]?.asObject ?? [:] { env[k] = pyStr(v) }
            return env
        }

        /// Spawn the instance's program (idempotent while it runs).
        private func start(_ id: AgentId, _ kernel: Kernel) async -> JSON? {
            if let r = PyRuns.shared.get(kernel, id), r.currentPhase != .stopped { return nil }
            var merged = record
            for (k, v) in kernel.agent(id)?.meta ?? [:] { merged[k] = v }
            let spawned: Spawn.Spawned
            do {
                spawned = try Spawn.job(
                    argv: command, selfId: id.value, env: childEnv(merged), cwd: folder.path)
            } catch is Spawn.NoPath {
                return Refusal.make(
                    Refusal.missingArg,
                    "\(name): \(pyRepr(.string(command[0]))) is a bare command and the program gets no "
                        + "PATH to find it in — the kernel's own PATH is not the child's",
                    hint: "bundle.yaml record: {inherit_env: [PATH]} (or an absolute command)",
                    extra: [("arg", .string("PATH"))])
            } catch {
                return Refusal.make(Refusal.transportError, "\(name): spawn failed: \(error)")
            }
            let run = PyRun(child: spawned.child)
            PyRuns.shared.put(kernel, id, run)
            let declared = Set(verbTable.keys)
            let bundleName = name
            let relay = Relay(
                fd: spawned.ctrlFd, owner: id, jobId: TaskId(kernel.mintId()), kernel: kernel,
                extraOps: { [weak run] m in
                    guard let run else { return }
                    switch m["op"].asString {
                    case "hello":
                        let served = m["verbs"].asArray?.compactMap { $0.asString }
                        if let served, served.count == m["verbs"].asArray?.count,
                            Set(served) == declared
                        {
                            run.markRunning()
                            await kernel.emit(
                                id,
                                .object(["type": .string("status"), "phase": .string("running")]))
                        } else {
                            let got =
                                served.map { pyRepr(.array($0.sorted().map { JSON.string($0) })) }
                                ?? pyRepr(m["verbs"])
                            let want = pyRepr(.array(declared.sorted().map { JSON.string($0) }))
                            run.stop(
                                "\(bundleName): the program serves \(got) but bundle.yaml declares \(want) — they must match",
                                exited: false)
                        }
                    case "result":
                        let rid = pyStr(m["rid"])
                        let c = run.lock.withLock { run.pending.removeValue(forKey: rid) }
                        c?.resume(returning: m["result"])
                    default: break
                    }
                })
            run.lock.withLock { run.relay = relay }
            let relayTask = kernel.spawnTask(agent: id, verb: "relay") { [relay] in
                await relay.run(taskId: KernelTaskLocals.currentTask)
            }
            let child = spawned.child
            let waitTask = kernel.spawnTask(agent: id, verb: "program") { [weak run] in
                let code = await child.wait()
                guard let run, run.currentPhase != .stopped else { return }
                run.lock.withLock { run.code = code }
                run.stop("\(id.value): its program exited (code \(code))", exited: true)
                await kernel.emit(
                    id,
                    .object([
                        "type": .string("status"), "phase": .string("stopped"),
                        "reason": .string("exited"), "code": .integer(Int64(code)),
                    ]))
            }
            // stdout/stderr are PIPES: unread, a chatty program blocks once one fills.
            // stdout is the program's own; stderr's tail is kept for reflect.
            let (out, err) = (spawned.stdout, spawned.stderr)
            let outputTask = kernel.spawnTask(agent: id, verb: "output") { [run] in
                async let a: Void = Self.drain(out) { _ in }
                async let b: Void = Self.drain(err) { chunk in
                    let text = String(decoding: chunk, as: UTF8.self)
                    run.lock.withLock {
                        run.stderr = String((run.stderr + text).suffix(PY_AGENT_STDERR_TAIL))
                    }
                }
                _ = await (a, b)
            }
            run.lock.withLock {
                run.taskIds.formUnion([relayTask.value, waitTask.value, outputTask.value])
            }
            return nil
        }

        /// Read `handle` to EOF, handing each chunk to `onChunk`.
        private static func drain(
            _ handle: FileHandle, _ onChunk: @escaping @Sendable (Data) -> Void
        ) async {
            await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
                handle.readabilityHandler = { h in
                    let chunk = h.availableData
                    if chunk.isEmpty {
                        h.readabilityHandler = nil
                        try? h.close()
                        c.resume()
                        return
                    }
                    onChunk(chunk)
                }
            }
        }

        private func call(_ id: AgentId, _ payload: JSON, _ kernel: Kernel) async -> JSON {
            guard let run = PyRuns.shared.get(kernel, id), run.currentPhase != .stopped else {
                let why =
                    PyRuns.shared.get(kernel, id).flatMap { r in r.lock.withLock { r.error } }
                    ?? "\(id.value): its program is not running"
                return Refusal.make(
                    Refusal.transportClosed, why,
                    hint: "fix the bundle, then the root's reload_bundle restarts it")
            }
            await run.waitHello()
            guard run.currentPhase == .running else {
                let why = run.lock.withLock { run.error } ?? "\(id.value): its program stopped"
                return Refusal.make(Refusal.transportClosed, why)
            }
            let rid: String = run.lock.withLock {
                run.rid += 1
                return String(run.rid)
            }
            return await withCheckedContinuation { (c: CheckedContinuation<JSON, Never>) in
                let why: String? = run.lock.withLock {
                    if run.phase == .stopped {
                        return run.error ?? "\(id.value): its program stopped"
                    }
                    run.pending[rid] = c
                    return nil
                }
                if let why {
                    c.resume(returning: Refusal.make(Refusal.transportClosed, why))
                    return
                }
                run.relay?.pushOut(
                    .object(["op": .string("call"), "rid": .string(rid), "payload": payload]))
            }
        }
    }
#endif
