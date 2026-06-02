// Python subprocess execution bundle (macOS only).
//
// Mirrors Rust's `fantastic-python-runtime::PythonRuntimeBundle`.
// Spawns `python3 -c <script>` per `exec` verb; streams stdout +
// stderr back as the reply. iOS sandbox forbids subprocess so this
// is macOS-only.

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation

    public let HANDLER_MODULE = "python_runtime.tools"

    public struct PythonRuntimeBundle: AgentBundle {
        public let name = "python_runtime"
        public init() {}

        public var readme: String? {
            """
            python_runtime — subprocess Python exec. Each `exec` is its own \
            process (`python3 -c <code>`), stateless across calls.
            Verbs: reflect, exec (code), interrupt, stop, boot.
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
                    "kind": .string("python_runtime"),
                    "sentence": .string(
                        "Python subprocess — exec `python3 -c <script>` and capture stdout/stderr."),
                    "verbs": [
                        "exec": "args: code. Returns {stdout, stderr, exit_code}."
                    ] as JSON,
                ] as JSON
            case "boot", "shutdown":
                return .object(["ok": .bool(true)])
            case "exec":
                guard let code = payload["code"].asString else {
                    return .object(["error": .string("exec requires code")])
                }
                return await execPython(code: code)
            default:
                return .object(["error": .string("unknown verb \(verb)")])
            }
        }

        private func execPython(code: String) async -> JSON {
            let proc = Process()
            proc.executableURL = URL(fileURLWithPath: "/usr/bin/python3")
            proc.arguments = ["-c", code]
            let stdoutPipe = Pipe()
            let stderrPipe = Pipe()
            proc.standardOutput = stdoutPipe
            proc.standardError = stderrPipe

            do {
                try proc.run()
            } catch {
                return .object([
                    "error": .string("python3 spawn failed: \(error)"),
                    "exit_code": .integer(-1),
                ])
            }
            proc.waitUntilExit()

            let stdoutData = stdoutPipe.fileHandleForReading.readDataToEndOfFile()
            let stderrData = stderrPipe.fileHandleForReading.readDataToEndOfFile()
            let stdout = String(data: stdoutData, encoding: .utf8) ?? ""
            let stderr = String(data: stderrData, encoding: .utf8) ?? ""

            return .object([
                "stdout": .string(stdout),
                "stderr": .string(stderr),
                "exit_code": .integer(Int64(proc.terminationStatus)),
            ])
        }
    }

#endif  // os(macOS)
