// Pseudo-tty backend (macOS only).
//
// Mirrors Rust's `fantastic-terminal-backend`. Spawns a shell under
// a PTY via Darwin's `openpty()` + `posix_spawn`. Background reader
// pumps PTY master output to the kernel as `{type:"output", chunk}`
// emit events; `{type:"input", data}` verbs write to the master.
//
// iOS sandbox forbids fork / posix_spawn, so this file is
// macOS-only via `#if`.
//
// Trade-offs vs Rust:
//   - No flow control window (ack-per-5K-chars) in the initial
//     port; ack verbs are accepted but treated as no-op.
//   - 5 MB output cap not enforced (Apple's terminal flow control
//     covers most cases; revisit if a flood actually shows up).

#if os(macOS)

    import Darwin
    import FantasticJSON
    import FantasticKernel
    import Foundation

    public let HANDLER_MODULE = "terminal_backend.tools"

    public final class TerminalBackendBundle: AgentBundle, @unchecked Sendable {
        public let name = "terminal_backend"
        public init() {}

        private let lock = NSLock()
        private var sessions: [AgentId: PtySession] = [:]

        public var readme: String? {
            """
            terminal_backend — PTY shell session as an agent. One PTY per agent; process-memory state only.
            Verbs: reflect, boot, input/write, ack, resize, paste_image, shutdown/stop. Output streams to this agent's own inbox as {type:"output", chunk} (a client watches it).
            """
        }

        public func handle(
            agentId: AgentId,
            payload: JSON,
            kernel: Kernel
        ) async throws -> JSON? {
            let verb = payload["type"].asString ?? ""
            guard let agent = kernel.agent(agentId) else {
                return .object(["error": .string("no agent")])
            }
            switch verb {
            case "reflect":
                return [
                    "id": .string(agent.id.value),
                    "kind": .string("terminal_backend"),
                    "sentence": .string("PTY shell session."),
                    "running": .bool(sessionFor(agent.id) != nil),
                    "verbs": [
                        "boot": "Spawns the shell (defaults to $SHELL or /bin/zsh).",
                        "input": "args: data. Writes bytes to PTY stdin.",
                        "resize": "args: cols, rows. Resizes the PTY.",
                        "ack": "args: bytes. Flow control ack (no-op in Swift port).",
                        "paste_image": "args: data_base64. Writes base64-decoded bytes.",
                        "shutdown": "Kills the session.",
                    ] as JSON,
                ] as JSON
            case "boot":
                return bootVerb(agent: agent, payload: payload, kernel: kernel)
            case "input":
                return inputVerb(agentId: agent.id, payload: payload)
            case "paste_image":
                return pasteImageVerb(agentId: agent.id, payload: payload)
            case "resize":
                return resizeVerb(agentId: agent.id, payload: payload)
            case "ack":
                // Flow control no-op in the Swift port — Apple PTY
                // backpressure is enough for the brain-kernel scale.
                return .object(["ok": .bool(true)])
            case "shutdown":
                shutdownVerb(agentId: agent.id)
                return .object(["ok": .bool(true)])
            default:
                return .object(["error": .string("unknown verb \(verb)")])
            }
        }

        public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
            shutdownVerb(agentId: agentId)
        }

        public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
            shutdownVerb(agentId: agentId)
        }

        // MARK: - Verbs

        private func bootVerb(agent: Agent, payload: JSON, kernel: Kernel) -> JSON {
            if let existing = sessionFor(agent.id) {
                return .object([
                    "ok": .bool(true),
                    "running": .bool(true),
                    "pid": .integer(Int64(existing.pid)),
                ])
            }
            let shell =
                agent.metaValue(forKey: "shell")?.asString
                ?? ProcessInfo.processInfo.environment["SHELL"]
                ?? "/bin/zsh"
            let cols = UInt16(agent.metaValue(forKey: "cols")?.asInt ?? 80)
            let rows = UInt16(agent.metaValue(forKey: "rows")?.asInt ?? 24)
            do {
                let session = try PtySession.start(
                    shell: shell,
                    cols: cols,
                    rows: rows,
                    agentId: agent.id,
                    kernel: kernel
                )
                lock.lock()
                sessions[agent.id] = session
                lock.unlock()
                return .object([
                    "ok": .bool(true),
                    "running": .bool(true),
                    "pid": .integer(Int64(session.pid)),
                ])
            } catch {
                return .object([
                    "error": .string("pty boot failed: \(error)"),
                    "reason": .string("pty_failed"),
                ])
            }
        }

        private func inputVerb(agentId: AgentId, payload: JSON) -> JSON {
            guard let session = sessionFor(agentId) else {
                return .object(["error": .string("no session"), "reason": .string("not_booted")])
            }
            guard let data = payload["data"].asString else {
                return .object(["error": .string("input requires data")])
            }
            session.write(data: data.data(using: .utf8) ?? Data())
            return .object(["ok": .bool(true)])
        }

        private func pasteImageVerb(agentId: AgentId, payload: JSON) -> JSON {
            guard let session = sessionFor(agentId) else {
                return .object(["error": .string("no session"), "reason": .string("not_booted")])
            }
            guard let b64 = payload["data_base64"].asString,
                let bytes = Data(base64Encoded: b64)
            else {
                return .object(["error": .string("paste_image requires data_base64")])
            }
            session.write(data: bytes)
            return .object(["ok": .bool(true), "bytes": .integer(Int64(bytes.count))])
        }

        private func resizeVerb(agentId: AgentId, payload: JSON) -> JSON {
            guard let session = sessionFor(agentId) else {
                return .object(["error": .string("no session"), "reason": .string("not_booted")])
            }
            let cols = UInt16(payload["cols"].asInt ?? 80)
            let rows = UInt16(payload["rows"].asInt ?? 24)
            session.resize(cols: cols, rows: rows)
            return .object(["ok": .bool(true)])
        }

        private func shutdownVerb(agentId: AgentId) {
            lock.lock()
            let session = sessions.removeValue(forKey: agentId)
            lock.unlock()
            session?.terminate()
        }

        private func sessionFor(_ id: AgentId) -> PtySession? {
            lock.lock()
            defer { lock.unlock() }
            return sessions[id]
        }
    }

    // MARK: - PtySession

    final class PtySession: @unchecked Sendable {
        let pid: pid_t
        private let masterFd: Int32
        private let agentId: AgentId
        private weak var kernel: Kernel?
        private let queue = DispatchQueue(label: "pty.read")
        private var stopped = false

        init(pid: pid_t, masterFd: Int32, agentId: AgentId, kernel: Kernel) {
            self.pid = pid
            self.masterFd = masterFd
            self.agentId = agentId
            self.kernel = kernel
            startReadPump()
        }

        static func start(
            shell: String,
            cols: UInt16,
            rows: UInt16,
            agentId: AgentId,
            kernel: Kernel
        ) throws -> PtySession {
            // `forkpty()` does openpty + fork + setsid + setsctty in
            // one call. The child returns with stdin/stdout/stderr
            // already wired to the slave; the parent gets the master
            // fd + the child pid. Available on macOS via Darwin.
            var master: Int32 = 0
            var winsize = winsize(ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0)
            let pid = forkpty(&master, nil, nil, &winsize)
            if pid < 0 {
                throw NSError(
                    domain: "forkpty", code: Int(errno),
                    userInfo: [NSLocalizedDescriptionKey: "forkpty failed"])
            }
            if pid == 0 {
                // Child — exec the shell. After forkpty, stdin/out/err
                // are already wired to the slave PTY.
                let shellCStr = strdup(shell)
                var argv: [UnsafeMutablePointer<CChar>?] = [shellCStr, nil]
                execvp(shellCStr, &argv)
                _exit(127)
            }
            // Parent.
            return PtySession(pid: pid, masterFd: master, agentId: agentId, kernel: kernel)
        }

        private func startReadPump() {
            queue.async { [weak self] in
                self?.readLoop()
            }
        }

        private func readLoop() {
            var buffer = [UInt8](repeating: 0, count: 8192)
            while !stopped {
                let n = read(masterFd, &buffer, buffer.count)
                if n <= 0 { break }
                let chunk = Data(buffer.prefix(n))
                guard let text = String(data: chunk, encoding: .utf8) else {
                    // Skip non-UTF-8 chunks (rare; usually a mid-codepoint split).
                    continue
                }
                guard let kernel = kernel else { break }
                Task { [agentId, text] in
                    await kernel.emit(
                        agentId,
                        .object([
                            "type": .string("output"),
                            "chunk": .string(text),
                        ]))
                }
            }
        }

        func write(data: Data) {
            data.withUnsafeBytes { ptr in
                let baseAddress = ptr.baseAddress
                _ = Darwin.write(masterFd, baseAddress, data.count)
            }
        }

        func resize(cols: UInt16, rows: UInt16) {
            var winsize = winsize(ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0)
            _ = ioctl(masterFd, TIOCSWINSZ, &winsize)
        }

        func terminate() {
            stopped = true
            kill(pid, SIGTERM)
            close(masterFd)
        }

        deinit {
            terminate()
        }
    }

#endif  // os(macOS)
