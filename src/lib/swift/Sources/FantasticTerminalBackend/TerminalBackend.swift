// terminal_backend — a PTY shell session as an agent (macOS only).
//
// Python `terminal_backend/tools.py`, verb for verb: one PTY per agent,
// process-memory state only. Output is emitted as `{type:"output", data}` on
// the agent's own inbox (a client watches it); `{type:"closed"}` when the
// child exits. The synchronous `shell` verb drains via the SCROLLBACK, not the
// emit stream, and finds its done-token there.
//
// Flow control (VSCode's integrated terminal, as python ports it): the reader
// counts every char it emits; past HIGH_WATERMARK unacknowledged chars it
// PAUSES (stops reading — the kernel PTY buffer fills and the shell throttles).
// A streaming consumer acks each chunk after xterm parses it; once the backlog
// drains below LOW_WATERMARK the reader resumes. `shell` is exempt: no pause
// while a done-token is pending, and the accounting is cleared around it.
//
// iOS forbids fork, so this file is macOS-only via `#if`.

#if os(macOS)

    import Darwin
    import FantasticJSON
    import FantasticKernel
    import Foundation

    public let HANDLER_MODULE = "terminal_backend.tools"

    /// Python `MAX_SCROLLBACK`.
    let MAX_SCROLLBACK = 256 * 1024
    let DEFAULT_COLS = 200
    let DEFAULT_ROWS = 50
    /// Python `HIGH_WATERMARK` / `LOW_WATERMARK`.
    let HIGH_WATERMARK = 100_000
    let LOW_WATERMARK = 5_000

    public final class TerminalBackendBundle: AgentBundle, @unchecked Sendable {
        public let name = "terminal_backend"
        public init() {}

        private let lock = NSLock()
        private var sessions: [AgentId: PtySession] = [:]

        public var readme: String? {
            """
            terminal_backend — PTY shell session as an agent. One PTY per agent; process-memory state only.
            Verbs: reflect, boot, write, ack, paste_image, shell, resize, output, restart, signal, stop. Output streams to this agent's own inbox as {type:"output", data} (a client watches it); {type:"closed"} when the child exits.
            paste_image rides the BINARY channel: header {type:"paste_image", mime?} + blob = the image bytes (raw, never base64; 5 MB cap). Saves the image to a scratch file and types its path + a space into the PTY. Returns {path, bytes}.
            """
        }

        public func handle(
            agentId: AgentId,
            payload: JSON,
            kernel: Kernel
        ) async throws -> JSON? {
            guard let agent = kernel.agent(agentId) else {
                return .object(["error": .string("no agent")])
            }
            switch payload["type"].asString {
            case "reflect":
                return reflectVerb(agent: agent)
            case "boot":
                // Python `_boot`: spawn if not running, clear stale flow-control
                // state so a freshly-mounted streamer starts clean.
                if let err = spawn(agent: agent, kernel: kernel) { return err }
                sessionFor(agent.id)?.clearFlowControl()
                return .object(["running": .bool(true)])
            case "write":
                guard let session = sessionFor(agent.id) else { return notRunning }
                let data = payload["data"].asString ?? ""
                let written = await session.writeAll(Data(data.utf8))
                return .object(["written": .integer(Int64(written))])
            case "ack":
                guard let session = sessionFor(agent.id) else { return notRunning }
                let (unacked, paused) = session.ack(chars: pyInt(payload["chars"]) ?? 0)
                return .object(["unacked": .integer(Int64(unacked)), "paused": .bool(paused)])
            case "paste_image":
                // The image is the blob of a binary frame; the text channel carries
                // no bytes, and python says exactly this about a `data` that is not.
                guard sessionFor(agent.id) != nil else { return notRunning }
                return .object(["error": .string("paste_image: data must be bytes")])
            case "shell":
                return await shellVerb(agent: agent, payload: payload, kernel: kernel)
            case "resize":
                guard let session = sessionFor(agent.id) else { return notRunning }
                let cols = pyInt(payload["cols"]) ?? DEFAULT_COLS
                let rows = pyInt(payload["rows"]) ?? DEFAULT_ROWS
                session.resize(cols: cols, rows: rows)
                return .object([
                    "resized": .bool(true), "cols": .integer(Int64(cols)), "rows": .integer(Int64(rows)),
                ])
            case "output":
                let maxBytes = pyInt(payload["max_bytes"]) ?? MAX_SCROLLBACK
                var text = sessionFor(agent.id)?.scrollbackText() ?? ""
                if maxBytes < text.utf8.count {
                    // Python slices CODE POINTS: `text[-max_bytes:]`.
                    text = String(String.UnicodeScalarView(text.unicodeScalars.suffix(maxBytes)))
                }
                return .object(["output": .string(text)])
            case "restart":
                cleanup(agentId: agent.id)
                if let err = spawn(agent: agent, kernel: kernel) { return err }
                return .object(["restarted": .bool(true)])
            case "signal":
                guard let session = sessionFor(agent.id) else { return notRunning }
                let sig = pyInt(payload["signal"]) ?? Int(SIGINT)
                if kill(session.pid, Int32(sig)) != 0 {
                    return .object(["error": .string(osError())])
                }
                return .object(["signal": .integer(Int64(sig))])
            case "stop":
                cleanup(agentId: agent.id)
                return .object(["stopped": .bool(true)])
            case "shutdown":
                // A lifecycle no-op; the PTY dies in `onShutdown`/`onDelete`.
                return .null
            default:
                let t = payload["type"]
                let r = t == .null ? "None" : t.asString.map(pyRepr) ?? t.serialize()
                return .object(["error": .string("terminal: unknown type \(r)")])
            }
        }

        public func handleBinary(
            agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
        ) async throws -> (JSON?, Data) {
            let verb = header["type"].asString ?? "(none)"
            guard verb == "paste_image" else {
                return (
                    .object([
                        "error": .string(
                            "\(name): no binary edge — verb \"\(verb)\" does not accept binary frames"
                        )
                    ]), Data()
                )
            }
            guard kernel.agent(agentId) != nil else {
                return (.object(["error": .string("no agent")]), Data())
            }
            return (await pasteImage(agentId: agentId, header: header, blob: blob), Data())
        }

        public func onShutdown(agentId: AgentId, kernel: Kernel) async throws {
            cleanup(agentId: agentId)
        }

        /// Python `on_delete`: the PTY must not outlive its agent record.
        public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
            cleanup(agentId: agentId)
        }

        // MARK: - verbs

        private var notRunning: JSON { .object(["error": .string("not running")]) }

        /// Python `_reflect`: identity + live PTY state.
        private func reflectVerb(agent: Agent) -> JSON {
            let s = sessionFor(agent.id)
            let command =
                s?.cmd ?? agent.metaValue(forKey: "command")?.asString ?? detectShell()
            let cols = s?.cols ?? pyInt(agent.metaValue(forKey: "cols") ?? .null) ?? DEFAULT_COLS
            let rows = s?.rows ?? pyInt(agent.metaValue(forKey: "rows") ?? .null) ?? DEFAULT_ROWS
            let flow = s?.flowSnapshot() ?? (unacked: 0, paused: false, scrollbackBytes: 0)
            return [
                "id": .string(agent.id.value),
                "sentence": .string("PTY shell session."),
                "command": .string(command),
                "running": .bool(s != nil),
                "pid": s.map { .integer(Int64($0.pid)) } ?? .null,
                "cols": .integer(Int64(cols)),
                "rows": .integer(Int64(rows)),
                "scrollback_bytes": .integer(Int64(flow.scrollbackBytes)),
                "paused": .bool(flow.paused),
                "unacked": .integer(Int64(flow.unacked)),
                "verbs": [
                    "reflect": "Identity + PTY state. No args. `running`/`scrollback_bytes`/`paused`/`unacked` are process-local — read via the live serve to get truth.",
                    "boot": "Idempotent. Spawns the PTY child if not already running; clears stale flow-control state so a freshly-mounted streamer starts clean. Returns {running:true}.",
                    "write": "args: data:str (req). Writes UTF-8 bytes to PTY stdin IN FULL — loops over the non-blocking fd so large pastes aren't truncated; serialized per-agent so concurrent writes can't interleave a bracketed-paste sequence. Returns {written:int}.",
                    "ack": "args: chars:int. Flow-control ack from a streaming consumer (terminal_view (the TS frontend) acks each chunk after xterm parses it). Decrements the unacked-char count; re-attaches the PTY reader once it drops below LOW_WATERMARK. Returns {unacked:int, paused:bool}.",
                    "paste_image": "args: data:bytes (req), mime:str? (default image/png). Saves a pasted image to a per-agent scratch file and types its absolute path into the PTY. Bridges image paste for a CLI (e.g. claude) running in a browser xterm — the server can't reach the browser clipboard, so path injection mimics a file drag-drop. Returns {path:str, bytes:int}.",
                    "shell": "args: cmd:str (req), timeout:float? (default 30s). Synchronous run via done-token; returns {cmd, output, completed:bool, error?}.",
                    "resize": "args: cols:int, rows:int. Fires SIGWINCH so TUI apps redraw. Returns {resized, cols, rows}.",
                    "output": "args: max_bytes:int? (default MAX_SCROLLBACK=256K). Returns {output:str} — tail of the PTY scrollback.",
                    "restart": "No args. SIGKILLs the PTY child and respawns with the same command/cwd/cols/rows. Returns {restarted:true}.",
                    "signal": "args: signal:int? (default SIGINT=2). os.kill(pid, signal). Returns {signal:int} or {error}.",
                    "stop": "No args. Closes the PTY fd and SIGKILLs the child. Returns {stopped:true}.",
                ] as JSON,
                "emits": [
                    "output": "{type:'output', data:str} — every PTY read chunk; UTF-8-decoded bytes",
                    "closed": "{type:'closed'} — child process exited (EOF on the PTY fd)",
                ] as JSON,
            ] as JSON
        }

        /// Python `_spawn`: no-op when running; else fork the PTY child from the
        /// record (`command`, `args`, `cwd`, `cols`, `rows`). Returns the error
        /// reply when the fork itself fails.
        private func spawn(agent: Agent, kernel: Kernel) -> JSON? {
            if sessionFor(agent.id) != nil { return nil }
            let cmd = agent.metaValue(forKey: "command")?.asString ?? detectShell()
            let args = (agent.metaValue(forKey: "args")?.asArray ?? []).compactMap { $0.asString }
            let cwd = agent.metaValue(forKey: "cwd")?.asString ?? FileManager.default.currentDirectoryPath
            let cols = pyInt(agent.metaValue(forKey: "cols") ?? .null) ?? DEFAULT_COLS
            let rows = pyInt(agent.metaValue(forKey: "rows") ?? .null) ?? DEFAULT_ROWS
            do {
                let session = try PtySession.start(
                    cmd: cmd, args: args, cwd: cwd, cols: cols, rows: rows,
                    agentId: agent.id, kernel: kernel)
                // EOF on the master: python detaches, emits `closed`, cleans up.
                session.onExit = { [weak self] in self?.cleanup(agentId: agent.id) }
                lock.lock()
                sessions[agent.id] = session
                lock.unlock()
                return nil
            } catch {
                return .object(["error": .string("\(error)"), "reason": .string("pty_failed")])
            }
        }

        /// Python `_shell`: run one command line and collect its output from the
        /// scrollback, delimited by a done-token the shell prints after it.
        private func shellVerb(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
            guard let session = sessionFor(agent.id) else {
                return .object(["error": .string("shell: not running; call boot first")])
            }
            let cmd = payload["cmd"].asString ?? ""
            if cmd.isEmpty { return .object(["error": .string("shell: cmd required")]) }
            let timeout = payload["timeout"].asDouble ?? payload["timeout"].asInt.map(Double.init) ?? 30.0
            // `shell` drains via scrollback, not the emit stream — it can't be left
            // at the mercy of a streamer's stale flow-control pause (a detached
            // reader means the done-token never lands → timeout). Clear on the
            // way in, and again on the way out so this call's own output doesn't
            // strand the reader paused for the next streamer.
            session.clearFlowControl()
            let token = "__DONE_\(hexToken(8))__"
            let waiter = session.addToken(token)
            defer {
                session.removeToken(token)
                session.clearFlowControl()
            }
            let before = Array(session.scrollbackText().unicodeScalars)
            let line = "\(cmd); printf '\\n%s\\n' '\(token)'\n"
            _ = await session.writeAll(Data(line.utf8))
            var completed = await waiter.wait(seconds: timeout)
            if !completed {
                // Still running — Ctrl-C so the PTY shell is ready for the next
                // call; then a brief grace for a token that was already printing.
                session.writeRaw(Data([0x03]))
                _ = await waiter.wait(seconds: 0.5)
            }
            let after = Array(session.scrollbackText().unicodeScalars)
            let delta: [Unicode.Scalar] =
                after.count >= before.count && Array(after.prefix(before.count)) == before
                ? Array(after.dropFirst(before.count)) : after
            // The token appears TWICE: in the echoed command line (followed by
            // `'`) and as printf's output (followed by a newline). Match the
            // second and slice before it, back to the printf's leading newline.
            var output = delta
            let tokIdx = find(Array((token + "\r\n").unicodeScalars), in: delta)
                ?? find(Array((token + "\n").unicodeScalars), in: delta)
            if let tokIdx {
                let lineStart = delta[..<tokIdx].lastIndex(of: "\n")
                output = Array(delta[..<(lineStart ?? tokIdx)])
                while let last = output.last, last == "\r" || last == "\n" { output.removeLast() }
            }
            let text = String(String.UnicodeScalarView(output))
            if !completed {
                return .object([
                    "cmd": .string(cmd), "output": .string(text),
                    "completed": .bool(false), "error": .string("timeout"),
                ])
            }
            return .object(["cmd": .string(cmd), "output": .string(text), "completed": .bool(true)])
        }

        /// Binary-channel `paste_image` (python `_paste_image`): save the blob to
        /// a per-session scratch file and type its absolute path + one trailing
        /// space into the PTY — no newline, so pasting an image never submits.
        private func pasteImage(agentId: AgentId, header: JSON, blob: Data) async -> JSON {
            guard let session = sessionFor(agentId) else { return notRunning }
            if blob.count > MAX_PASTE_IMAGE {
                return .object([
                    "error": .string("paste_image: \(blob.count) bytes exceeds the 5 MB cap")
                ])
            }
            let mime = (header["mime"].asString ?? "image/png").lowercased()
            guard let ext = IMAGE_EXT[mime] else {
                return .object([
                    "error": .string("paste_image: unsupported image type \(pyRepr(mime))")
                ])
            }
            let dir: String
            do {
                dir = try session.pasteDirectory(agentId: agentId)
            } catch {
                return .object(["error": .string("\(error)")])
            }
            let path = (dir as NSString).appendingPathComponent("paste_\(hexToken(4)).\(ext)")
            do {
                try blob.write(to: URL(fileURLWithPath: path))
            } catch {
                return .object(["error": .string("\(error)")])
            }
            _ = await session.writeAll(Data((path + " ").utf8))
            return .object(["path": .string(path), "bytes": .integer(Int64(blob.count))])
        }

        /// Python `_cleanup`: close the fd, SIGKILL, reap, drop the paste dir.
        /// Idempotent.
        private func cleanup(agentId: AgentId) {
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

    // MARK: - helpers

    /// Image formats a pasted blob may carry, keyed by MIME (python `_IMAGE_EXT`).
    private let IMAGE_EXT: [String: String] = [
        "image/png": "png",
        "image/jpeg": "jpg",
        "image/gif": "gif",
        "image/webp": "webp",
    ]
    /// Per-image cap (python `MAX_PASTE_IMAGE`).
    private let MAX_PASTE_IMAGE = 5 * 1024 * 1024

    /// Python `secrets.token_hex(n)`: 2n hex chars.
    private func hexToken(_ n: Int) -> String {
        (0..<n).map { _ in String(format: "%02x", UInt8.random(in: .min ... .max)) }.joined()
    }

    /// Python `_detect_shell`: `$SHELL` if it is a file, else the first of
    /// zsh/bash/sh, else `/bin/sh`.
    func detectShell() -> String {
        let fm = FileManager.default
        if let sh = ProcessInfo.processInfo.environment["SHELL"], !sh.isEmpty, fm.fileExists(atPath: sh) {
            return sh
        }
        for cand in ["/bin/zsh", "/bin/bash", "/bin/sh"] where fm.fileExists(atPath: cand) {
            return cand
        }
        return "/bin/sh"
    }

    /// Python's `int(x)` of a payload field: an int, a float's integer part, a
    /// numeric string; anything else is nil (the caller's default applies).
    private func pyInt(_ v: JSON) -> Int? {
        switch v {
        case .integer(let i): return Int(i)
        case .double(let d): return Int(d)
        case .bool(let b): return b ? 1 : 0
        case .string(let s): return Int(s.trimmingCharacters(in: .whitespacesAndNewlines))
        default: return nil
        }
    }

    /// Python's `str(OSError)`: `[Errno 3] No such process`.
    private func osError() -> String {
        let e = errno
        return "[Errno \(e)] \(String(cString: strerror(e)))"
    }

    /// Python's `repr` of a str.
    private func pyRepr(_ s: String) -> String {
        let q: Character = (s.contains("'") && !s.contains("\"")) ? "\"" : "'"
        var out = String(q)
        for c in s {
            if c == "\\" { out += "\\\\" } else if c == q { out += "\\" + String(q) }
            else if c == "\n" { out += "\\n" } else { out.append(c) }
        }
        out.append(q)
        return out
    }

    /// First index of `needle` in `hay` (python `str.find`).
    private func find(_ needle: [Unicode.Scalar], in hay: [Unicode.Scalar]) -> Int? {
        guard !needle.isEmpty, hay.count >= needle.count else { return nil }
        for i in 0...(hay.count - needle.count) where hay[i] == needle[0] {
            if Array(hay[i..<(i + needle.count)]) == needle { return i }
        }
        return nil
    }

    /// One done-token a `shell` call is waiting on (python's `asyncio.Event`).
    final class TokenWaiter: @unchecked Sendable {
        private let lock = NSLock()
        private var fired = false
        private var continuation: CheckedContinuation<Bool, Never>?

        func fire() {
            lock.lock()
            fired = true
            let c = continuation
            continuation = nil
            lock.unlock()
            c?.resume(returning: true)
        }

        /// Park `c` unless the token already landed (then `false`: resume it now).
        private func arm(_ c: CheckedContinuation<Bool, Never>) -> Bool {
            lock.lock()
            defer { lock.unlock() }
            if fired { return false }
            continuation = c
            return true
        }

        private func takePending() -> CheckedContinuation<Bool, Never>? {
            lock.lock()
            defer { lock.unlock() }
            let c = continuation
            continuation = nil
            return c
        }

        /// True when the token landed within `seconds`; false on timeout. The
        /// waiter stays fired, so a second wait after a late token is true.
        func wait(seconds: Double) async -> Bool {
            await withCheckedContinuation { (c: CheckedContinuation<Bool, Never>) in
                guard arm(c) else { return c.resume(returning: true) }
                Task { [weak self] in
                    try? await Task.sleep(nanoseconds: UInt64(max(0, seconds) * 1_000_000_000))
                    self?.takePending()?.resume(returning: false)
                }
            }
        }
    }

    // MARK: - PtySession

    /// Python's per-agent `_procs[id]` state, plus the reader thread.
    final class PtySession: @unchecked Sendable {
        let pid: pid_t
        let cmd: String
        private var masterFd: Int32
        private let agentId: AgentId
        private weak var kernel: Kernel?
        private let readQueue = DispatchQueue(label: "pty.read")
        /// Python `write_lock`: writes never interleave (a bracketed paste
        /// split by another write leaves the shell stuck in paste mode).
        private let writeQueue = DispatchQueue(label: "pty.write")
        private let state = NSCondition()
        private var stopped = false
        private(set) var cols: Int
        private(set) var rows: Int
        // ── under `state` ──
        private var scrollback: [String] = []
        private var scrollbackBytes = 0
        private var unacked = 0
        private var paused = false
        private var pendingTokens: [String: TokenWaiter] = [:]
        /// Bytes of a multi-byte char split across two reads (python's
        /// incremental decoder): box-drawing glyphs straddle 4 K boundaries
        /// routinely, and a naive per-chunk decode litters `<?>`.
        private var pendingBytes: [UInt8] = []
        /// Lazily-minted scratch dir for pasted images (python `paste_dir`).
        private var pasteDir: String?
        /// The bundle's hook for EOF (python's `_cleanup` from `_on_readable`).
        var onExit: (() -> Void)?

        private init(
            pid: pid_t, masterFd: Int32, cmd: String, cols: Int, rows: Int, agentId: AgentId,
            kernel: Kernel
        ) {
            self.pid = pid
            self.masterFd = masterFd
            self.cmd = cmd
            self.cols = cols
            self.rows = rows
            self.agentId = agentId
            self.kernel = kernel
            readQueue.async { [weak self] in self?.readLoop() }
        }

        /// Python `_spawn`'s fork: `forkpty` + `execve` of the resolved command
        /// with `TERM=xterm-256color`, in `cwd`. Everything the child needs is
        /// built BEFORE the fork — only async-signal-safe calls run after it.
        static func start(
            cmd: String, args: [String], cwd: String, cols: Int, rows: Int,
            agentId: AgentId, kernel: Kernel
        ) throws -> PtySession {
            let path = resolve(cmd)
            var env = ProcessInfo.processInfo.environment
            env["TERM"] = "xterm-256color"
            let argv: [UnsafeMutablePointer<CChar>?] = ([cmd] + args).map { strdup($0) } + [nil]
            let envp: [UnsafeMutablePointer<CChar>?] = env.map { strdup("\($0.key)=\($0.value)") } + [nil]
            let cwdC = strdup(cwd)
            let pathC = strdup(path)
            defer {
                argv.forEach { free($0) }
                envp.forEach { free($0) }
                free(cwdC)
                free(pathC)
            }
            var master: Int32 = 0
            var winsize = winsize(ws_row: UInt16(rows), ws_col: UInt16(cols), ws_xpixel: 0, ws_ypixel: 0)
            let pid = forkpty(&master, nil, nil, &winsize)
            if pid < 0 {
                throw NSError(
                    domain: "forkpty", code: Int(errno),
                    userInfo: [NSLocalizedDescriptionKey: "forkpty failed"])
            }
            if pid == 0 {
                // Child: stdin/out/err are the slave already. Every OTHER fd of
                // the kernel process — sockets, other sessions' masters, an
                // exec_bridge control socket — is closed before exec: python's
                // fds are non-inheritable, so its PTY children never hold a
                // peer's lifeline open (an exec_bridge child self-reaps on EOF of
                // a socket a shell here must not be keeping alive). An exec
                // failure must never unwind into the parent's code — exit 127
                // like a shell, the message to the PTY (fd 2 is the slave).
                let maxFd = Int32(min(getdtablesize(), 65536))
                if maxFd > 3 { for fd in 3..<maxFd { _ = close(fd) } }
                _ = chdir(cwdC)
                var a = argv
                var e = envp
                execve(pathC, &a, &e)
                let msg = "\(cmd): [Errno \(errno)] \(String(cString: strerror(errno)))\r\n"
                _ = msg.withCString { Darwin.write(2, $0, strlen($0)) }
                _exit(127)
            }
            return PtySession(
                pid: pid, masterFd: master, cmd: cmd, cols: cols, rows: rows, agentId: agentId,
                kernel: kernel)
        }

        /// `execvpe`'s PATH search, done in the parent.
        private static func resolve(_ cmd: String) -> String {
            if cmd.contains("/") { return cmd }
            let path = ProcessInfo.processInfo.environment["PATH"] ?? "/usr/bin:/bin"
            for dir in path.split(separator: ":") {
                let cand = "\(dir)/\(cmd)"
                if FileManager.default.isExecutableFile(atPath: cand) { return cand }
            }
            return cmd
        }

        // ── the reader (python `_on_readable`) ──

        private func readLoop() {
            var buffer = [UInt8](repeating: 0, count: 4096)
            while true {
                // PAUSED: the reader is detached — nothing is read until acks
                // drain the backlog (or `shell` / `boot` clears it).
                state.lock()
                while paused && !stopped { state.wait() }
                let fd = masterFd
                let done = stopped
                state.unlock()
                if done { return }
                let n = read(fd, &buffer, buffer.count)
                if n <= 0 {
                    // EOF — the child exited. Python: detach, emit closed, cleanup.
                    state.lock()
                    let wasStopped = stopped
                    state.unlock()
                    if !wasStopped, let kernel = kernel {
                        Task { [agentId] in await kernel.emit(agentId, .object(["type": .string("closed")])) }
                        onExit?()
                    }
                    return
                }
                let text = decode(Array(buffer.prefix(n)))
                if text.isEmpty { continue }
                state.lock()
                appendScrollback(text)
                // Signal any pending done-token: `<token>\r\n` (printf's output),
                // never the echoed command line (`<token>'`).
                var fire: [TokenWaiter] = []
                if !pendingTokens.isEmpty {
                    let full = scrollback.joined()
                    for (tok, w) in pendingTokens
                    where full.contains(tok + "\r\n") || full.contains(tok + "\n") {
                        fire.append(w)
                    }
                }
                // Flow control: account the emitted chars; pause when the
                // streaming consumer is falling behind — never under a `shell`
                // call, which drains via scrollback and never acks.
                unacked += text.unicodeScalars.count
                if pendingTokens.isEmpty && !paused && unacked > HIGH_WATERMARK { paused = true }
                state.unlock()
                fire.forEach { $0.fire() }
                if let kernel = kernel {
                    Task { [agentId, text] in
                        await kernel.emit(agentId, .object(["type": .string("output"), "data": .string(text)]))
                    }
                }
            }
        }

        /// Incremental UTF-8: decode what is complete, keep a split tail for
        /// the next read, replace what is invalid (python's `errors="replace"`).
        private func decode(_ chunk: [UInt8]) -> String {
            var bytes = pendingBytes + chunk
            pendingBytes = []
            // A trailing incomplete sequence: a lead byte whose length overruns
            // the buffer. At most 3 bytes are ever held back.
            var i = bytes.count - 1
            var held = 0
            while i >= 0 && held < 3 {
                let b = bytes[i]
                if b & 0xC0 == 0x80 {
                    held += 1
                    i -= 1
                    continue
                }
                let need = b >= 0xF0 ? 4 : b >= 0xE0 ? 3 : b >= 0xC0 ? 2 : 1
                if need > held + 1 { pendingBytes = Array(bytes[i...]); bytes = Array(bytes[..<i]) }
                break
            }
            return String(decoding: bytes, as: UTF8.self)
        }

        /// Python `_append_scrollback`: bounded by UTF-8 bytes, oldest out first.
        private func appendScrollback(_ text: String) {
            scrollback.append(text)
            scrollbackBytes += text.utf8.count
            while scrollbackBytes > MAX_SCROLLBACK && !scrollback.isEmpty {
                scrollbackBytes -= scrollback.removeFirst().utf8.count
            }
        }

        func scrollbackText() -> String {
            state.lock()
            defer { state.unlock() }
            return scrollback.joined()
        }

        func flowSnapshot() -> (unacked: Int, paused: Bool, scrollbackBytes: Int) {
            state.lock()
            defer { state.unlock() }
            return (unacked, paused, scrollbackBytes)
        }

        /// Python `_ack`.
        func ack(chars: Int) -> (unacked: Int, paused: Bool) {
            state.lock()
            defer { state.unlock() }
            unacked = max(0, unacked - chars)
            if paused && unacked < LOW_WATERMARK {
                paused = false
                state.broadcast()
            }
            return (unacked, paused)
        }

        /// Python `_clear_flow_control`: reset the accounting, resume the reader.
        func clearFlowControl() {
            state.lock()
            defer { state.unlock() }
            unacked = 0
            if paused {
                paused = false
                state.broadcast()
            }
        }

        func addToken(_ token: String) -> TokenWaiter {
            let w = TokenWaiter()
            state.lock()
            pendingTokens[token] = w
            state.unlock()
            return w
        }

        func removeToken(_ token: String) {
            state.lock()
            pendingTokens.removeValue(forKey: token)
            state.unlock()
        }

        // ── writes (python `_write_all` under `write_lock`) ──

        /// Write the FULL buffer, serialized per session: a short write is
        /// retried from where it stopped, a full PTY input queue is waited out.
        func writeAll(_ data: Data) async -> Int {
            await withCheckedContinuation { (c: CheckedContinuation<Int, Never>) in
                writeQueue.async { [self] in
                    let fd = fdForWrite()
                    guard fd >= 0 else { return c.resume(returning: 0) }
                    var written = 0
                    data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
                        guard let base = raw.baseAddress else { return }
                        while written < data.count {
                            let n = Darwin.write(fd, base + written, data.count - written)
                            if n > 0 { written += n } else if n < 0 && errno == EAGAIN { usleep(1000) } else { break }
                        }
                    }
                    c.resume(returning: written)
                }
            }
        }

        /// One raw byte string, unserialized (python's bare `os.write` for ^C).
        func writeRaw(_ data: Data) {
            let fd = fdForWrite()
            guard fd >= 0 else { return }
            data.withUnsafeBytes { raw in
                if let base = raw.baseAddress { _ = Darwin.write(fd, base, data.count) }
            }
        }

        private func fdForWrite() -> Int32 {
            state.lock()
            defer { state.unlock() }
            return stopped ? -1 : masterFd
        }

        func resize(cols: Int, rows: Int) {
            state.lock()
            self.cols = cols
            self.rows = rows
            let fd = masterFd
            state.unlock()
            var winsize = winsize(ws_row: UInt16(rows), ws_col: UInt16(cols), ws_xpixel: 0, ws_ypixel: 0)
            _ = ioctl(fd, TIOCSWINSZ, &winsize)
        }

        /// Scratch dir for pasted images, created on first use — most sessions
        /// never paste, so no temp dir is minted per spawn (python `mkdtemp`).
        func pasteDirectory(agentId: AgentId) throws -> String {
            state.lock()
            defer { state.unlock() }
            if let dir = pasteDir { return dir }
            let dir = (NSTemporaryDirectory() as NSString).appendingPathComponent(
                "fantastic_paste_\(agentId.value)_\(hexToken(4))")
            try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
            pasteDir = dir
            return dir
        }

        /// Python `_cleanup`, ONCE: SIGKILL + `waitpid` (no zombie), the master
        /// closed, the paste dir dropped. Order is kill → waitpid → close: the
        /// reader is a thread parked in `read(masterFd)`, and on macOS `close()`
        /// of a PTY master with a pending read blocks; killing the session
        /// leader first revokes the slave, the read returns, close is immediate.
        func terminate() {
            state.lock()
            if stopped {
                state.unlock()
                return
            }
            stopped = true
            let fd = masterFd
            masterFd = -1
            let dir = pasteDir
            pasteDir = nil
            let waiters = Array(pendingTokens.values)
            pendingTokens = [:]
            state.broadcast()
            state.unlock()
            kill(pid, SIGKILL)
            var status: Int32 = 0
            waitpid(pid, &status, 0)
            if fd >= 0 { close(fd) }
            if let dir { try? FileManager.default.removeItem(atPath: dir) }
            // A `shell` waiting on a token of a dead session times out on its own;
            // nothing to fire.
            _ = waiters
        }

        deinit {
            terminate()
        }
    }

#endif  // os(macOS)
