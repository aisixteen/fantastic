//! Unix implementation of the exec_bridge OS boundary.
//!
//! A job is a forked child that LEADS ITS OWN SESSION (`setsid`), so "kill the
//! job" is "SIGKILL every member of that session". Its control socket is one end
//! of a `socketpair`, kept across exec at the fd number `FANTASTIC_CTRL_FD`
//! advertises. A PTY job's terminal is an `openpty` pair whose slave becomes the
//! session's controlling terminal.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use nix::sys::signal::Signal;
use tokio::io::unix::AsyncFd;

/// One end of the control socket pair, before it is handed to tokio (the
/// relay's) or to a job (the child's).
pub(crate) type CtrlEnd = std::os::unix::net::UnixStream;
/// The relay's end of the control socket.
pub type CtrlStream = tokio::net::UnixStream;
/// The read half of a [`CtrlStream`].
pub type CtrlReadHalf = tokio::net::unix::OwnedReadHalf;
/// The write half of a [`CtrlStream`].
pub type CtrlWriteHalf = tokio::net::unix::OwnedWriteHalf;
/// A spawned job: its pid, its pipes (a pipe job's `stdout`/`stderr`), `wait`.
pub type Child = tokio::process::Child;

/// The control socket pair: `(parent, child)` ends.
///
/// ## Why `std::os::unix::net::UnixStream::pair` and not `nix::socketpair`
///
/// `nix` 0.29 gates `SockFlag::SOCK_CLOEXEC` to linux/BSD/solaris — it does not exist
/// on macOS, so `nix` cannot produce a close-on-exec pair here without a `#[cfg]` fork.
/// std sets `FD_CLOEXEC` on both ends on every unix, for free.
///
/// And CLOEXEC-by-default is load-bearing, not hygiene. This process forks from several
/// places concurrently (other jobs, PTY jobs). A
/// non-CLOEXEC child end would leak a copy into every one of those, and each leaked copy
/// holds the socket open — so the connector's EOF self-reap (`_o._exit(0)`) would never
/// fire and parked tools would outlive the kernel. [`child_setup`] then clears the flag
/// on exactly the one fd the child is meant to keep.
pub(crate) fn ctrl_pair() -> std::io::Result<(CtrlEnd, CtrlEnd)> {
    CtrlEnd::pair()
}

/// The number `FANTASTIC_CTRL_FD` advertises for a control-socket end: its fd.
///
/// Deliberately NOT a constant like 3: std dup2s stdio onto 0/1/2 before running
/// `pre_exec` and separately allocates an internal pipe to report exec failure, so
/// 3-6 are std's. Clobbering the exec-error pipe turns a failed spawn into a silent
/// hang. Whatever number the OS handed us is already ours and can never be 0/1/2 —
/// and [`child_setup`] keeps it at that same number across exec.
pub(crate) fn ctrl_number(end: &CtrlEnd) -> u64 {
    end.as_raw_fd() as u64
}

/// The relay's end, made async (non-blocking, registered with tokio).
pub(crate) fn ctrl_stream(end: CtrlEnd) -> std::io::Result<CtrlStream> {
    end.set_nonblocking(true)?;
    CtrlStream::from_std(end)
}

// ── what every child's environment holds (python `OS_ENV`) ────────────

/// The variables the OS needs in every process it starts, passed down whatever
/// the record's `inherit_env` says: none on unix.
pub(crate) const OS_ENV: &[&str] = &[];

// ── the child's own shell (python `SHELL_VAR` / `LOGIN_ARGS`) ─────────

/// The child env var that names its shell: the login shell the account runs.
pub(crate) const SHELL_VAR: &str = "SHELL";

/// What makes that shell a LOGIN shell — the tail `shell: "login"` permits.
pub(crate) const LOGIN_ARGS: &[&str] = &["-l"];

// ── the PATH grammar (python `proc.resolve`) ────────────────────────

/// The separator between `PATH` entries (`os.pathsep`).
pub(crate) const PATH_LIST_SEP: char = ':';

