#if os(macOS)
    import FantasticJSON
    import Foundation

    /// Spawning a COMMAND with a control socket it can talk back on.
    ///
    /// A spawned program MAY speak the kernel over the inherited control fd; one that
    /// never reads it runs exactly like any OS process. This is the PARENT half of that
    /// wire: it makes the socket pair, hands one end to the child, and returns the other
    /// for the relay. Mirror of python `proc.spawn_argv`: spawn argv AS GIVEN — the gate
    /// is the caller's regex allowlist on the shell-joined line, and this adds nothing.
    ///
    /// ## `posix_spawnp`, not Foundation's `Process` — and no shell shim
    ///
    /// `Process` spawns with `POSIX_SPAWN_CLOEXEC_DEFAULT`, so every descriptor it does
    /// not name in `file_actions` is closed regardless of its FD_CLOEXEC bit — and it
    /// only ever names 0/1/2. Carrying the control socket through it needed a `sh -c`
    /// shim that moved stdin to fd 3 and re-exec'd, which meant the child's **stdin was
    /// /dev/null** where python leaves it INHERITED, and an `sh` process image existed
    /// between the spawn and the command.
    ///
    /// `posix_spawnp` takes the file actions directly: the control socket is dup'd to
    /// fd 3, the two pipes to 1/2, stdin is inherited by name, and every other
    /// descriptor is closed — python's `create_subprocess_exec(..., pass_fds=(fd,))`
    /// behaviour (`close_fds=True`), argv untouched.
    enum Spawn {

        /// A spawn that never reached `exec`. `description` is python's `str(OSError)`
        /// for the same failure, so the refusal text matches byte for byte.
        struct Failure: Error, CustomStringConvertible {
            let code: Int32
            let path: String
            var description: String { osErrorStr(code, path) }
        }

        struct Spawned {
            /// The live child. Owns the reap and THE definition of liveness.
            let child: Child
            /// Parent end of the control socket. The relay owns it and must close it.
            let ctrlFd: Int32
            /// Read ends of the child's stdout/stderr pipes; the pump owns them.
            let stdout: FileHandle
            let stderr: FileHandle
        }

        /// Launch `argv` (argv[0] = command, rest = args) with a control socket on an
        /// inherited fd. `selfId` is the OWNER agent id (the tool's public id, not an
        /// ephemeral job id), so a tool can address itself by the id others use.
        /// `envExtra` is the per-call `env` block, applied BEFORE the two control vars
        /// so a caller cannot override them. There is NO `FANTASTIC_ARGS`: a
        /// general-purpose command runner passes arguments as argv, not a smuggled env.
        static func job(
            argv: [String], selfId: String, envExtra: [String: String], cwd: String?
        ) throws -> Spawned {
            guard let command = argv.first else {
                throw Failure(code: EINVAL, path: "")
            }
            var sock: [Int32] = [0, 0]
            guard socketpair(AF_UNIX, SOCK_STREAM, 0, &sock) == 0 else {
                throw Failure(code: errno, path: command)
            }
            let parentFd = sock[0], childFd = sock[1]
            var outFds: [Int32] = [0, 0]
            var errFds: [Int32] = [0, 0]
            guard pipe(&outFds) == 0 else {
                close(parentFd)
                close(childFd)
                throw Failure(code: errno, path: command)
            }
            guard pipe(&errFds) == 0 else {
                close(parentFd)
                close(childFd)
                close(outFds[0])
                close(outFds[1])
                throw Failure(code: errno, path: command)
            }
            // The parent's keepers never cross the exec; the child's three are dup'd
            // onto 3/1/2 (which clears CLOEXEC on the target) and their originals are
            // closed by an explicit file action when they are not already in place.
            for fd in [parentFd, outFds[0], errFds[0]] { _ = fcntl(fd, F_SETFD, FD_CLOEXEC) }

            var actions: posix_spawn_file_actions_t?
            posix_spawn_file_actions_init(&actions)
            defer { posix_spawn_file_actions_destroy(&actions) }
            for (from, to) in [(childFd, Int32(3)), (outFds[1], Int32(1)), (errFds[1], Int32(2))]
            where from != to {
                posix_spawn_file_actions_adddup2(&actions, from, to)
                posix_spawn_file_actions_addclose(&actions, from)
            }
            // `POSIX_SPAWN_CLOEXEC_DEFAULT` (below) closes everything a file action
            // does not name. stdin is named so it stays INHERITED, as python leaves it.
            posix_spawn_file_actions_addinherit_np(&actions, 0)
            if let cwd, !cwd.isEmpty {
                // `_np`, not the 26.0-only spelling: this package deploys to macOS 14.
                posix_spawn_file_actions_addchdir_np(&actions, cwd)
            }

            // `restore_signals=True`, python's default: every signal back to SIG_DFL
            // with an empty mask, so a child does not inherit a handler or a block the
            // kernel installed for itself.
            var attrs: posix_spawnattr_t?
            posix_spawnattr_init(&attrs)
            defer { posix_spawnattr_destroy(&attrs) }
            var allSignals = sigset_t()
            sigfillset(&allSignals)
            var noSignals = sigset_t()
            sigemptyset(&noSignals)
            posix_spawnattr_setsigdefault(&attrs, &allSignals)
            posix_spawnattr_setsigmask(&attrs, &noSignals)
            // `close_fds=True`, python's default with `pass_fds`: the child gets 0/1/2/3
            // and NOTHING else. Without it a child inherited every descriptor of the
            // kernel that was not yet FD_CLOEXEC — including ANOTHER job's parent socket
            // in the window between its `socketpair()` and its `fcntl`, which kept that
            // job's lifeline open after the kernel closed it: the child never saw EOF
            // and never self-reaped (an orphan, measured 2026-09-22).
            posix_spawnattr_setflags(
                &attrs,
                Int16(POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_CLOEXEC_DEFAULT))

            var env = ProcessInfo.processInfo.environment
            for (k, v) in envExtra { env[k] = v }
            // AFTER `envExtra`: a caller's `env` cannot rewrite the control wire.
            env["FANTASTIC_CTRL_FD"] = "3"
            env["FANTASTIC_SELF_ID"] = selfId

            var pid: pid_t = 0
            let rc = withCStrings(argv) { cArgv in
                withCStrings(env.map { "\($0.key)=\($0.value)" }) { cEnv in
                    posix_spawnp(&pid, command, &actions, &attrs, cArgv, cEnv)
                }
            }
            // The child's ends are ours to drop on every path — keep the write ends and
            // the pipes never reach EOF; keep the socket's child end and the connector's
            // reader never returns, so the program never learns the kernel died.
            close(childFd)
            close(outFds[1])
            close(errFds[1])
            guard rc == 0 else {
                close(parentFd)
                close(outFds[0])
                close(errFds[0])
                throw Failure(code: rc, path: command)
            }
            return Spawned(
                child: Child(pid: pid), ctrlFd: parentFd,
                stdout: FileHandle(fileDescriptor: outFds[0], closeOnDealloc: true),
                stderr: FileHandle(fileDescriptor: errFds[0], closeOnDealloc: true))
        }

        /// Hand a `[String]` to C as a NULL-terminated `char *const *`.
        private static func withCStrings<R>(
            _ strings: [String], _ body: (UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> R
        ) -> R {
            var pointers: [UnsafeMutablePointer<CChar>?] = strings.map { strdup($0) }
            pointers.append(nil)
            defer { for p in pointers where p != nil { free(p) } }
            return body(&pointers)
        }
    }

    /// One live (or exited) child: its pid, THE definition of liveness, and the reap.
    ///
    /// Python's `_alive` is `process.returncode is None` — the OS PROCESS has not
    /// exited, never "the pipes reached EOF". `exitCode` is python's `returncode`:
    /// the exit status, or `-signal` when a signal killed it.
    final class Child: @unchecked Sendable {
        let pid: pid_t
        private let lock = NSLock()
        private var code: Int32?
        private var waiters: [CheckedContinuation<Int32, Never>] = []

        init(pid: pid_t) {
            self.pid = pid
            // One detached reaper per child: `waitpid` on OUR pid only, so nothing
            // else's child is claimed.
            Thread.detachNewThread { [self] in
                var status: Int32 = 0
                while waitpid(pid, &status, 0) < 0 && errno == EINTR {}
                finish(Self.decode(status))
            }
        }

        /// `wait(2)` status → python's `returncode`: `-signal` for a signal death
        /// (`ok:false` and a NEGATIVE `exit_code`), else the exit status.
        static func decode(_ status: Int32) -> Int32 {
            let term = status & 0o177
            if term != 0 && term != 0o177 { return -term }
            return (status >> 8) & 0xFF
        }

        /// python `returncode`: nil until the process has exited.
        var exitCode: Int32? { lock.withLock { code } }

        /// THE definition of a live job, one place: its OS process has not exited.
        var isAlive: Bool { lock.withLock { code == nil } }

        func signal(_ sig: Int32) -> Bool { kill(pid, sig) == 0 }

        func wait() async -> Int32 {
            await withCheckedContinuation { (c: CheckedContinuation<Int32, Never>) in
                lock.lock()
                if let code {
                    lock.unlock()
                    c.resume(returning: code)
                } else {
                    waiters.append(c)
                    lock.unlock()
                }
            }
        }

        private func finish(_ status: Int32) {
            lock.lock()
            guard code == nil else {
                lock.unlock()
                return
            }
            code = status
            let pending = waiters
            waiters.removeAll()
            lock.unlock()
            for c in pending { c.resume(returning: status) }
        }
    }
#endif
