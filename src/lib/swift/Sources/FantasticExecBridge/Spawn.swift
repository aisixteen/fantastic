#if os(macOS)
    import Foundation

    /// Spawning a COMMAND with a control socket it can talk back on.
    ///
    /// A spawned program MAY speak the kernel over the inherited control fd; one that
    /// never reads it runs exactly like any OS process. This is the PARENT half of that
    /// wire: it makes the socket pair, hands one end to the child, and returns the other
    /// for the relay. Mirror of python `proc.spawn_argv`: spawn argv AS GIVEN — the gate
    /// is the caller's regex allowlist on the shell-joined line, and this adds nothing.
    ///
    /// ## Foundation's `Process` cannot pass an arbitrary fd — measured
    ///
    /// It spawns with `POSIX_SPAWN_CLOEXEC_DEFAULT`, so every descriptor not named in its
    /// `file_actions` is closed regardless of its actual `FD_CLOEXEC` bit — and it only
    /// ever names 0/1/2. So the socket goes in as **stdin**, which IS in `file_actions`,
    /// and a tiny `sh` shim moves it to fd 3 and re-execs the command:
    ///
    ///     sh -c 'exec 3<&0 0</dev/null; exec "$@"' fx <command> <args...>
    ///
    /// `exec "$@"` replaces the process image, so the **pid is preserved** — signals,
    /// `kill(pid,0)` and the orphan gate all keep working — and Foundation still does the
    /// `waitpid`. The command + argv are passed as `"$@"`, so no shell quoting ever
    /// touches them.
    enum Spawn {

        struct Spawned {
            let process: Process
            /// Parent end of the control socket. The relay owns it and must close it.
            let ctrlFd: Int32
            let stdout: Pipe
            let stderr: Pipe
        }

        /// Launch `argv` (argv[0] = command, rest = args) with a control socket on an
        /// inherited fd. `selfId` is the OWNER agent id (the tool's public id, not an
        /// ephemeral job id), so a tool can address itself by the id others use.
        /// `envExtra` is the per-call `env` block. There is NO `FANTASTIC_ARGS`: a
        /// general-purpose command runner passes arguments as argv, not a smuggled env.
        static func job(
            argv: [String], selfId: String, envExtra: [String: String], cwd: String?
        ) throws -> Spawned {
            guard !argv.isEmpty else {
                throw NSError(
                    domain: NSPOSIXErrorDomain, code: Int(EINVAL),
                    userInfo: [NSLocalizedDescriptionKey: "empty argv"])
            }
            var fds: [Int32] = [0, 0]
            guard socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0 else {
                throw NSError(
                    domain: NSPOSIXErrorDomain, code: Int(errno),
                    userInfo: [NSLocalizedDescriptionKey: "socketpair failed"])
            }
            let parentFd = fds[0], childFd = fds[1]
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/bin/sh")
            // $0=fx (a label), $@ = argv; `exec "$@"` runs command with its args.
            p.arguments = ["-c", "exec 3<&0 0</dev/null; exec \"$@\"", "fx"] + argv
            // stdin IS in Process's file_actions, so this one survives. The shim moves it
            // to 3 and re-execs.
            p.standardInput = FileHandle(fileDescriptor: childFd, closeOnDealloc: false)
            var env = ProcessInfo.processInfo.environment
            for (k, v) in envExtra { env[k] = v }
            env["FANTASTIC_CTRL_FD"] = "3"  // where the shim puts it
            env["FANTASTIC_SELF_ID"] = selfId
            p.environment = env
            if let cwd, !cwd.isEmpty { p.currentDirectoryURL = URL(fileURLWithPath: cwd) }
            let out = Pipe(), err = Pipe()
            p.standardOutput = out
            p.standardError = err

            do {
                try p.run()
            } catch {
                close(parentFd)
                close(childFd)
                throw error
            }
            // CLOSE THE PARENT'S COPY OF THE CHILD END, on every path. Keep it and the
            // socket never reaches EOF when the relay closes, the connector's reader
            // never returns, and the child never self-reaps.
            close(childFd)
            return Spawned(process: p, ctrlFd: parentFd, stdout: out, stderr: err)
        }
    }
#endif