/// A command naming a file — exec'd as written, never searched: one with a `/`.
pub(crate) fn names_a_file(command: &str) -> bool {
    command.contains('/')
}

/// The file names a bare command may be found under in a `PATH` dir: itself.
pub(crate) fn exe_names(command: &str) -> Vec<String> {
    vec![command.to_string()]
}

/// The error a PATH search that found nothing answers (`ENOENT`).
pub(crate) fn not_found() -> std::io::Error {
    std::io::Error::from_raw_os_error(libc::ENOENT)
}

/// Whether `full` is a regular file this process may execute (`access(X_OK)`).
pub(crate) fn is_executable(full: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(full.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `access` on a NUL-terminated path we own.
    full.is_file() && unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0
}

// ── jobs ────────────────────────────────────────────────────────────

/// Start `program` (argv[0] `arg0`, then `args`) in `cwd` with EXACTLY `env`
/// (later pairs win), keeping the control socket end `ctrl` open at its own
/// number, leading a new session. A pipe job's stdout/stderr are piped (stdin is
/// the kernel's, as python's `create_subprocess_exec` leaves it); a PTY job's
/// three are the slave, its controlling terminal.
pub(crate) fn spawn(
    program: &Path,
    arg0: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: &str,
    ctrl: &CtrlEnd,
    pty: Option<PtySlave>,
) -> std::io::Result<Child> {
    // The RESOLVED file, argv[0] as the caller wrote it.
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg0(arg0);
    cmd.args(args).kill_on_drop(false);
    let is_pty = pty.is_some();
    match pty {
        None => {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        Some(PtySlave(slave)) => {
            cmd.stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave));
        }
    }
    // The kernel's own environment does not leak in (`env_clear`).
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.current_dir(cwd);
    // Keep the control fd across exec, lead a new session, take the PTY as the
    // controlling terminal.
    child_setup(&mut cmd, ctrl.as_raw_fd(), is_pty);
    cmd.spawn()
}

