//! Spawning a COMMAND, with a control socket it can talk back on.
//!
//! A spawned program MAY speak the kernel over the inherited control fd; one that
//! never reads it runs exactly like any OS process. This module is the PARENT half of
//! that wire: it makes the socket pair, hands one end to the child, and returns the
//! other for the relay. Mirror of python `proc.spawn_argv`: spawn argv AS GIVEN — the
//! gate is the caller's regex allowlist on the shell-joined line, and this function
//! adds nothing to it.
//!
//! The OS half — the socket pair, the job's container (a session, a Job Object),
//! the PTY — lives in [`crate::sys`].

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::sys::{self, Child, CtrlStream};

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

/// python `proc.resolve`: argv[0] → the file to exec. A command naming a file (a
/// `/` in it; on Windows a `\` too) is exec'd as written — relative to `cwd` on
/// unix, to the kernel's directory on Windows (`CreateProcessW` resolves it before
/// the job has a directory, as python's `subprocess` does there). A bare name is looked up in the CHILD's `PATH` only — never the kernel's, never a
/// built-in default path (std's search falls back to both); a child env without
/// `PATH` is [`NoPath`]. Each `PATH` dir is tried under each name the OS runs the
/// command as (on Windows: with its `PATHEXT` extensions, `shutil.which`'s order).
fn resolve(command: &str, cwd: &str, env: &Map<String, Value>) -> std::io::Result<PathBuf> {
    if sys::names_a_file(command) {
        return Ok(PathBuf::from(command));
    }
    let Some(path) = env.get("PATH") else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            NoPath,
        ));
    };
    let names = sys::exe_names(command);
    for dir in value_to_env(path).split(sys::PATH_LIST_SEP) {
        // POSIX: an empty entry is the working directory — the CHILD's.
        let base = Path::new(cwd).join(if dir.is_empty() { "." } else { dir });
        for name in &names {
            let full = std::path::absolute(base.join(name))?;
            if sys::is_executable(&full) {
                return Ok(full);
            }
        }
    }
    Err(sys::not_found())
}

/// SIGKILL a job's WHOLE session: every member first, then the leader (python
/// `proc.kill_session`). Every job leads its own session, and what it launched
/// stays there — a script's `server &`, a `nohup`, and each job an interactive
/// shell puts in its OWN process group, which neither a kill of the leader nor a
/// tty hangup (foreground group only) reaches. Once the leader is gone they
/// reparent to init — so the members are found by session id BEFORE it dies. A process that `setsid()`s out of the
/// session leaves it by design: containment beyond that is the OS's. On Windows
/// the session is the job's Job Object, terminated whole. Returns whether the
/// leader was signalled.
pub fn kill_session(leader: i32) -> bool {
    sys::kill_session(leader)
}

/// A spawned job: the OS child, plus the parent end of its control socket.
pub struct Spawned {
    /// The running process.
    pub child: Child,
    /// Parent end of the control socket — hand this to the relay.
    pub ctrl: CtrlStream,
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
pub(crate) fn spawn_pty(
    argv: &[String],
    self_id: &str,
    env: &Map<String, Value>,
    cwd: &str,
    cols: u16,
    rows: u16,
) -> std::io::Result<(Spawned, sys::PtyMaster)> {
    let (master, slave) = sys::open_pty(cols, rows)?;
    let spawned = spawn(argv, self_id, env, cwd, Some(slave))?;
    Ok((spawned, master))
}

fn spawn(
    argv: &[String],
    self_id: &str,
    env: &Map<String, Value>,
    cwd: &str,
    pty_slave: Option<sys::PtySlave>,
) -> std::io::Result<Spawned> {
    let Some((exe, rest)) = argv.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty argv",
        ));
    };
    let program = resolve(exe, cwd, env)?;
    let (parent, child_end) = sys::ctrl_pair()?;

    // THE CHILD'S WHOLE ENVIRONMENT is the record's — each value stringified,
    // exactly like python's `{str(k): str(v) …}`. The kernel's own environment
    // does not leak in.
    //
    // The two control vars are written OVER it (a later pair wins), so a record
    // `env` cannot make a job claim another agent's identity, nor point
    // `FANTASTIC_CTRL_FD` at a descriptor it chose.
    let mut child_env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.clone(), value_to_env(v)))
        .collect();
    child_env.push((
        "FANTASTIC_CTRL_FD".to_string(),
        advertised_fd(&child_end).to_string(),
    ));
    child_env.push(("FANTASTIC_SELF_ID".to_string(), self_id.to_string()));

    // The RESOLVED file, argv[0] as the caller wrote it; the control end kept
    // open in the child at the number advertised; a new session (Job Object).
    let spawned = sys::spawn(&program, exe, rest, &child_env, cwd, &child_end, pty_slave);
    // DROP THE CHILD END ON BOTH PATHS. If the parent keeps it, the socket never reaches
    // EOF when the relay closes, the connector's reader thread never returns, and
    // `_o._exit(0)` never runs — the durable tool becomes an orphan. Python does this in
    // a `finally:` for the same reason.
    drop(child_end);
    let child = spawned?;

    let ctrl = sys::ctrl_stream(parent)?;
    Ok(Spawned { child, ctrl })
}

/// Stringify an env value exactly as python's `str(v)` does — a string as itself,
/// everything else through CPython's repr (`True`, not `true`).
fn value_to_env(v: &Value) -> String {
    fantastic_bundle::py_str(v)
}

/// The number the child is told to use (`FANTASTIC_CTRL_FD`): the control end's
/// own fd on unix, its SOCKET handle on Windows — see [`sys::ctrl_number`] for why
/// it is never a fixed one.
fn advertised_fd(s: &sys::CtrlEnd) -> u64 {
    sys::ctrl_number(s)
}
