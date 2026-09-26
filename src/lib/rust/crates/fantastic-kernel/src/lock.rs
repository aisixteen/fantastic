//! `.fantastic/lock.yaml` PID guard.
//!
//! Shape: `pid: <u32>` (a YAML mapping, [`crate::yaml_io::dump`]). A daemon writes the lock at boot; later
//! invocations refuse with `KernelError::LockHeld` if the pid is alive.
//! Stale locks (dead pid) get overwritten — matches Python's behavior.
//!
//! Liveness check: `libc::kill(pid, 0)` returns `Ok` if the process
//! exists (or `EPERM` if we lack permission but the process IS alive);
//! it returns `ESRCH` if the process is gone.

use crate::errors::{KernelError, KernelResult};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
struct LockFile {
    pid: u32,
}

/// Check whether a pid is alive on this system.
///
/// On unix: `kill(pid, 0)`. On other platforms: assume alive (we
/// only run macOS + Linux today; Windows handling lands when we
/// add Windows CI).
pub fn pid_alive(pid: u32) -> bool {
    // python `_pid_alive`: `pid <= 0` is dead, no syscall. `kill(0, sig)`
    // targets the CALLER'S OWN process GROUP (not "pid 0" as a process), so
    // without this a lock recorded `{"pid": 0}` read back as alive — C83.
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // Safety: kill(pid, 0) only checks existence + permission; no
        // signal is actually delivered.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        // -1 + ESRCH = dead; -1 + EPERM = alive but not ours.
        let errno = std::io::Error::last_os_error().raw_os_error();
        errno == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Acquire the workdir lock for the current process. Writes
/// `.fantastic/lock.yaml` with our pid.
///
/// If the lock file already exists AND the pid in it is alive,
/// returns [`KernelError::LockHeld`]. If the pid is dead, the lock
/// is stale and gets overwritten.
pub fn acquire(workdir: &Path) -> KernelResult<PathBuf> {
    let path = workdir.join(super::persistence::LOCK_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| KernelError::LockIo {
            path: path.clone(),
            source: e,
        })?;
    }
    if path.exists() {
        let raw = fs::read_to_string(&path).map_err(|e| KernelError::LockIo {
            path: path.clone(),
            source: e,
        })?;
        if let Some(existing) = parse_lock(&raw) {
            if pid_alive(existing.pid) {
                return Err(KernelError::LockHeld { pid: existing.pid });
            }
            // Stale — fall through to overwrite.
        }
    }
    let me = std::process::id();
    // python `yaml.dump({"pid": os.getpid()}, CSafeDumper, sort_keys=False)`.
    let body = crate::yaml_io::dump(&serde_json::json!({"pid": me}));
    fs::write(&path, body).map_err(|e| KernelError::LockIo {
        path: path.clone(),
        source: e,
    })?;
    Ok(path)
}

/// Release the workdir lock — but only if THIS process holds it.
/// Mirrors Python's `release_lock` (`_lock.py:63-70`): reads the
/// lock file's `pid`, compares to `std::process::id()`, and removes
/// only on match. If a foreign pid owns it (e.g. a second daemon
/// raced us, or our own release runs after the lock was already
/// reclaimed), the file stays — we don't yank someone else's lock.
/// No error on missing file (idempotent shutdown path).
pub fn release(workdir: &Path) -> KernelResult<()> {
    let path = workdir.join(super::persistence::LOCK_PATH);
    if !path.exists() {
        return Ok(());
    }
    // python `release_lock`: `cur = _read_lock()` is `None` on ANY read/parse
    // failure, and `if cur and cur.get("pid") == os.getpid(): unlink()` — a
    // malformed file is left ALONE, never removed (C83; this used to fall
    // through to `remove_file` on a parse error, which could delete a lock we
    // never verified we own).
    let Ok(raw) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let Some(lock) = parse_lock(&raw) else {
        return Ok(());
    };
    if lock.pid != std::process::id() {
        // Someone else owns this lock — leave it alone.
        return Ok(());
    }
    fs::remove_file(&path).map_err(|e| KernelError::LockIo { path, source: e })?;
    Ok(())
}

/// Read the pid currently in the lock file, if any. Used by callers
/// that want to attach to a running daemon (e.g. one-shot CLI
/// invocations detecting a serve in the same workdir).
pub fn current_holder(workdir: &Path) -> KernelResult<Option<u32>> {
    let path = workdir.join(super::persistence::LOCK_PATH);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(|e| KernelError::LockIo {
        path: path.clone(),
        source: e,
    })?;
    let lock: LockFile = parse_lock(&raw).ok_or_else(|| KernelError::LockIo {
        path: path.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, "lock.yaml malformed"),
    })?;
    Ok(Some(lock.pid))
}

/// python `_read_lock`: the YAML mapping, or `None` on any parse failure.
fn parse_lock(raw: &str) -> Option<LockFile> {
    let v = crate::yaml_io::load(raw).ok()?;
    serde_json::from_value(v).ok()
}

#[cfg(test)]
mod tests;
