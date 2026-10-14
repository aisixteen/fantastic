//! Spawning a COMMAND, with a control socket it can talk back on.
//!
//! A spawned program MAY speak the kernel over the inherited control fd; one that
//! never reads it runs exactly like any OS process. This module is the PARENT half of
//! that wire: it makes the socket pair, hands one end to the child, and returns the
//! other for the relay. Mirror of python `proc.spawn_argv`: spawn argv AS GIVEN — the
//! gate is the caller's regex allowlist on the shell-joined line, and this function
//! adds nothing to it.
//!
//! ## Why `std::os::unix::net::UnixStream::pair` and not `nix::socketpair`
//!
//! `nix` 0.29 gates `SockFlag::SOCK_CLOEXEC` to linux/BSD/solaris — it does not exist
//! on macOS, so `nix` cannot produce a close-on-exec pair here without a `#[cfg]` fork.
//! std sets `FD_CLOEXEC` on both ends on every unix, for free.
//!
//! And CLOEXEC-by-default is load-bearing, not hygiene. This process forks from several
//! places concurrently (other jobs, PTY jobs). A
//! non-CLOEXEC child end would leak a copy into every one of those, and each leaked copy
//! holds the socket open — so the connector's EOF self-reap (`_o._exit(0)`) would never
//! fire and parked tools would outlive the kernel. We then clear the flag on exactly the
//! one fd the child is meant to keep, in `pre_exec`.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Map, Value};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

/// A bare command, and the child's environment has no `PATH` to find it in — the
/// error payload [`spawn_job`]/[`spawn_pty`] return for it (python `proc.NoPath`).
#[derive(Debug)]
pub struct NoPath;

impl std::fmt::Display for NoPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no PATH in the child environment")
    }
}

impl std::error::Error for NoPath {}

/// python `proc.resolve`: argv[0] → the file to exec. A command with a `/` is exec'd
/// as written (relative to `cwd`). A bare name is looked up in the CHILD's `PATH`
/// only — never the kernel's, never a built-in default path (std's search falls back
/// to both); a child env without `PATH` is [`NoPath`].
fn resolve(command: &str, cwd: &str, env: &Map<String, Value>) -> std::io::Result<PathBuf> {
    if command.contains('/') {
        return Ok(PathBuf::from(command));
    }
    let Some(path) = env.get("PATH") else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            NoPath,
        ));
    };
    for dir in value_to_env(path).split(':') {
        // POSIX: an empty entry is the working directory — the CHILD's.
        let rel = Path::new(cwd)
            .join(if dir.is_empty() { "." } else { dir })
            .join(command);
        let full = std::path::absolute(&rel)?;
        let Ok(c) = std::ffi::CString::new(full.as_os_str().as_bytes()) else {
            continue;
        };
        // SAFETY: `access` on a NUL-terminated path we own.
        if full.is_file() && unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0 {
            return Ok(full);
        }
    }
    Err(std::io::Error::from_raw_os_error(libc::ENOENT))
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

/// [`all_pids`], for the teardown tests that look for survivors.
#[cfg(test)]
pub(crate) fn all_pids_for_tests() -> Vec<i32> {
    all_pids()
}

