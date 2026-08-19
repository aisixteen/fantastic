//! Spawning a durable python child, with a control socket it can talk back on.
//!
//! The child half of the wire is [`crate::CONNECTOR_SRC`] — python source, vendored
//! verbatim, prepended to the program. This module is the PARENT half: it makes the
//! socket pair, hands one end to the child, and returns the other for the relay.
//!
//! ## Why `std::os::unix::net::UnixStream::pair` and not `nix::socketpair`
//!
//! `nix` 0.29 gates `SockFlag::SOCK_CLOEXEC` to linux/BSD/solaris — it does not exist
//! on macOS, so `nix` cannot produce a close-on-exec pair here without a `#[cfg]` fork.
//! std sets `FD_CLOEXEC` on both ends on every unix, for free.
//!
//! And CLOEXEC-by-default is load-bearing, not hygiene. This process forks from several
//! places concurrently (other jobs, `terminal_backend` PTYs, `ssh_runner`'s tunnel). A
//! non-CLOEXEC child end would leak a copy into every one of those, and each leaked copy
//! holds the socket open — so the connector's EOF self-reap (`_o._exit(0)`) would never
//! fire and parked tools would outlive the kernel. We then clear the flag on exactly the
//! one fd the child is meant to keep, in `pre_exec`.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::process::Stdio;

use tokio::net::UnixStream;
use tokio::process::{Child, Command};

/// A spawned durable job: the OS child, plus the parent end of its control socket.
pub struct Spawned {
    /// The running python process.
    pub child: Child,
    /// Parent end of the control socket — hand this to the relay.
    pub ctrl: UnixStream,
}

/// Spawn `argv` (already allowlist-resolved and shim-applied by the caller —
/// [`crate::resolve_interpreter`] + [`crate::apply_shim`] + [`crate::build_argv`]
/// are the ONLY path here, which is what makes the allowlist unbypassable) with a
/// control socket on an inherited fd.
///
/// `args_json` reaches the program as `kernel.args`; `self_id` is the OWNER agent id
/// (the tool's public id, not an ephemeral job id) so a tool can address itself by the
/// id others use.
pub fn spawn_job(
    argv: &[String],
    self_id: &str,
    args_json: &str,
    cwd: Option<&str>,
) -> std::io::Result<Spawned> {
    let Some((exe, rest)) = argv.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty argv",
        ));
    };
    let (parent, child_end) = StdUnixStream::pair()?;
    let child_raw = child_end.as_raw_fd();

    let mut cmd = Command::new(exe);
    cmd.args(rest)
        .env("FANTASTIC_CTRL_FD", child_raw.to_string())
        .env("FANTASTIC_SELF_ID", self_id)
        .env("FANTASTIC_ARGS", args_json)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    if let Some(c) = cwd.filter(|c| !c.is_empty()) {
        cmd.current_dir(c);
    }

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

/// The fd number the child was told to use. Exposed for tests.
///
/// Deliberately NOT a constant like 3: std dup2s stdio onto 0/1/2 before running
/// `pre_exec` and separately allocates an internal pipe to report exec failure, so
/// 3-6 are std's. Clobbering the exec-error pipe turns a failed spawn into a silent
/// hang. Whatever number the OS handed us is already ours and can never be 0/1/2.
pub fn advertised_fd(s: &StdUnixStream) -> i32 {
    s.as_raw_fd()
}
