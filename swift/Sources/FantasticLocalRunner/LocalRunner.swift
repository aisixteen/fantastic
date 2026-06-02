// Subprocess lifecycle bundle (macOS only).
//
// Mirrors Rust's `fantastic-local-runner::LocalRunnerBundle`. Spawns
// + tracks child processes; lets the kernel start / stop /
// list-children external programs. Gated to macOS via `#if`
// because iOS sandbox forbids Process spawning.

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation

    public let HANDLER_MODULE = "local_runner.tools"

    public final class LocalRunnerBundle: AgentBundle, @unchecked Sendable {
        public let name = "local_runner"
        public init() {}

        private let lock = NSLock()
        private var processes: [String: Process] = [:]

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
            switch verb {
            case "reflect":
                return [
                    "id": .string(agentId.value),
                    "kind": .string("local_runner"),
                    "sentence": .string("Subprocess lifecycle — spawn / stop / list."),
                    "active": .integer(Int64(activeCount())),
                    "verbs": [
                        "start": "args: name, command, args?, env?, cwd?.",
                        "stop": "args: name.",
                        "list": "Returns active children.",
                    ] as JSON,
                ] as JSON
            case "boot":
                return .object(["ok": .bool(true)])
            case "shutdown":
                stopAll()
                return .object(["ok": .bool(true)])
            case "start":
                return startVerb(payload: payload)
            case "stop":
                return stopVerb(payload: payload)
            case "list":
                return listVerb()
            default:
                return .object(["error": .string("unknown verb \(verb)")])
            }
        }

        public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
            stopAll()
        }

        private func startVerb(payload: JSON) -> JSON {
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
            lock.lock()
            processes[name]?.terminate()
            processes[name] = proc
            lock.unlock()
            return .object([
                "ok": .bool(true),
                "name": .string(name),
                "pid": .integer(Int64(proc.processIdentifier)),
            ])
        }

        private func stopVerb(payload: JSON) -> JSON {
            guard let name = payload["name"].asString else {
                return .object(["error": .string("stop requires name")])
            }
            lock.lock()
            let proc = processes.removeValue(forKey: name)
            lock.unlock()
            proc?.terminate()
            return .object([
                "ok": .bool(true),
                "stopped": .bool(proc != nil),
            ])
        }

        private func listVerb() -> JSON {
            lock.lock()
            let rows = processes.map { (name, proc) in
                JSON.object([
                    "name": .string(name),
                    "pid": .integer(Int64(proc.processIdentifier)),
                    "running": .bool(proc.isRunning),
                ])
            }
            lock.unlock()
            return .object(["children": .array(rows)])
        }

        private func stopAll() {
            lock.lock()
            let snapshot = processes
            processes.removeAll()
            lock.unlock()
            for (_, p) in snapshot { p.terminate() }
        }

        private func activeCount() -> Int {
            lock.lock()
            defer { lock.unlock() }
            return processes.count
        }
    }

#endif  // os(macOS)
