// PTY jobs — a terminal is a JOB of exec_bridge (terminal_backend merged in).
//
// Mirror of python `exec_bridge.tools`'s PTY section. `start pty={cols,rows}`
// spawns through the same gate as every job; the job streams `output` events and
// is driven with `write` / `resize` / `ack` / `output` / `paste_image`.
// terminal_backend's robustness, ported onto the job table: incremental UTF-8
// decode (a multi-byte char split across reads is held, not shattered into `<?>`),
// writes in FULL and serialized per job (a bracketed paste is never truncated or
// interleaved), a bounded scrollback RING for re-attach, and VSCode's flow
// control (the reader pauses past HIGH_WATERMARK unacked chars; a streamer's
// `ack` resumes it under LOW_WATERMARK).
#if os(macOS)
    import Darwin
    import FantasticFile
    import FantasticJSON
    import FantasticKernel
    import Foundation

    let PTY_RING = 256 * 1024
    let PTY_HIGH_WATERMARK = 100_000
    let PTY_LOW_WATERMARK = 5_000
    let MAX_PASTE_IMAGE = 5 * 1024 * 1024

    /// An `errno` a PTY write failed with.
    struct PtyErrno: Error { let code: Int32 }
    let PTY_IMAGE_EXT: [(String, String)] = [
        ("image/png", "png"), ("image/jpeg", "jpg"), ("image/gif", "gif"), ("image/webp", "webp"),
    ]

    /// Guards `forkpty` — Darwin's `openpty` names the slave with `ptsname()`, a
    /// STATIC buffer: two threads spawning at once interleave there and one child
    /// opens the OTHER pair's slave. Process-wide, so nothing here forks from two.
    private let forkLock = NSLock()

    extension Spawn {
        /// A PTY job: the OS child on the SLAVE end of a fresh pseudo-terminal
        /// (stdin, stdout and stderr), leading a new session with that tty as its
        /// controlling terminal (`forkpty`). Same gate, same env rule, same control
        /// fd (on 3) as `job` — a terminal is a job, not a second spawner. Returns
        /// the child, the MASTER fd and the parent end of the control socket.
        static func pty(
            argv: [String], selfId: String, env: [String: String], cwd: String, cols: Int,
            rows: Int
        ) throws -> (Child, Int32, Int32) {
            guard let command = argv.first else { throw Failure(code: EINVAL, path: "") }
            let program = try resolve(command, cwd: cwd, env: env)
            var sock: [Int32] = [0, 0]
            guard socketpair(AF_UNIX, SOCK_STREAM, 0, &sock) == 0 else {
                throw Failure(code: errno, path: command)
            }
            let parentFd = sock[0], childFd = sock[1]
            _ = fcntl(parentFd, F_SETFD, FD_CLOEXEC)
            var full = env
            full["FANTASTIC_CTRL_FD"] = "3"
            full["FANTASTIC_SELF_ID"] = selfId
            full["TERM"] = "xterm-256color"
            // RAW C VECTORS, BUILT BEFORE THE FORK — the child touches no Swift
            // array (a copy-on-write in a fork of a threaded process may malloc
            // under an inherited lock and never reach `execve`).
            let argvList: [UnsafeMutablePointer<CChar>?] = argv.map { strdup($0) } + [nil]
            let envList: [UnsafeMutablePointer<CChar>?] =
                full.map { strdup("\($0.key)=\($0.value)") } + [nil]
            let programC = strdup(program)
            let cwdC = strdup(cwd)
            let argvC = UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>.allocate(
                capacity: argvList.count)
            argvList.withUnsafeBufferPointer {
                argvC.update(from: $0.baseAddress!, count: $0.count)
            }
            let envC = UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>.allocate(
                capacity: envList.count)
            envList.withUnsafeBufferPointer { envC.update(from: $0.baseAddress!, count: $0.count) }
            let failMsg = strdup("\(command): exec failed\r\n")
            defer {
                argvList.forEach { free($0) }
                envList.forEach { free($0) }
                free(programC)
                free(cwdC)
                free(failMsg)
                argvC.deallocate()
                envC.deallocate()
            }
            var ws = winsize(ws_row: UInt16(rows), ws_col: UInt16(cols), ws_xpixel: 0, ws_ypixel: 0)
            // `job`'s `restore_signals`, for the fork path: the child starts with an
            // EMPTY mask and every handler at SIG_DFL. A fork keeps the FORKING
            // THREAD's mask, and a pool thread that blocks SIGWINCH gave a shell
            // that never heard a resize.
            var noSignals = sigset_t()
            sigemptyset(&noSignals)
            var master: Int32 = 0
            forkLock.lock()
            let pid = forkpty(&master, nil, nil, &ws)
            if pid > 0 { forkLock.unlock() }
            if pid < 0 {
                let e = errno
                forkLock.unlock()
                close(parentFd)
                close(childFd)
                throw Failure(code: e, path: command)
            }
            if pid == 0 {
                // Child: stdin/out/err are the slave; the control socket goes to 3;
                // EVERY OTHER fd of the kernel process is closed (python's fds are
                // non-inheritable). Only async-signal-safe calls from here on.
                for sig in 1..<NSIG { _ = signal(sig, SIG_DFL) }
                _ = sigprocmask(SIG_SETMASK, &noSignals, nil)
                if childFd != 3 {
                    _ = dup2(childFd, 3)
                }
                let maxFd = Int32(min(getdtablesize(), 65536))
                if maxFd > 4 { for fd in 4..<maxFd { _ = close(fd) } }
                if chdir(cwdC) != 0 {
                    _ = Darwin.write(2, failMsg, strlen(failMsg!))
                    _exit(127)
                }
                execve(programC, argvC, envC)
                _ = Darwin.write(2, failMsg, strlen(failMsg!))
                _exit(127)
            }
            close(childFd)
            // The master stays with the kernel only, non-blocking for full writes.
            _ = fcntl(master, F_SETFD, FD_CLOEXEC)
            _ = fcntl(master, F_SETFL, fcntl(master, F_GETFL) | O_NONBLOCK)
            return (Child(pid: pid), master, parentFd)
        }
    }

    /// One PTY job's terminal — the master end and its mutable state.
    final class PtyJob: @unchecked Sendable {
        /// The job's id — its kernel task id, an int on the wire.
        let jobId: TaskId
        private let owner: AgentId
        private let child: Child
        private var masterFd: Int32
        private let readQueue = DispatchQueue(label: "exec.pty.read")
        /// Writes never interleave (a bracketed paste split by another write
        /// leaves the shell stuck in paste mode).
        private let writeQueue = DispatchQueue(label: "exec.pty.write")
        private let state = NSCondition()
        // ── under `state` ──
        private var ring: [String] = []
        private var ringBytes = 0
        private var unacked = 0
        private var paused = false
        private var eof = false
        private var pendingBytes: [UInt8] = []
        private var pasteDir: String?
        private(set) var cols: Int
        private(set) var rows: Int
        /// ONE ordered consumer: each `output` chunk reaches the inbox in read
        /// order, then `job_done` last.
        private let events: AsyncStream<JSON>.Continuation
        private let stream: AsyncStream<JSON>

        init(jobId: TaskId, owner: AgentId, child: Child, masterFd: Int32, cols: Int, rows: Int) {
            self.jobId = jobId
            self.owner = owner
            self.child = child
            self.masterFd = masterFd
            self.cols = cols
            self.rows = rows
            (stream, events) = AsyncStream<JSON>.makeStream(bufferingPolicy: .unbounded)
        }

        /// THE JOB'S TASK body (python `_pty_pump`): start the reader, forward every
        /// event in order, and when the terminal reaches EOF await the process and
        /// return its exit code — the caller writes the row and emits `job_done`.
        func run(kernel: Kernel) async -> Int32 {
            readQueue.async { [weak self] in self?.readLoop() }
            for await event in stream { await kernel.emit(owner, event) }
            return await child.wait()
        }

        private func readLoop() {
            var buffer = [UInt8](repeating: 0, count: 4096)
            while true {
                state.lock()
                while paused && !eof { state.wait() }
                let fd = masterFd
                let done = eof
                state.unlock()
                if done || fd < 0 { break }
                var pfd = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
                _ = poll(&pfd, 1, 200)
                let n = read(fd, &buffer, buffer.count)
                if n < 0 && (errno == EAGAIN || errno == EINTR) {
                    // Darwin reports a master with no slave opened yet as readable
                    // and empty; a child that is alive has not spoken yet.
                    continue
                }
                if n == 0 && child.isAlive {
                    usleep(1000)
                    continue
                }
                if n <= 0 { break }
                let text = decode(Array(buffer.prefix(n)))
                if text.isEmpty { continue }
                state.lock()
                ring.append(text)
                ringBytes += text.utf8.count
                while ringBytes > PTY_RING && !ring.isEmpty {
                    ringBytes -= ring.removeFirst().utf8.count
                }
                unacked += text.unicodeScalars.count
                if unacked > PTY_HIGH_WATERMARK { paused = true }
                state.unlock()
                events.yield(
                    .object([
                        "type": .string("output"), "job_id": .integer(jobId.value), "data": .string(text),
                    ]))
            }
            state.lock()
            eof = true
            let fd = masterFd
            masterFd = -1
            let dir = pasteDir
            pasteDir = nil
            state.unlock()
            if fd >= 0 { close(fd) }
            if let dir { try? fsRemove(dir, "", recursive: true, base: "/") }
            events.finish()
        }

        /// Incremental UTF-8: decode what is complete, keep a split tail for the
        /// next read, replace what is invalid (python's `errors="replace"`).
        private func decode(_ chunk: [UInt8]) -> String {
            var bytes = pendingBytes + chunk
            pendingBytes = []
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

        var isLive: Bool {
            state.lock()
            defer { state.unlock() }
            return !eof && masterFd >= 0
        }

        /// `ack` — flow control.
        func ack(_ chars: Int) -> (Int, Bool) {
            state.lock()
            unacked = max(0, unacked - chars)
            if paused && unacked < PTY_LOW_WATERMARK {
                paused = false
                state.broadcast()
            }
            let snap = (unacked, paused)
            state.unlock()
            return snap
        }

        /// `output` — the ring, for a client re-attaching.
        func ringText() -> String {
            state.lock()
            defer { state.unlock() }
            return ring.joined()
        }

        /// `resize` — the tty sends SIGWINCH to the program.
        func resize(cols: Int, rows: Int) -> Int32 {
            state.lock()
            self.cols = cols
            self.rows = rows
            let fd = masterFd
            state.unlock()
            var ws = winsize(ws_row: UInt16(rows), ws_col: UInt16(cols), ws_xpixel: 0, ws_ypixel: 0)
            return ioctl(fd, TIOCSWINSZ, &ws) == 0 ? 0 : errno
        }

        /// Write the FULL buffer, serialized per job; a full tty queue is waited out.
        func writeAll(_ data: Data) async -> Result<Int, PtyErrno> {
            await withCheckedContinuation { (c: CheckedContinuation<Result<Int, PtyErrno>, Never>) in
                writeQueue.async { [self] in
                    state.lock()
                    let fd = masterFd
                    state.unlock()
                    guard fd >= 0 else { return c.resume(returning: .failure(PtyErrno(code: EBADF))) }
                    var written = 0
                    var failure: Int32 = 0
                    data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
                        guard let base = raw.baseAddress else { return }
                        while written < data.count {
                            let n = Darwin.write(fd, base + written, data.count - written)
                            if n > 0 {
                                written += n
                            } else if n < 0 && (errno == EAGAIN || errno == EINTR) {
                                var pfd = pollfd(fd: fd, events: Int16(POLLOUT), revents: 0)
                                _ = poll(&pfd, 1, 50)
                            } else {
                                failure = errno
                                return
                            }
                        }
                    }
                    c.resume(
                        returning: failure == 0 ? .success(written) : .failure(PtyErrno(code: failure)))
                }
            }
        }

        /// A per-job scratch dir for pasted images, created on first use.
        func pasteDirectory() -> String? {
            state.lock()
            defer { state.unlock() }
            if let dir = pasteDir { return dir }
            let template = (NSTemporaryDirectory() as NSString).appendingPathComponent(
                "fantastic_paste_\(owner.value)_XXXXXXXX")
            var bytes = Array(template.utf8CString)
            guard let made = mkdtemp(&bytes) else { return nil }
            let dir = String(cString: made)
            pasteDir = dir
            return dir
        }
    }
#endif
