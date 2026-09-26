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
// What python RAISES, this raises: `int()`/`float()` of a bad arg, the
// `struct.pack("HHHH", …)` range check behind every winsize, and a `.encode`
// on a non-str `write.data` all fail the DISPATCH rather than answering a
// reply the python kernel never writes.
//
// iOS forbids fork, so this file is macOS-only via `#if`.

#if os(macOS)

    import Darwin
    import FantasticFile
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
        public var verbs: [String] { Array((Self.verbDocs.asObject ?? [:]).keys) }

        /// python's module-level `VERBS` — ONE table: reflect renders it and
        /// `advertises` reads it.
        static let verbDocs: JSON = [
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
        ] as JSON
        public init() {}

        private let lock = NSLock()
        private var sessions: [AgentId: PtySession] = [:]

        public var readme: String? { Self.readmeText }

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
                try spawn(agent: agent, kernel: kernel)
                sessionFor(agent.id)?.clearFlowControl(kernel: kernel)
                return .object(["running": .bool(true)])
            case "write":
                guard let session = sessionFor(agent.id) else { return notRunning }
                // Python writes `data.encode("utf-8")`: a non-str has no
                // `.encode`, and the AttributeError fails the dispatch.
                let raw = payload.asObject?["data"] ?? .string("")
                guard case .string(let data) = raw else { throw noEncode(raw) }
                switch await session.writeAll(Data(data.utf8)) {
                case .ok(let written): return .object(["written": .integer(Int64(written))])
                case .failed(let code): return .object(["error": .string(osErrorStr(code))])
                }
            case "ack":
                guard let session = sessionFor(agent.id) else { return notRunning }
                let chars = try pyInt(payload.asObject?["chars"] ?? .integer(0))
                let (unacked, paused) = session.ack(chars: chars, kernel: kernel)
                return .object([
                    "unacked": .integer(Int64(unacked)), "paused": .bool(paused),
                ])
            case "paste_image":
                // The image is the blob of a binary frame; the text channel carries
                // no bytes, and python says exactly this about a `data` that is not.
                guard sessionFor(agent.id) != nil else { return notRunning }
                return .object(["error": .string("paste_image: data must be bytes")])
            case "shell":
                return try await shellVerb(agent: agent, payload: payload, kernel: kernel)
            case "resize":
                guard let session = sessionFor(agent.id) else { return notRunning }
                let cols = try pyInt(payload.asObject?["cols"] ?? .integer(Int64(DEFAULT_COLS)))
                let rows = try pyInt(payload.asObject?["rows"] ?? .integer(Int64(DEFAULT_ROWS)))
                // Python stores FIRST and only then packs the winsize — an
                // out-of-range pack raises with the new cols/rows already kept.
                try session.resize(cols: cols, rows: rows)
                return .object([
                    "resized": .bool(true), "cols": .integer(Int64(cols)),
                    "rows": .integer(Int64(rows)),
                ])
            case "output":
                let maxBytes = try pyInt(
                    payload.asObject?["max_bytes"] ?? .integer(Int64(MAX_SCROLLBACK)))
                let text = sessionFor(agent.id)?.scrollbackText() ?? ""
                return .object([
                    "output": .string(pySliceTail(text, maxBytes)),
                    // HOW TO GET THIS AGAIN — python `_output`'s `origin`.
                    "origin": .string("\(agent.id.value) output — the scrollback NOW; re-send, it moves"),
                ])
            case "restart":
                cleanup(agentId: agent.id)
                try spawn(agent: agent, kernel: kernel)
                return .object(["restarted": .bool(true)])
            case "signal":
                guard let session = sessionFor(agent.id) else { return notRunning }
                let sig = try pyInt(payload.asObject?["signal"] ?? .integer(Int64(SIGINT)))
                // `os.kill(pid, signal)` takes a Py_ssize_t and CASTS it to int:
                // an out-of-int value wraps here exactly as it does there.
                if kill(session.pid, Int32(truncatingIfNeeded: sig)) != 0 {
                    return .object(["error": .string(osErrorStr(errno))])
                }
                return .object(["signal": .integer(Int64(sig))])
            case "stop":
                cleanup(agentId: agent.id)
                return .object(["stopped": .bool(true)])
            default:
                let t = payload["type"]
                return .object(["error": .string("terminal: unknown type \(pyRepr(t))")])
            }
        }

        public func handleBinary(
            agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
        ) async throws -> (JSON?, Data?) {
            let verb = header["type"].asString ?? "(none)"
            guard verb == "paste_image" else {
                return (
                    .object([
                        "error": .string(
                            "\(name): no binary edge — verb \"\(verb)\" does not accept binary frames"
                        )
                    ]), nil
                )
            }
            guard kernel.agent(agentId) != nil else {
                return (.object(["error": .string("no agent")]), nil)
            }
            return (try await pasteImage(agentId: agentId, header: header, blob: blob), nil)
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

        /// Python `'<t>' object has no attribute 'encode'`.
        private func noEncode(_ v: JSON) -> PyError {
            PyError("AttributeError", "'\(pyTypeName(v))' object has no attribute 'encode'")
        }

        /// Python `_reflect`: identity + live PTY state.
        private func reflectVerb(agent: Agent) -> JSON {
            let s = sessionFor(agent.id)
            // `(state or {}).get("cmd") or rec.get("command") or _detect_shell()`
            // — truthiness at every step, not key presence.
            var command = s?.cmd ?? ""
            if command.isEmpty { command = agent.metaValue(forKey: "command")?.asString ?? "" }
            if command.isEmpty { command = detectShell() }
            // Stopped, the record's value is returned RAW: python neither
            // `int()`s it nor falls back on a falsy one.
            let cols =
                s.map { JSON.integer(Int64($0.cols)) }
                ?? agent.metaValue(forKey: "cols") ?? .integer(Int64(DEFAULT_COLS))
            let rows =
                s.map { JSON.integer(Int64($0.rows)) }
                ?? agent.metaValue(forKey: "rows") ?? .integer(Int64(DEFAULT_ROWS))
            let flow = s?.flowSnapshot() ?? (unacked: 0, paused: false, scrollbackBytes: 0)
            return [
                "id": .string(agent.id.value),
                "sentence": .string("PTY shell session."),
                "command": .string(command),
                "running": .bool(s != nil),
                "pid": s.map { .integer(Int64($0.pid)) } ?? .null,
                "cols": cols,
                "rows": rows,
                "scrollback_bytes": .integer(Int64(flow.scrollbackBytes)),
                "paused": .bool(flow.paused),
                "unacked": .integer(Int64(flow.unacked)),
                "verbs": Self.verbDocs,
                "emits": [
                    "output": "{type:'output', data:str} — every PTY read chunk; UTF-8-decoded bytes",
                    "closed": "{type:'closed'} — child process exited (EOF on the PTY fd)",
                ] as JSON,
            ] as JSON
        }

        /// Python `_spawn`: no-op when running; else fork the PTY child from the
        /// record (`command`, `args`, `cwd`, `cols`, `rows` — each read with
        /// python's `or` truthiness, not key presence). A fork failure RAISES,
        /// and so does an out-of-ushort winsize (after the child exists).
        private func spawn(agent: Agent, kernel: Kernel) throws {
            // CHECKED AND STORED UNDER THE SAME LOCK. `if sessionFor(id) != nil`
            // was a check-then-act: two `boot`s for one agent (the create-time
            // auto-boot and an explicit one) both read nil, both forked, and the
            // loser's session was ORPHANED — a live PTY child nothing owned,
            // holding a master fd, whose reader's EOF later removed the WINNER
            // from `sessions` (`running:false, pid:null` on a PTY nobody
            // stopped). Python forks from the one event loop; the window does
            // not exist there.
            lock.lock()
            defer { lock.unlock() }
            if sessions[agent.id] != nil { return }
            let cmd = truthyString(agent.metaValue(forKey: "command")) ?? detectShell()
            let args = (agent.metaValue(forKey: "args")?.asArray ?? []).compactMap { $0.asString }
            let cwd =
                truthyString(agent.metaValue(forKey: "cwd"))
                ?? FileManager.default.currentDirectoryPath
            let cols = try pyIntOr(agent.metaValue(forKey: "cols") ?? .null, DEFAULT_COLS)
            let rows = try pyIntOr(agent.metaValue(forKey: "rows") ?? .null, DEFAULT_ROWS)
            let session = try PtySession.start(
                cmd: cmd, args: args, cwd: cwd, cols: cols, rows: rows,
                agentId: agent.id, kernel: kernel)
            // EOF on the master: python detaches, emits `closed`, cleans up.
            // ITS OWN session, not whatever is registered under the id: the
            // reader reads `stopped` and then calls back OUT of the lock, so a
            // callback that lands after a `stop` + `boot` cycle would otherwise
            // SIGKILL the session `boot` just spawned.
            session.onExit = { [weak self, weak session] in
                guard let session else { return }
                self?.cleanup(agentId: agent.id, ifCurrent: session)
            }
            sessions[agent.id] = session
            session.attachReader(kernel: kernel)
            session.beginReading()
        }

        /// `rec.get(k) or <default>` — a missing key, a null and an empty string
        /// all fall through.
        private func truthyString(_ v: JSON?) -> String? {
            guard let v, pyTruthy(v), case .string(let s) = v else { return nil }
            return s
        }

        /// Python `_shell`: run one command line and collect its output from the
        /// scrollback, delimited by a done-token the shell prints after it.
        private func shellVerb(agent: Agent, payload: JSON, kernel: Kernel) async throws -> JSON {
            guard let session = sessionFor(agent.id) else {
                return .object(["error": .string("shell: not running; call boot first")])
            }
            // `cmd` is tested for TRUTH and then interpolated with `str()`; the
            // reply echoes the value as it arrived.
            let cmd = payload.asObject?["cmd"] ?? .string("")
            if !pyTruthy(cmd) { return .object(["error": .string("shell: cmd required")]) }
            let timeout = try pyFloat(payload.asObject?["timeout"] ?? .double(30.0))
            // `shell` drains via scrollback, not the emit stream — it can't be left
            // at the mercy of a streamer's stale flow-control pause (a detached
            // reader means the done-token never lands → timeout). Clear on the
            // way in, and again on the way out so this call's own output doesn't
            // strand the reader paused for the next streamer.
            session.clearFlowControl(kernel: kernel)
            let token = "__DONE_\(hexToken(8))__"
            let waiter = session.addToken(token)
            defer {
                session.removeToken(token)
                session.clearFlowControl(kernel: kernel)
            }
            let before = Array(session.scrollbackText().unicodeScalars)
            let line = "\(pyStr(cmd)); printf '\\n%s\\n' '\(token)'\n"
            if case .failed(let code) = await session.writeAll(Data(line.utf8)) {
                return .object(["error": .string(osErrorStr(code))])
            }
            let completed = await waiter.wait(seconds: timeout)
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
            let tokIdx =
                find(Array((token + "\r\n").unicodeScalars), in: delta)
                ?? find(Array((token + "\n").unicodeScalars), in: delta)
            if let tokIdx {
                let lineStart = delta[..<tokIdx].lastIndex(of: "\n")
                output = Array(delta[..<(lineStart ?? tokIdx)])
                while let last = output.last, last == "\r" || last == "\n" { output.removeLast() }
            }
            let text = String(String.UnicodeScalarView(output))
            if !completed {
                return .object([
                    "cmd": cmd, "output": .string(text),
                    "completed": .bool(false), "error": .string("timeout"),
                ])
            }
            return .object(["cmd": cmd, "output": .string(text), "completed": .bool(true)])
        }

        /// Binary-channel `paste_image` (python `_paste_image`): save the blob to
        /// a per-session scratch file and type its absolute path + one trailing
        /// space into the PTY — no newline, so pasting an image never submits.
        private func pasteImage(agentId: AgentId, header: JSON, blob: Data) async throws -> JSON {
            guard let session = sessionFor(agentId) else { return notRunning }
            if blob.count > MAX_PASTE_IMAGE {
                return .object([
                    "error": .string("paste_image: \(blob.count) bytes exceeds the 5 MB cap")
                ])
            }
            // `(payload.get("mime") or "image/png").lower()` — an empty mime is
            // png, not an unsupported type.
            let raw = header.asObject?["mime"] ?? .null
            let mime = (pyTruthy(raw) ? pyStr(raw) : "image/png").lowercased()
            guard let ext = IMAGE_EXT[mime] else {
                return .object([
                    "error": .string("paste_image: unsupported image type \(pyRepr(mime))")
                ])
            }
            // `mkdtemp` failure RAISES in python — it is not part of the reply.
            let dir = try session.pasteDirectory(agentId: agentId)
            let name = "paste_\(hexToken(4)).\(ext)"
            do {
                // The scratch dir is a system temp dir (outside cwd by design) —
                // written through `fs`'s EXTERNAL surface, the one disk module.
                _ = try fsWriteChunk(dir, name, blob, offset: 0, truncate: true, base: "/")
            } catch {
                return .object(["error": .string(error.message)])
            }
            let path = dir + "/" + name
            if case .failed(let code) = await session.writeAll(Data((path + " ").utf8)) {
                return .object(["error": .string(osErrorStr(code))])
            }
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

        /// `cleanup` from a session's OWN reader — a no-op once that session is
        /// no longer the registered one.
        private func cleanup(agentId: AgentId, ifCurrent session: PtySession) {
            lock.lock()
            let current = sessions[agentId] === session
            if current { sessions.removeValue(forKey: agentId) }
            lock.unlock()
            if current { session.terminate() }
        }

        private func sessionFor(_ id: AgentId) -> PtySession? {
            lock.lock()
            defer { lock.unlock() }
            return sessions[id]
        }

        /// Python `readme.md`, byte for byte.
        static let readmeText = """
            # terminal_backend — PTY shell
            Verb: shell (`cmd`) runs a command in a persistent PTY, streams token/done. Per-agent interrupt/stop. One persistent PTY **session per agent** — a session is exclusive (it carries one process + its scrollback; it is not shared or multiplexed), so a client owns the backend it attaches to. The process running in the PTY (e.g. `claude`) reaches the kernel via `fantastic` CLI one-shots or the web surface.

            Streaming output is flow-controlled (VSCode's terminal model, ported): the PTY reader detaches once >100K emitted chars sit unacked and re-attaches once a consumer's `ack` verb drains the backlog — real backpressure so a flood can't lock up a tab. `write` is looped + per-agent serialized so large pastes land whole and bracketed-paste sequences can't interleave. `shell` is exempt (drains via scrollback).

            `paste_image` (data:bytes, mime?) saves a pasted image to a per-agent scratch file and types its path into the PTY — bridges image paste for a CLI like `claude` running under the PTY (the backend can't reach a client's clipboard; path injection mimics a file drag-drop). png/jpeg/gif/webp, ≤5 MB.

            PTY output is decoded with a per-session incremental UTF-8 decoder, so a multi-byte char split across an `os.read` chunk boundary is reassembled — no replacement-char (`<?>`) litter or column-shift line breaks on resize (node-pty's behaviour, ported).

            """
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

    /// Python `text[-n:]` over CODE POINTS, guarded by `if n < len(utf8)`:
    /// `n == 0` keeps the whole text (`text[0:]`), a negative `n` DROPS that many
    /// leading code points, and neither traps.
    func pySliceTail(_ text: String, _ maxBytes: Int) -> String {
        guard maxBytes < text.utf8.count else { return text }
        let scalars = text.unicodeScalars
        if maxBytes > 0 {
            return String(String.UnicodeScalarView(scalars.suffix(maxBytes)))
        }
        return String(String.UnicodeScalarView(scalars.dropFirst(-maxBytes)))
    }

    /// Python `secrets.token_hex(n)`: 2n hex chars.
    private func hexToken(_ n: Int) -> String {
        (0..<n).map { _ in String(format: "%02x", UInt8.random(in: .min ... .max)) }.joined()
    }

    /// Python `_detect_shell`: `$SHELL` if it is a file, else the first of
    /// zsh/bash/sh, else `/bin/sh`.
    func detectShell() -> String {
        let fm = FileManager.default
        if let sh = ProcessInfo.processInfo.environment["SHELL"], !sh.isEmpty,
            fm.fileExists(atPath: sh)
        {
            return sh
        }
        for cand in ["/bin/zsh", "/bin/bash", "/bin/sh"] where fm.fileExists(atPath: cand) {
            return cand
        }
        return "/bin/sh"
    }

    /// First index of `needle` in `hay` (python `str.find`).
    private func find(_ needle: [Unicode.Scalar], in hay: [Unicode.Scalar]) -> Int? {
        guard !needle.isEmpty, hay.count >= needle.count else { return nil }
        for i in 0...(hay.count - needle.count) where hay[i] == needle[0] {
            if Array(hay[i..<(i + needle.count)]) == needle { return i }
        }
        return nil
    }

    /// `struct.pack("HHHH", rows, cols, 0, 0)` — CPython's `np_ushort` range
    /// check, which is what a resize to 70000 columns actually hits. `struct`'s
    /// exception class is named `error`, so python stamps `error: …`.
    func packWinsize(cols: Int, rows: Int) throws(PyError) -> winsize {
        for n in [rows, cols] {
            if n < 0 { throw PyError("error", "argument out of range") }
            if n > 65535 {
                throw PyError("error", "ushort format requires 0 <= number <= 65535")
            }
        }
        return winsize(ws_row: UInt16(rows), ws_col: UInt16(cols), ws_xpixel: 0, ws_ypixel: 0)
    }

    /// The outcome of a full PTY write: python's `{written:int}` or the `OSError`
    /// it lets `_write` / `_paste_image` / `_shell` turn into `{error}`.
    enum WriteOutcome {
        case ok(Int)
        case failed(Int32)
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

    /// The body of python's `_hold()`: a task that lives until it is cancelled,
    /// so the reader attachment is WORK the kernel can see in `ps` and kill.
    final class ReaderHolder: @unchecked Sendable {
        private let lock = NSLock()
        private var released = false
        private var continuation: CheckedContinuation<Void, Never>?

        func park() async {
            await withTaskCancellationHandler {
                await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
                    lock.lock()
                    if released {
                        lock.unlock()
                        return c.resume()
                    }
                    continuation = c
                    lock.unlock()
                }
            } onCancel: {
                release()
            }
        }

        func release() {
            lock.lock()
            released = true
            let c = continuation
            continuation = nil
            lock.unlock()
            c?.resume()
        }
    }

    // MARK: - PtySession

    /// Guards `forkpty` — see `PtySession.start`. Process-wide: every PTY in
    /// this process shares Darwin's one `ptsname` buffer.
    private let forkLock = NSLock()

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
        /// ONE ordered consumer for the inbox events: python hands each emit to
        /// the loop in read order, so `output` chunks — and the final `closed` —
        /// arrive in the order the PTY produced them. A `Task` per chunk does
        /// not: independent tasks interleave, and a burst arrives shuffled.
        private let events: AsyncStream<JSON>.Continuation
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
            let (stream, continuation) = AsyncStream<JSON>.makeStream(
                bufferingPolicy: .unbounded)
            self.events = continuation
            Task { [agentId, weak kernel] in
                for await event in stream {
                    guard let kernel else { return }
                    await kernel.emit(agentId, event)
                }
            }
        }

        /// The reader starts only once the bundle has stored the session AND
        /// wired `onExit` — python stores `_procs[id]` and only then calls
        /// `_attach_reader`. Started inside `init`, a child that died in that
        /// window hit a nil `onExit` and left a dead session reading `running`.
        func beginReading() {
            readQueue.async { [weak self] in self?.readLoop() }
        }

        /// Python `_spawn`'s fork: `pty.fork()` and then `_execvpe` of the
        /// command in `cwd` with `TERM=xterm-256color`. Everything the child
        /// needs is built BEFORE the fork — only async-signal-safe calls after.
        static func start(
            cmd: String, args: [String], cwd: String, cols: Int, rows: Int,
            agentId: AgentId, kernel: Kernel
        ) throws(PyError) -> PtySession {
            // `_execvpe`: a command with a `/` is execed as written and its
            // failure names the STRING; a bare one walks PATH and the exception
            // names the failing candidate as BYTES.
            let candidates: [(path: String, display: String)] =
                cmd.contains("/")
                ? [(cmd, pyRepr(cmd))]
                : execPath().map { ("\($0)/\(cmd)", "b" + pyRepr("\($0)/\(cmd)")) }
            var env = ProcessInfo.processInfo.environment
            env["TERM"] = "xterm-256color"
            let argv: [UnsafeMutablePointer<CChar>?] = ([cmd] + args).map { strdup($0) } + [nil]
            let envp: [UnsafeMutablePointer<CChar>?] =
                env.map { strdup("\($0.key)=\($0.value)") } + [nil]
            let cwdC = strdup(cwd)
            let pathC = candidates.map { strdup($0.path) }
            // RAW C VECTORS, BUILT BEFORE THE FORK — the child touches no Swift
            // array. `execve(p, &a, &e)` is an INOUT access on a copy-on-write
            // buffer the parent still references, so the copy Swift makes for it
            // happens IN THE CHILD: a malloc in a fork of a threaded process,
            // which may have inherited a held malloc lock. The child then never
            // reaches `execve`, its master sees instant EOF, and the session
            // comes back `running:false, pid:null` — measured 2026-09-22 at ~1
            // spawn in 9 (`bootsShellWriteEmitsOutputAndScrollback`, "reflect
            // carries no pid"). `var a = argv` did NOT copy; it only shared.
            let argvC = UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>.allocate(
                capacity: argv.count)
            argv.withUnsafeBufferPointer { argvC.update(from: $0.baseAddress!, count: $0.count) }
            let envpC = UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>.allocate(
                capacity: envp.count)
            envp.withUnsafeBufferPointer { envpC.update(from: $0.baseAddress!, count: $0.count) }
            let pathVec = UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>.allocate(
                capacity: max(1, pathC.count))
            pathC.withUnsafeBufferPointer { pathVec.update(from: $0.baseAddress!, count: $0.count) }
            let pathCount = pathC.count
            defer {
                argv.forEach { free($0) }
                envp.forEach { free($0) }
                free(cwdC)
                pathC.forEach { free($0) }
                argvC.deallocate()
                envpC.deallocate()
                pathVec.deallocate()
            }
            var master: Int32 = 0
            // SERIALIZED. Darwin's `forkpty` calls `openpty`, which names the
            // slave with `ptsname()` — a STATIC buffer. Two threads spawning at
            // once interleave there and one child opens the OTHER pair's slave:
            // that master sees instant EOF (session reports `running:false`,
            // `pid:null`) while the other master never sees EOF and carries two
            // children's output. Python forks from one thread, so nothing here
            // may fork from two.
            forkLock.lock()
            let pid = forkpty(&master, nil, nil, nil)
            if pid > 0 { forkLock.unlock() }
            if pid < 0 {
                let e = errno
                forkLock.unlock()
                throw pyOSError(e)
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
                // `_execvpe` skips ENOENT/ENOTDIR candidates, raises the FIRST
                // other failure if there was one, else the LAST.
                var savedErrno: Int32 = 0
                var savedIdx = 0
                var lastErrno: Int32 = ENOENT
                var lastIdx = 0
                // An INDEX loop over the raw vector: `pathC.enumerated()` is a
                // Swift sequence, and iterating one after the fork is the same
                // hazard the vectors above exist to remove.
                for i in 0..<pathCount {
                    execve(pathVec[i], argvC, envpC)
                    lastErrno = errno
                    lastIdx = i
                    if errno != ENOENT && errno != ENOTDIR && savedErrno == 0 {
                        savedErrno = errno
                        savedIdx = i
                    }
                }
                let code = savedErrno != 0 ? savedErrno : lastErrno
                let idx = savedErrno != 0 ? savedIdx : lastIdx
                let blame = idx < candidates.count ? candidates[idx].display : pyRepr(cmd)
                let msg = "\(cmd): \(osErrorStr(code)): \(blame)\r\n"
                _ = msg.withCString { Darwin.write(2, $0, strlen($0)) }
                _exit(127)
            }
            // Python sets the winsize AFTER the fork: an out-of-ushort size
            // raises with the child already alive.
            var ws = try packWinsize(cols: cols, rows: rows)
            _ = ioctl(master, TIOCSWINSZ, &ws)
            return PtySession(
                pid: pid, masterFd: master, cmd: cmd, cols: cols, rows: rows, agentId: agentId,
                kernel: kernel)
        }

        /// `os.get_exec_path(env)` — `PATH`, or `os.defpath` when it is unset.
        private static func execPath() -> [String] {
            let path = ProcessInfo.processInfo.environment["PATH"] ?? ":/bin:/usr/bin"
            return path.components(separatedBy: ":")
        }

        // ── the reader (python `_on_readable`) ──

        /// Python `_attach_reader`: the fd callback is the flow-control design,
        /// but the ATTACHMENT is registered as this agent's work — a PTY
        /// streaming into a kernel that appears in no `ps` row cannot be found
        /// or cancelled. One holder per (re)attach, python's lifecycle included.
        func attachReader(kernel: Kernel) {
            // ONE holder per attachment: the previous one ends with its detach
            // (python cancels the `_hold` task in `_detach_reader`, 2026-09-20).
            // UNDER `state`: `ack` and `clear_flow_control` re-attach from a
            // kernel dispatch thread while `terminate` drops the holder from
            // another, and an unsynchronized store to a strong class reference
            // is a use-after-free, not a lost wakeup. A session that has already
            // terminated takes no new holder — that task would park forever.
            state.lock()
            let previous = readerHolder
            let alreadyStopped = stopped
            let holder = ReaderHolder()
            readerHolder = alreadyStopped ? nil : holder
            state.unlock()
            previous?.release()
            if alreadyStopped { return }
            kernel.spawnTask(agent: agentId, verb: "pty_reader") { await holder.park() }
        }

        /// The reader attachment as kernel work; released on detach.
        private var readerHolder: ReaderHolder?

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
                var n = read(fd, &buffer, buffer.count)
                // AN EOF IS ONLY TERMINAL WHEN THE CHILD IS GONE. Measured
                // 2026-09-21 (~2 runs in 15 of `--filter FantasticBundlesTests`):
                // right after `forkpty` the FIRST `read()` on the correct master
                // returns 0 while the child is still ALIVE — on Darwin a PTY
                // master reports EOF in the window before the child has opened
                // its slave side. Cleanup then dropped a session that was about
                // to speak (`running:false, pid:null`, empty scrollback). Python
                // never sees it because its event loop reaches the fd later.
                // `waitpid(WNOHANG)` is the authority: 0 means the child has not
                // exited, so the read is retried. Bounded so a live child whose
                // slave really is gone still terminates, and the 1 ms backoff is
                // the smallest pause that keeps the retry off a busy spin.
                if n == 0 {
                    var attempts = 0
                    while attempts < 1000 {
                        var status: Int32 = 0
                        if waitpid(pid, &status, WNOHANG) != 0 { break }
                        usleep(1000)
                        attempts += 1
                        state.lock()
                        let gone = stopped
                        state.unlock()
                        if gone { return }
                        n = read(fd, &buffer, buffer.count)
                        if n != 0 { break }
                    }
                }
                if n <= 0 {
                    // EOF — the child exited. Python: detach, emit closed, cleanup.
                    state.lock()
                    let wasStopped = stopped
                    state.unlock()
                    if !wasStopped {
                        events.yield(.object(["type": .string("closed")]))
                        onExit?()
                    }
                    return
                }
                // EVERY non-empty read emits, even when the bytes are the first
                // half of one char and decode to `''` — python's `_on_readable`
                // has no empty-text guard, and a consumer counting events sees
                // the same count here.
                let text = decode(Array(buffer.prefix(n)))
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
                events.yield(.object(["type": .string("output"), "data": .string(text)]))
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
                if need > held + 1 {
                    pendingBytes = Array(bytes[i...])
                    bytes = Array(bytes[..<i])
                }
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
        func ack(chars: Int, kernel: Kernel) -> (unacked: Int, paused: Bool) {
            state.lock()
            unacked = max(0, unacked - chars)
            var resumed = false
            if paused && unacked < LOW_WATERMARK {
                paused = false
                resumed = true
                state.broadcast()
            }
            let snapshot = (unacked, paused)
            state.unlock()
            if resumed { attachReader(kernel: kernel) }
            return snapshot
        }

        /// Python `_clear_flow_control`: reset the accounting, resume the reader.
        func clearFlowControl(kernel: Kernel) {
            state.lock()
            unacked = 0
            var resumed = false
            if paused {
                paused = false
                resumed = true
                state.broadcast()
            }
            state.unlock()
            if resumed { attachReader(kernel: kernel) }
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
        /// retried from where it stopped, a full PTY input queue is waited out,
        /// and any OTHER error is the `OSError` python reports as `{error}`.
        func writeAll(_ data: Data) async -> WriteOutcome {
            await withCheckedContinuation { (c: CheckedContinuation<WriteOutcome, Never>) in
                writeQueue.async { [self] in
                    let fd = fdForWrite()
                    guard fd >= 0 else { return c.resume(returning: .failed(EBADF)) }
                    var written = 0
                    var failure: Int32 = 0
                    data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
                        guard let base = raw.baseAddress else { return }
                        while written < data.count {
                            let n = Darwin.write(fd, base + written, data.count - written)
                            if n > 0 {
                                written += n
                            } else if n < 0 && errno == EAGAIN {
                                usleep(1000)
                            } else {
                                failure = n < 0 ? errno : EIO
                                return
                            }
                        }
                    }
                    c.resume(returning: failure == 0 ? .ok(written) : .failed(failure))
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

        /// Python `_resize`: the new size is STORED first, then packed — a pack
        /// that raises leaves the stored size changed and the ioctl unmade.
        func resize(cols: Int, rows: Int) throws(PyError) {
            state.lock()
            self.cols = cols
            self.rows = rows
            let fd = masterFd
            state.unlock()
            var ws = try packWinsize(cols: cols, rows: rows)
            _ = ioctl(fd, TIOCSWINSZ, &ws)
        }

        /// Scratch dir for pasted images, created on first use — most sessions
        /// never paste, so no temp dir is minted per spawn (python `mkdtemp`).
        func pasteDirectory(agentId: AgentId) throws(PyError) -> String {
            state.lock()
            defer { state.unlock() }
            if let dir = pasteDir { return dir }
            let dir = (NSTemporaryDirectory() as NSString).appendingPathComponent(
                "fantastic_paste_\(agentId.value)_\(hexToken(4))")
            if mkdir(dir, 0o700) != 0 { throw pyOSError(errno, dir) }
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
            let holder = readerHolder
            readerHolder = nil
            let fd = masterFd
            masterFd = -1
            let dir = pasteDir
            pasteDir = nil
            pendingTokens = [:]
            state.broadcast()
            state.unlock()
            holder?.release()
            kill(pid, SIGKILL)
            var status: Int32 = 0
            waitpid(pid, &status, 0)
            if fd >= 0 { close(fd) }
            // The scratch dir goes with the session (python `fs.remove`,
            // external surface, ignoring an already-gone dir).
            if let dir { try? fsRemove(dir, "", recursive: true, base: "/") }
            // A `shell` waiting on a token of a dead session times out on its own;
            // nothing to fire. The ordered consumer drains what is buffered.
            events.finish()
        }

        deinit {
            terminate()
        }
    }

#endif  // os(macOS)
