#if os(macOS)
    import Foundation

    /// Spawning a durable python child with a control socket it can talk back on.
    ///
    /// The caller applies the OPTIONAL shim (the connector) before handing `code`
    /// here — the wire is the contract; this is the PARENT half's launcher.
    ///
    /// ## Foundation's `Process` cannot pass an arbitrary fd — measured
    ///
    /// It spawns with `POSIX_SPAWN_CLOEXEC_DEFAULT`, so every descriptor not named in its
    /// `file_actions` is closed regardless of its actual `FD_CLOEXEC` bit — and it only
    /// ever names 0/1/2. Measured 2026-08-05: with stdout+stderr piped the child reports
    /// `HAVE ['0','1','2','3']` no matter what number we hand it, and dies
    /// `OSError: [Errno 9] Bad file descriptor`. (A one-pipe probe appears to work and is
    /// a false green — do not re-derive this from a simpler test.)
    ///
    /// So the socket goes in as **stdin**, which IS in `file_actions`, and a tiny `sh`
    /// shim moves it to fd 3 and re-execs python:
    ///
    ///     sh -c 'exec 3<&0 0</dev/null; exec "$0" -u -c "$1"' <interp> <code>
    ///
    /// Both `exec`s replace the process image, so the **pid is preserved** — signals,
    /// `kill(pid,0)` and the orphan gate all keep working — and Foundation still does the
    /// `waitpid`. The program is passed as `$1`, so no shell quoting ever touches it.
    enum Spawn {

        struct Spawned {
            let process: Process
            /// Parent end of the control socket. The relay owns it and must close it.
            let ctrlFd: Int32
            let stdout: Pipe
            let stderr: Pipe
        }

        /// Launch `interp -u -c <connector + code>` with a control socket on an
        /// inherited fd. `selfId` is the OWNER agent id (the tool's public id, not an
        /// ephemeral job id), so a tool can address itself by the id others use.
        static func job(
            interp: String, code: String, selfId: String, argsJSON: String, cwd: String?
        ) throws -> Spawned {
            var fds: [Int32] = [0, 0]
            guard socketpair(AF_UNIX, SOCK_STREAM, 0, &fds) == 0 else {
                throw NSError(
                    domain: NSPOSIXErrorDomain, code: Int(errno),
                    userInfo: [NSLocalizedDescriptionKey: "socketpair failed"])
            }
            let parentFd = fds[0], childFd = fds[1]
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/bin/sh")
            p.arguments = [
                "-c", "exec 3<&0 0</dev/null; exec \"$0\" -u -c \"$1\"",
                interp, code,
            ]
            // stdin IS in Process's file_actions, so this one survives. The shim moves it
            // to 3 and re-execs.
            p.standardInput = FileHandle(fileDescriptor: childFd, closeOnDealloc: false)
            var env = ProcessInfo.processInfo.environment
            env["FANTASTIC_CTRL_FD"] = "3"  // where the shim puts it
            env["FANTASTIC_SELF_ID"] = selfId
            env["FANTASTIC_ARGS"] = argsJSON
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
