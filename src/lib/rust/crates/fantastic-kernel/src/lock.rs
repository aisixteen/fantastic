//! `.fantastic/lock.json` PID guard.
//!
//! Shape: `{"pid": <u32>}`. A daemon writes the lock at boot; later
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
        let _ = pid;
        true
    }
}

/// Acquire the workdir lock for the current process. Writes
/// `.fantastic/lock.json` with our pid.
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
        if let Ok(existing) = serde_json::from_str::<LockFile>(&raw) {
            if pid_alive(existing.pid) {
                return Err(KernelError::LockHeld { pid: existing.pid });
            }
            // Stale — fall through to overwrite.
        }
    }
    let me = std::process::id();
    let lock = LockFile { pid: me };
    let json = serde_json::to_string(&lock).expect("LockFile always serializable");
    fs::write(&path, json).map_err(|e| KernelError::LockIo {
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
    // Read the holder pid; bail if the file is malformed (don't touch).
    let raw = fs::read_to_string(&path).map_err(|e| KernelError::LockIo {
        path: path.clone(),
        source: e,
    })?;
    if let Ok(lock) = serde_json::from_str::<LockFile>(&raw) {
        if lock.pid != std::process::id() {
            // Someone else owns this lock — leave it alone.
            return Ok(());
        }
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
    let lock: LockFile = serde_json::from_str(&raw).map_err(|_| KernelError::LockIo {
        path: path.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, "lock.json malformed"),
    })?;
    Ok(Some(lock.pid))
}

#[cfg(test)]
mod tests;
