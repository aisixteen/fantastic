//! Spawning a COMMAND, with a control socket it can talk back on.
//!
//! A spawned program MAY speak the kernel over the inherited control fd; one that
//! never reads it runs exactly like any OS process. This module is the PARENT half of
//! that wire: it makes the socket pair, hands one end to the child, and returns the
//! other for the relay. Mirror of python `proc.spawn_argv`: spawn argv AS GIVEN — the
//! gate is the caller's regex allowlist on the shell-joined line, and this function
//! adds nothing to it.
//!
//! The OS half — the close-on-exec socket pair, the session, the PTY — lives in
//! [`crate::sys`].

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Map, Value};
use tokio::process::{Child, Command};

use crate::sys::{self, OwnedFd, StdUnixStream, UnixStream};

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
        if sys::is_executable(&full) {
            return Ok(full);
        }
    }
    Err(sys::not_found())
}

/// Every pid the OS lists right now, for the teardown tests that look for survivors.
#[cfg(test)]
pub(crate) fn all_pids_for_tests() -> Vec<i32> {
    sys::all_pids()
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
    sys::kill_session(leader)
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
    let (master, slave) = sys::open_pty(cols, rows)?;
    let spawned = spawn(argv, self_id, env, cwd, Some(slave))?;
    Ok((spawned, master))
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
    let (parent, child_end) = sys::socket_pair()?;
    let child_raw = sys::raw_fd(&child_end);

    // The RESOLVED file, argv[0] as the caller wrote it.
    let mut cmd = Command::new(&program);
    sys::set_arg0(&mut cmd, exe);
    cmd.args(rest).kill_on_drop(false);
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

    // Keep the control fd across exec, lead a new session, take the PTY as the
    // controlling terminal.
    sys::child_setup(&mut cmd, child_raw, is_pty);

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
    sys::raw_fd(s)
}