/// SIGKILL a job's WHOLE session: every member first, then the leader (python
/// `proc.kill_session`). Every job leads its own session, and what it launched
/// stays there — a script's `server &`, a `nohup`, and each job an interactive
/// shell puts in its OWN process group, which neither a kill of the leader nor a
/// tty hangup (foreground group only) reaches. Once the leader is gone they
/// reparent to init — so the members are found by session id BEFORE it dies. A process that `setsid()`s out of the
/// session leaves it by design: containment beyond that is the OS's. Returns
/// whether the leader was signalled.
pub fn kill_session(leader: i32) -> bool {
    for pid in all_pids() {
        // SAFETY: `getsid`/`kill` on a plain pid; a vanished one returns -1.
        if pid != leader && unsafe { libc::getsid(pid) } == leader {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    // SAFETY: as above.
    unsafe { libc::kill(leader, libc::SIGKILL) == 0 }
}

/// A spawned job: the OS child, plus the parent end of its control socket.
pub struct Spawned {
    /// The running process.
    pub child: Child,
    /// Parent end of the control socket — hand this to the relay.
    pub ctrl: UnixStream,
}

/// Spawn `argv` AS GIVEN (already gated by the caller's regex allowlist on the
/// shell-joined line — [`crate::command_permitted`] is the ONLY path here, which is
/// what makes the allowlist unbypassable) with a control socket on an inherited fd.
///
/// Mirror of python `proc.spawn_argv`: the child gets EXACTLY `env` (the caller
/// composed it from the record — the inherited names, the operator's extras) —
/// nothing leaks in from the kernel's own environment — and this function
/// additionally sets `FANTASTIC_CTRL_FD` (the control-wire fd) and
/// `FANTASTIC_SELF_ID` (the OWNER agent id — the tool's public id, not an ephemeral
/// job id, so a tool can address itself by the id others use). There is no
/// `FANTASTIC_ARGS`: a general-purpose command runner passes arguments as argv.
pub fn spawn_job(
    argv: &[String],
    self_id: &str,
    env: &Map<String, Value>,
    cwd: &str,
) -> std::io::Result<Spawned> {
    spawn(argv, self_id, env, cwd, None)
}

/// A PTY job: the OS child on the SLAVE end of a fresh pseudo-terminal (stdin,
/// stdout and stderr), leading a new session with that tty as its controlling
/// terminal. Same gate, same env rule, same control fd as [`spawn_job`] — a
/// terminal is a job, not a second spawner. Returns the MASTER end beside it.
pub fn spawn_pty(
    argv: &[String],
    self_id: &str,
    env: &Map<String, Value>,
    cwd: &str,
    cols: u16,
    rows: u16,
) -> std::io::Result<(Spawned, OwnedFd)> {
    let (master, slave) = open_pty(cols, rows)?;
    let spawned = spawn(argv, self_id, env, cwd, Some(slave))?;
    Ok((spawned, master))
}

fn open_pty(cols: u16, rows: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
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
    Ok((master, slave))
}

fn spawn(
    argv: &[String],
    self_id: &str,
    env: &Map<String, Value>,
    cwd: &str,
    pty_slave: Option<OwnedFd>,
) -> std::io::Result<Spawned> {
    let Some((exe, rest)) = argv.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty argv",
        ));
    };
    let program = resolve(exe, cwd, env)?;
    let (parent, child_end) = StdUnixStream::pair()?;
    let child_raw = child_end.as_raw_fd();

    // The RESOLVED file, argv[0] as the caller wrote it.
    let mut cmd = Command::new(&program);
    cmd.arg0(exe).args(rest).kill_on_drop(false);
    let is_pty = pty_slave.is_some();
    match pty_slave {
        None => {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        Some(slave) => {
            cmd.stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave));
        }
    }
    // THE CHILD'S WHOLE ENVIRONMENT is the record's — each value stringified,
    // exactly like python's `{str(k): str(v) …}`. The kernel's own environment
    // does not leak in (`env_clear`).
    //
    // The two control vars are written OVER it, so a record `env` cannot
    // make a job claim another agent's identity, nor point `FANTASTIC_CTRL_FD` at a
    // descriptor it chose.
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, value_to_env(v));
    }
    cmd.env("FANTASTIC_CTRL_FD", child_raw.to_string())
        .env("FANTASTIC_SELF_ID", self_id);
    cmd.current_dir(cwd);

    // SAFETY: post-fork, pre-exec, in a multithreaded process — the closure must be
    // async-signal-safe. `fcntl` is; anything that allocates, logs, or takes a lock is
    // not. It captures the raw i32 ONLY: capturing the owning `UnixStream` would keep
    // the parent's copy of the child end alive for the Command's lifetime, and that copy
    // is exactly what must die (see the drop below).
    unsafe {
        cmd.pre_exec(move || {
            // Clear FD_CLOEXEC so the control socket survives execve at the SAME number
            // the parent advertised in FANTASTIC_CTRL_FD.
            if libc::fcntl(child_raw, libc::F_SETFD, 0) == -1 {
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

    let spawned = cmd.spawn();
    // DROP THE CHILD END ON BOTH PATHS. If the parent keeps it, the socket never reaches
    // EOF when the relay closes, the connector's reader thread never returns, and
    // `_o._exit(0)` never runs — the durable tool becomes an orphan. Python does this in
    // a `finally:` for the same reason.
    drop(child_end);
    let child = spawned?;

    parent.set_nonblocking(true)?;
    let ctrl = UnixStream::from_std(parent)?;
    Ok(Spawned { child, ctrl })
}

/// Stringify an env value exactly as python's `str(v)` does — a string as itself,
/// everything else through CPython's repr (`True`, not `true`).
fn value_to_env(v: &Value) -> String {
    fantastic_bundle::py_str(v)
}

/// The fd number the child was told to use. Exposed for tests.
///
/// Deliberately NOT a constant like 3: std dup2s stdio onto 0/1/2 before running
/// `pre_exec` and separately allocates an internal pipe to report exec failure, so
/// 3-6 are std's. Clobbering the exec-error pipe turns a failed spawn into a silent
/// hang. Whatever number the OS handed us is already ours and can never be 0/1/2.
pub fn advertised_fd(s: &StdUnixStream) -> i32 {
    s.as_raw_fd()
}