/// The child's post-fork setup: keep the control fd `ctrl_fd` across exec, lead a
/// new session, and for a PTY job take the slave on fd 0 as controlling terminal.
fn child_setup(cmd: &mut tokio::process::Command, ctrl_fd: i32, is_pty: bool) {
    // SAFETY: post-fork, pre-exec, in a multithreaded process — the closure must be
    // async-signal-safe. `fcntl` is; anything that allocates, logs, or takes a lock is
    // not. It captures the raw i32 ONLY: capturing the owning `UnixStream` would keep
    // the parent's copy of the child end alive for the Command's lifetime, and that copy
    // is exactly what must die (the caller drops it after spawning).
    unsafe {
        cmd.pre_exec(move || {
            // Clear FD_CLOEXEC so the control socket survives execve at the SAME number
            // the parent advertised in FANTASTIC_CTRL_FD.
            if libc::fcntl(ctrl_fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // EVERY job leads its own session, so `kill_session` takes the
            // command and everything it launched (`server &`, a `nohup`), and a
            // Ctrl-C at the kernel's terminal is the kernel's to handle.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // A PTY job's session has the slave (already on fd 0) as its
            // controlling terminal, so job control, Ctrl-C and SIGWINCH reach it.
            if is_pty && libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Every pid the OS lists right now — `proc_listallpids` on macOS, `/proc` on
/// Linux (python `proc._all_pids`). Picked by target, statically.
fn all_pids() -> Vec<i32> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: a null buffer asks for the count; the second call fills a buffer
        // sized for it (plus slack for processes born in between).
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize;
        let mut buf = vec![0i32; n + 64];
        let bytes = (buf.len() * std::mem::size_of::<i32>()) as libc::c_int;
        let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), bytes) }.max(0);
        buf.truncate(got as usize);
        buf.retain(|p| *p > 0);
        buf
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_dir("/proc")
            .map(|rd| {
                rd.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// SIGKILL every member of `leader`'s session, then the leader. Returns whether
/// the leader was signalled.
pub(crate) fn kill_session(leader: i32) -> bool {
    for pid in all_pids() {
        // SAFETY: `getsid`/`kill` on a plain pid; a vanished one returns -1.
        if pid != leader && unsafe { libc::getsid(pid) } == leader {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    // SAFETY: as above.
    unsafe { libc::kill(leader, libc::SIGKILL) == 0 }
}

/// Interrupt a job: SIGINT to its own process (python
/// `proc.send_signal(SIGINT)`) — a PTY job's too; its terminal is not involved.
/// Returns whether it was delivered.
pub(crate) fn interrupt(pid: u32, _pty: Option<&PtyMaster>) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGINT).is_ok()
}

/// python's `Process.returncode`: the exit status, or `-N` when a signal killed it.
pub(crate) fn exit_code(s: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    s.code().or_else(|| s.signal().map(|n| -n))
}

/// The exit code a job [`kill_session`] killed reports: `-SIGKILL`.
pub(crate) const KILLED_EXIT_CODE: i32 = -libc::SIGKILL;

/// Whether a failed [`spawn`] failed on its working directory rather than its
/// program. `chdir` and `execve` both answer `ENOENT`, so the cwd is blamed only
/// when it is in fact missing (CPython's `noexec:chdir` names it).
pub(crate) fn cwd_failed(e: &std::io::Error, cwd: &str) -> bool {
    e.kind() == std::io::ErrorKind::NotFound && !Path::new(cwd).is_dir()
}

// ── terminals ───────────────────────────────────────────────────────

/// The kernel's end of a job's terminal: the PTY master, non-blocking and
/// close-on-exec, registered with tokio.
pub(crate) struct PtyMaster(AsyncFd<OwnedFd>);

/// The job's end of a terminal: the PTY slave, which [`spawn`] makes the job's
/// stdin, stdout, stderr and controlling terminal.
pub(crate) struct PtySlave(OwnedFd);

/// A fresh pseudo-terminal sized `cols`×`rows`.
pub(crate) fn open_pty(cols: u16, rows: u16) -> std::io::Result<(PtyMaster, PtySlave)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: valid out-pointers; a null name/termios is allowed.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openpty handed us two fresh fds we now own.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // The master stays with the kernel only: CLOEXEC so no other spawn inherits it.
    // SAFETY: fcntl on an fd we own.
    unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    set_nonblocking(&master)?;
    Ok((PtyMaster(AsyncFd::new(master)?), PtySlave(slave)))
}

impl PtyMaster {
    /// One read of what the program wrote. `Ok(0)` — or an error: EIO once the
    /// child closed the slave — is the end of the terminal.
    pub(crate) async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = self.0.readable().await?;
            match guard.try_io(|inner| read_fd(inner.get_ref(), buf)) {
                Ok(r) => return r,
                Err(_would_block) => continue,
            }
        }
    }

    /// One write of `data` as typed input; may write short (a full tty buffer).
    pub(crate) async fn write(&self, data: &[u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = self.0.writable().await?;
            match guard.try_io(|inner| write_fd(inner.get_ref(), data)) {
                Ok(r) => return r,
                Err(_would_block) => continue,
            }
        }
    }

    /// Set the window size (the tty sends SIGWINCH to the program).
    pub(crate) fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: a valid winsize on an fd we own.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCSWINSZ, &ws) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Put an fd in non-blocking mode (`O_NONBLOCK`).
fn set_nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: fcntl on an fd we own.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// One `read(2)` on `fd` into `buf`.
fn read_fd(fd: &OwnedFd, buf: &mut [u8]) -> std::io::Result<usize> {
    // SAFETY: a valid buffer on an fd we own.
    let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

/// One `write(2)` of `data` on `fd`; may write short.
fn write_fd(fd: &OwnedFd, data: &[u8]) -> std::io::Result<usize> {
    // SAFETY: a valid buffer on an fd we own.
    let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

/// python `tempfile.mkdtemp(prefix=…)` — a 0700 dir in the system temp dir.
pub(crate) fn mkdtemp(prefix: &str) -> std::io::Result<PathBuf> {
    let template = std::env::temp_dir().join(format!("{prefix}XXXXXXXX"));
    let mut bytes = template.to_string_lossy().into_owned().into_bytes();
    bytes.push(0);
    // SAFETY: a NUL-terminated, writable template buffer.
    let ptr = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
    if ptr.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    bytes.pop();
    Ok(PathBuf::from(String::from_utf8_lossy(&bytes).to_string()))
}

// ── test fixtures (python's tests use the same shell scripts) ───────

/// TEST ONLY: the members of `leader`'s job other than the leader — its SESSION.
#[cfg(test)]
pub(crate) fn job_members(leader: u32) -> Vec<u32> {
    let leader = leader as i32;
    all_pids()
        .into_iter()
        // SAFETY: `getsid` on a plain pid.
        .filter(|p| *p != leader && unsafe { libc::getsid(*p) } == leader)
        .map(|p| p as u32)
        .collect()
}

/// TEST ONLY: an executable script `dir/<stem>` that prints `text` and exits 0.
#[cfg(test)]
pub(crate) fn echo_script(dir: &Path, stem: &str, text: &str) -> PathBuf {
    script(dir, stem, &format!("#!/bin/sh\necho {text}\n"))
}

/// TEST ONLY: an executable stand-in for a login shell, `dir/<stem>`: it prints
/// `SHELL_UP <its args>`, then reads one typed line and prints `GOT <line>`.
#[cfg(test)]
pub(crate) fn fake_shell(dir: &Path, stem: &str) -> PathBuf {
    script(
        dir,
        stem,
        "#!/bin/sh\necho \"SHELL_UP $*\"\nread line\necho \"GOT $line\"\n",
    )
}

/// `dir/<stem>` holding `body`, mode 0755.
#[cfg(test)]
fn script(dir: &Path, stem: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(stem);
    std::fs::write(&path, body).expect("write the script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make it executable");
    path
}

/// TEST ONLY: a PIPE job that launches two background processes of its own and
/// then waits — `(command, args, env)`; the line's `sleep &` and `nohup` must
/// both die with the job.
#[cfg(test)]
pub(crate) fn launcher_job() -> (String, Vec<String>, serde_json::Value) {
    (
        "/bin/sh".to_string(),
        vec![
            "-c".to_string(),
            "sleep 303 & nohup sleep 304 >/dev/null 2>&1 & sleep 305".to_string(),
        ],
        serde_json::json!({"PATH": "/usr/bin:/bin"}),
    )
}

/// TEST ONLY: an interactive shell for a PTY job, and what to type into it to
/// launch two background jobs (each in its OWN process group) and then wait —
/// `(command, args, env, typed)`.
#[cfg(test)]
pub(crate) fn interactive_shell() -> (String, Vec<String>, serde_json::Value, String) {
    (
        "/bin/sh".to_string(),
        vec!["-i".to_string()],
        serde_json::json!({"PATH": "/usr/bin:/bin"}),
        "sleep 300 &\nnohup sleep 301 >/dev/null 2>&1 &\nsleep 302\n".to_string(),
    )
}

/// TEST ONLY: what an interactive program takes as Enter — the tty's newline.
#[cfg(test)]
pub(crate) const ENTER: &str = "\n";

/// TEST ONLY: a pipe-job `(command, args)` that prints its environment's
/// `FT_SECRET_RS` and `FT_OPEN_RS` (`none` when unset) as `secret/open`.
#[cfg(test)]
pub(crate) fn print_env_job() -> (String, Vec<String>) {
    (
        "/bin/sh".to_string(),
        vec![
            "-c".to_string(),
            "echo ${FT_SECRET_RS:-none}/${FT_OPEN_RS:-none}".to_string(),
        ],
    )
}

/// TEST ONLY: the errno a spawn into a missing `cwd` fails with — chdir's
/// `ENOENT`.
#[cfg(test)]
pub(crate) const MISSING_CWD_ERRNO: i32 = libc::ENOENT;
