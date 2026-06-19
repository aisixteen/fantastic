// Subprocess lifecycle bundle (macOS only).
//
// Mirrors Rust's `fantastic-local-runner::LocalRunnerBundle`. Spawns +
// tracks child processes; lets the kernel start / stop / list-children
// external programs. Gated to macOS via `#if` because the iOS sandbox
// forbids Process spawning.
//
// The shared lifecycle dispatch (reflect/boot/shutdown/start/stop +
// unknown-verb) lives in `FantasticRunnerCore`; this target supplies
// only the local `RunnerTransport` conformance (subprocess spawn +
// tracked-children state) + a thin bundle that routes through
// `RunnerCore`.

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import FantasticRunnerCore
    import Foundation

    public let HANDLER_MODULE = "local_runner.tools"

    /// Process-state shared between the bundle and its per-call
    /// transports: the tracked child processes guarded by a lock.
    final class LocalRunnerState: @unchecked Sendable {
        private let lock = NSLock()
        private var processes: [String: Process] = [:]

        func start(name: String, proc: Process) {
            lock.lock()
            processes[name]?.terminate()
            processes[name] = proc
            lock.unlock()
        }

        func remove(name: String) -> Process? {
            lock.lock()
            defer { lock.unlock() }
            return processes.removeValue(forKey: name)
        }

        func rows() -> [JSON] {
            lock.lock()
            defer { lock.unlock() }
            return processes.map { (name, proc) in
                JSON.object([
                    "name": .string(name),
                    "pid": .integer(Int64(proc.processIdentifier)),
                    "running": .bool(proc.isRunning),
                ])
            }
        }

        func stopAll() {
            lock.lock()
            let snapshot = processes
            processes.removeAll()
            lock.unlock()
            for (_, p) in snapshot { p.terminate() }
        }

        func count() -> Int {
            lock.lock()
            defer { lock.unlock() }
            return processes.count
        }
    }

    public final class LocalRunnerBundle: AgentBundle, @unchecked Sendable {
        public let name = "local_runner"
        public init() {}

        private let state = LocalRunnerState()

        public var readme: String? {
            """
            local_runner — subprocess lifecycle for local projects.
            Verbs: reflect / boot / shutdown / start / stop / list.
            """
        }

        public func handle(
            agentId: AgentId,
            payload: JSON,
            kernel: Kernel
        ) async throws -> JSON? {
            let verb = payload["type"].asString ?? ""
            let transport = LocalTransport(agentId: agentId, payload: payload, state: state)
            return await RunnerCore.handle(verb: verb, transport: transport)
        }

        public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
            state.stopAll()
        }
    }

    /// Local transport — owns each verb's concrete reply body. Built per
    /// `handle` call over the bundle's shared process state.
    struct LocalTransport: RunnerTransport {
        let agentId: AgentId
        let payload: JSON
        let state: LocalRunnerState

        func reflect() async -> JSON {
            [
                "id": .string(agentId.value),
                "kind": .string("local_runner"),
                "sentence": .string("Subprocess lifecycle — spawn / stop / list."),
                "active": .integer(Int64(state.count())),
                "verbs": [
                    "start": "args: name, command, args?, env?, cwd?.",
                    "stop": "args: name.",
                    "list": "Returns active children.",
                ] as JSON,
            ] as JSON
        }

        func start() async -> JSON {
            guard let name = payload["name"].asString,
                let cmd = payload["command"].asString
            else {
                return .object([
                    "error": .string("start requires name + command")
                ])
            }
            let args = (payload["args"].asArray ?? []).compactMap { $0.asString }
            let proc = Process()
            proc.executableURL = URL(fileURLWithPath: cmd)
            proc.arguments = args
            if let cwd = payload["cwd"].asString {
                proc.currentDirectoryURL = URL(fileURLWithPath: cwd)
            }
            if case let .object(envDict) = payload["env"] {
                var env = ProcessInfo.processInfo.environment
                for (k, v) in envDict {
                    if let s = v.asString { env[k] = s }
                }
                proc.environment = env
            }
            do {
                try proc.run()
            } catch {
                return .object([
                    "error": .string("start failed: \(error)"),
                    "name": .string(name),
                ])
            }
            state.start(name: name, proc: proc)
            return .object([
                "ok": .bool(true),
                "name": .string(name),
                "pid": .integer(Int64(proc.processIdentifier)),
            ])
        }

        func stop() async -> JSON {
            guard let name = payload["name"].asString else {
                return .object(["error": .string("stop requires name")])
            }
            let proc = state.remove(name: name)
            proc?.terminate()
            return .object([
                "ok": .bool(true),
                "stopped": .bool(proc != nil),
            ])
        }

        func shutdownAll() async {
            state.stopAll()
        }

        func handleVerb(_ verb: String) async -> JSON? {
            guard verb == "list" else { return nil }
            return .object(["children": .array(state.rows())])
        }
    }

#endif  // os(macOS)
