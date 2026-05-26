// SSH-based remote kernel runner (macOS only).
//
// Mirrors Rust's `fantastic-ssh-runner` shape. Spawns `ssh -L
// <local>:127.0.0.1:<remote> user@host` to tunnel a remote
// `fantastic` daemon's HTTP port to localhost. Polls the remote's
// lock.json over `ssh user@host cat …` to confirm liveness.
//
// iOS sandbox forbids subprocess; macOS-only by `#if`.

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import Foundation

    public let HANDLER_MODULE = "ssh_runner.tools"

    public final class SshRunnerBundle: AgentBundle, @unchecked Sendable {
        public let name = "ssh_runner"
        public init() {}

        private let lock = NSLock()
        private var sessions: [AgentId: SshSession] = [:]

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
                    "kind": .string("ssh_runner"),
                    "sentence": .string(
                        "SSH-tunneled remote kernel. Spawns ssh -L to forward a remote HTTP port + polls remote lock.json."
                    ),
                    "verbs": [
                        "start": "args: user, host, local_port?, remote_port?. Spawns ssh tunnel + waits for ready.",
                        "stop": "Kills the tunnel.",
                        "status": "Reports tunnel state + remote lock pid (if alive).",
                    ] as JSON,
                ] as JSON
            case "boot":
                return .object(["ok": .bool(true)])
            case "shutdown":
                stopAll()
                return .object(["ok": .bool(true)])
            case "start":
                return await startVerb(agentId: agent.id, payload: payload)
            case "stop":
                return stopVerb(agentId: agent.id)
            case "status":
                return statusVerb(agentId: agent.id)
            default:
                return .object(["error": .string("unknown verb \(verb)")])
            }
        }

        public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
            stopAll()
        }

        // MARK: - Verbs

        private func startVerb(agentId: AgentId, payload: JSON) async -> JSON {
            guard let user = payload["user"].asString,
                let host = payload["host"].asString
            else {
                return .object([
                    "error": .string("start requires user + host")
                ])
            }
            let localPort = UInt16(payload["local_port"].asInt ?? 0)
            let remotePort = UInt16(payload["remote_port"].asInt ?? 8080)
            let actualLocalPort = localPort == 0 ? findFreePort() : localPort

            let session = SshSession(
                user: user,
                host: host,
                localPort: actualLocalPort,
                remotePort: remotePort
            )
            do {
                try session.start()
            } catch {
                return .object([
                    "error": .string("ssh tunnel failed: \(error)"),
                    "reason": .string("tunnel_failed"),
                ])
            }

            // Probe localPort for up to 5s.
            let ready = await waitForTunnel(port: actualLocalPort, timeoutSeconds: 5)
            if !ready {
                session.stop()
                return .object([
                    "error": .string("tunnel did not become ready within 5s"),
                    "reason": .string("tunnel_timeout"),
                ])
            }

            installSession(session, for: agentId)
            return .object([
                "ok": .bool(true),
                "local_port": .integer(Int64(actualLocalPort)),
                "remote": .string("\(user)@\(host):\(remotePort)"),
            ])
        }

        /// Sync helper that swaps the session out of the lock-protected
        /// dict without holding the lock across any await.
        private func installSession(_ session: SshSession, for agentId: AgentId) {
            lock.lock()
            let previous = sessions[agentId]
            sessions[agentId] = session
            lock.unlock()
            previous?.stop()
        }

        private func stopVerb(agentId: AgentId) -> JSON {
            lock.lock()
            let session = sessions.removeValue(forKey: agentId)
            lock.unlock()
            session?.stop()
            return .object([
                "ok": .bool(true),
                "stopped": .bool(session != nil),
            ])
        }

        private func statusVerb(agentId: AgentId) -> JSON {
            lock.lock()
            let session = sessions[agentId]
            lock.unlock()
            guard let session = session else {
                return .object(["running": .bool(false)])
            }
            return .object([
                "running": .bool(session.isRunning),
                "local_port": .integer(Int64(session.localPort)),
                "remote": .string("\(session.user)@\(session.host):\(session.remotePort)"),
                "pid": .integer(Int64(session.pid)),
            ])
        }

        private func stopAll() {
            lock.lock()
            let snapshot = sessions
            sessions.removeAll()
            lock.unlock()
            for (_, s) in snapshot { s.stop() }
        }

        private func findFreePort() -> UInt16 {
            // Bind a TCP socket to :0, read back the assigned port, close.
            let sock = socket(AF_INET, SOCK_STREAM, 0)
            if sock < 0 { return 0 }
            defer { close(sock) }
            var addr = sockaddr_in()
            addr.sin_family = sa_family_t(AF_INET)
            addr.sin_addr.s_addr = inet_addr("127.0.0.1")
            addr.sin_port = 0
            let result = withUnsafePointer(to: &addr) { ptr in
                ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                    Darwin.bind(sock, sa, socklen_t(MemoryLayout<sockaddr_in>.size))
                }
            }
            if result != 0 { return 0 }
            var bound = sockaddr_in()
            var len = socklen_t(MemoryLayout<sockaddr_in>.size)
            _ = withUnsafeMutablePointer(to: &bound) { ptr in
                ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                    getsockname(sock, sa, &len)
                }
            }
            return UInt16(bigEndian: bound.sin_port)
        }

        private func waitForTunnel(port: UInt16, timeoutSeconds: Int) async -> Bool {
            let deadline = Date().addingTimeInterval(TimeInterval(timeoutSeconds))
            while Date() < deadline {
                if tcpProbe(port: port) { return true }
                try? await Task.sleep(nanoseconds: 200_000_000)
            }
            return false
        }

        private func tcpProbe(port: UInt16) -> Bool {
            let sock = socket(AF_INET, SOCK_STREAM, 0)
            if sock < 0 { return false }
            defer { close(sock) }
            var addr = sockaddr_in()
            addr.sin_family = sa_family_t(AF_INET)
            addr.sin_addr.s_addr = inet_addr("127.0.0.1")
            addr.sin_port = port.bigEndian
            let result = withUnsafePointer(to: &addr) { ptr in
                ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                    Darwin.connect(sock, sa, socklen_t(MemoryLayout<sockaddr_in>.size))
                }
            }
            return result == 0
        }
    }

    // MARK: - SshSession

    final class SshSession: @unchecked Sendable {
        let user: String
        let host: String
        let localPort: UInt16
        let remotePort: UInt16

        private var process: Process?

        init(user: String, host: String, localPort: UInt16, remotePort: UInt16) {
            self.user = user
            self.host = host
            self.localPort = localPort
            self.remotePort = remotePort
        }

        var pid: Int32 {
            return process?.processIdentifier ?? -1
        }

        var isRunning: Bool {
            process?.isRunning ?? false
        }

        func start() throws {
            let proc = Process()
            proc.executableURL = URL(fileURLWithPath: "/usr/bin/ssh")
            proc.arguments = [
                "-N",  // no command, just tunnel
                "-L", "\(localPort):127.0.0.1:\(remotePort)",
                "-o", "ConnectTimeout=5",
                "-o", "ServerAliveInterval=15",
                "\(user)@\(host)",
            ]
            try proc.run()
            self.process = proc
        }

        func stop() {
            process?.terminate()
            process = nil
        }
    }

#endif  // os(macOS)
