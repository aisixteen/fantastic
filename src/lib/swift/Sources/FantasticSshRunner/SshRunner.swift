// SSH-based remote kernel runner (macOS only).
//
// Mirrors Rust's `fantastic-ssh-runner` shape. Spawns `ssh -L
// <local>:127.0.0.1:<remote> user@host` to tunnel a remote `fantastic`
// daemon's HTTP port to localhost. Probes the tunnel's local port to
// confirm liveness.
//
// iOS sandbox forbids subprocess; macOS-only by `#if`.
//
// The shared lifecycle dispatch (reflect/boot/shutdown/start/stop +
// unknown-verb) lives in `FantasticRunnerCore`; this target supplies
// only the ssh `RunnerTransport` conformance (ssh exec + `ssh -L` tunnel
// + Darwin socket probe + per-agent session state) + a thin bundle that
// routes through `RunnerCore`.

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import FantasticRunnerCore
    import Foundation

    public let HANDLER_MODULE = "ssh_runner.tools"

    /// Per-agent ssh tunnel sessions, guarded by a lock. Shared between
    /// the bundle and its per-call transports.
    final class SshRunnerState: @unchecked Sendable {
        private let lock = NSLock()
        private var sessions: [AgentId: SshSession] = [:]

        /// Install a new session, returning the previous one (if any) so
        /// the caller can stop it without holding the lock across stop().
        func install(_ session: SshSession, for agentId: AgentId) -> SshSession? {
            lock.lock()
            let previous = sessions[agentId]
            sessions[agentId] = session
            lock.unlock()
            return previous
        }

        func remove(_ agentId: AgentId) -> SshSession? {
            lock.lock()
            defer { lock.unlock() }
            return sessions.removeValue(forKey: agentId)
        }

        func get(_ agentId: AgentId) -> SshSession? {
            lock.lock()
            defer { lock.unlock() }
            return sessions[agentId]
        }

        func stopAll() {
            lock.lock()
            let snapshot = sessions
            sessions.removeAll()
            lock.unlock()
            for (_, s) in snapshot { s.stop() }
        }
    }

    public final class SshRunnerBundle: AgentBundle, @unchecked Sendable {
        public let name = "ssh_runner"
        public init() {}

        private let state = SshRunnerState()

        public var readme: String? {
            """
            ssh_runner — remote `fantastic` lifecycle over SSH. Each agent is one project on one remote host.
            Verbs: start | stop | status — spawn an `ssh -L` subprocess tunnel and poll the remote lock for liveness.
            """
        }

        public func handle(
            agentId: AgentId,
            payload: JSON,
            kernel: Kernel
        ) async throws -> JSON? {
            // Preserve the pre-refactor guard: every verb requires the
            // agent to exist (the reply uses the live agent's id).
            guard let agent = kernel.agent(agentId) else {
                return .object(["error": .string("no agent")])
            }
            let verb = payload["type"].asString ?? ""
            let transport = SshTransport(agentId: agent.id, payload: payload, state: state)
            return await RunnerCore.handle(verb: verb, transport: transport)
        }

        public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
            state.stopAll()
        }
    }

    /// SSH transport — owns each verb's concrete reply body + the ssh
    /// exec / tunnel-probe machinery. Built per `handle` call over the
    /// bundle's shared session state.
    struct SshTransport: RunnerTransport {
        let agentId: AgentId
        let payload: JSON
        let state: SshRunnerState

        func reflect() async -> JSON {
            [
                "id": .string(agentId.value),
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
        }

        func start() async -> JSON {
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

            let previous = state.install(session, for: agentId)
            previous?.stop()
            return .object([
                "ok": .bool(true),
                "local_port": .integer(Int64(actualLocalPort)),
                "remote": .string("\(user)@\(host):\(remotePort)"),
            ])
        }

        func stop() async -> JSON {
            let session = state.remove(agentId)
            session?.stop()
            return .object([
                "ok": .bool(true),
                "stopped": .bool(session != nil),
            ])
        }

        func shutdownAll() async {
            state.stopAll()
        }

        func handleVerb(_ verb: String) async -> JSON? {
            guard verb == "status" else { return nil }
            guard let session = state.get(agentId) else {
                return .object(["running": .bool(false)])
            }
            return .object([
                "running": .bool(session.isRunning),
                "local_port": .integer(Int64(session.localPort)),
                "remote": .string("\(session.user)@\(session.host):\(session.remotePort)"),
                "pid": .integer(Int64(session.pid)),
            ])
        }

        // MARK: - Port / probe helpers

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
