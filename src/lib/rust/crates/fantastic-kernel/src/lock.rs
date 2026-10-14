//! `.fantastic/lock.yaml` — one kernel per workdir, python `kernel/_lock.py`.
//!
//! The decision is an `flock(LOCK_EX)` on `lock.yaml`, held on an open
//! descriptor for the life of the process: the OS grants it to exactly one
//! opener and drops it the moment its holder exits — however it exits — so a
//! dead holder's lock is simply taken over. Python, rust and swift take the
//! SAME flock on the SAME file, so kernels of different runtimes exclude each
//! other too.
//!
//! Shape: `pid: <u32>` (a YAML mapping, [`crate::yaml_io::dump`]) — the
//! holder's pid, for the refusal text ([`KernelError::LockHeld`]), never for
//! the decision. It is in the file BEFORE the file is `lock.yaml`: the pid goes
//! into a private `lock.yaml.tmp.<hex>`, which is flocked and then hard-linked
//! to `lock.yaml` (atomic, refused when the name exists), so no opener ever
//! reads a lock without its pid. A `lock.yaml` nobody holds (its holder died)
//! is replaced by renaming the staged file over it while holding the dead
//! file's flock — a second taker of the same dead file finds it held.
//!
//! The one-shot CLI paths (`call`, `init`) take this same flock through the
//! bootstrap and refuse on [`KernelError::LockHeld`] — they never judge a pid
//! alive. A caller that looks for a running daemon to
//! ATTACH to asks [`held`] — the same flock, probed, never taken.

use crate::errors::{KernelError, KernelResult};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
struct LockFile {
    pid: u32,
}

/// The open descriptors whose flocks ARE this process's locks, by lock path —
/// python's `_held`. A lock is held exactly as long as its entry is here.
static HELD: Mutex<Vec<(PathBuf, File)>> = Mutex::new(Vec::new());

/// Check whether a pid is alive on this system.
///
/// On unix: `kill(pid, 0)`. On other platforms: assume alive (we
/// only run macOS + Linux today; Windows handling lands when we
/// add Windows CI).
pub fn pid_alive(pid: u32) -> bool {
    // python `_pid_alive`: `pid <= 0` is dead, no syscall. `kill(0, sig)`
    // targets the CALLER'S OWN process GROUP (not "pid 0" as a process), so
    // without this a lock recorded `{"pid": 0}` would read back as alive.
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

/// Acquire the workdir lock for the current process: take the flock on
/// `.fantastic/lock.yaml` and keep its descriptor open until [`release`].
///
/// If a live process holds it, returns [`KernelError::LockHeld`] naming the
/// pid in the file. A lock whose holder died is taken over.
pub fn acquire(workdir: &Path) -> KernelResult<PathBuf> {
    let path = workdir.join(super::persistence::LOCK_PATH);
    let io = |e: std::io::Error| KernelError::LockIo {
        path: path.clone(),
        source: e,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    let staged = path.with_file_name(format!(
        "lock.yaml.tmp.{}",
        crate::lifecycle::hex(&crate::lifecycle::os_random(4))
    ));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&staged)
        .map_err(io)?;
    // python `yaml.dump({"pid": os.getpid()}, CSafeDumper, sort_keys=False)`.
    let body = crate::yaml_io::dump(&serde_json::json!({"pid": std::process::id()}));
    let published = (|| -> KernelResult<()> {
        file.write_all(body.as_bytes()).map_err(io)?;
        flock(&file, libc::LOCK_EX).map_err(io)?; // uncontended: the staged file is ours alone
        while !publish(&staged, &path)? {}
        Ok(())
    })();
    // After a link the staged name is a second name for `lock.yaml`; after a
    // rename it is gone; after a refusal it is ours to drop.
    let cleaned = match fs::remove_file(&staged) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(io(e)),
        _ => Ok(()),
    };
    published?;
    cleaned?;
    HELD.lock()
        .expect("lock registry poisoned")
        .push((path.clone(), file));
    Ok(path)
}

/// One attempt to make the staged (flocked, pid-filled) file `lock.yaml` —
/// python `_publish`. `true` once it is; `false` when `lock.yaml` changed under
/// us (try again); [`KernelError::LockHeld`] when a live process holds it.
fn publish(staged: &Path, path: &Path) -> KernelResult<bool> {
    let io = |e: std::io::Error| KernelError::LockIo {
        path: path.to_path_buf(),
        source: e,
    };
    match fs::hard_link(staged, path) {
        Ok(()) => return Ok(true),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io(e)),
    }
    let mut cur = match File::open(path) {
        Ok(f) => f,
        // its holder released it between the two calls
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io(e)),
    };
    if let Err(e) = flock(&cur, libc::LOCK_EX | libc::LOCK_NB) {
        if e.kind() != ErrorKind::WouldBlock {
            return Err(io(e));
        }
        // Held: a live owner — or a taker still replacing a dead lock, whose
        // file (and dead pid) this racer then reads. Either way, refused.
        let mut raw = Vec::new();
        cur.read_to_end(&mut raw).map_err(io)?;
        let held = std::str::from_utf8(&raw).ok().and_then(parse_lock);
        return Err(match held {
            Some(lock) => KernelError::LockHeld { pid: lock.pid },
            // python's `lock file at .fantastic/lock.yaml: lock.yaml malformed`.
            None => KernelError::LockIo {
                path: PathBuf::from(super::persistence::LOCK_PATH),
                source: std::io::Error::new(ErrorKind::InvalidData, "lock.yaml malformed"),
            },
        });
    }
    // Nobody holds it: its holder died. Replace it — but only while it is
    // still the file `lock.yaml` names (another taker may have replaced it).
    let named = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io(e)),
    };
    let held = cur.metadata().map_err(io)?;
    if (named.dev(), named.ino()) != (held.dev(), held.ino()) {
        return Ok(false);
    }
    fs::rename(staged, path).map_err(io)?;
    Ok(true)
}

/// `flock(2)` on `file`'s descriptor.
fn flock(file: &File, op: libc::c_int) -> std::io::Result<()> {
    // SAFETY: the descriptor is owned by `file`, open for this whole call.
    if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Release the workdir lock — but only if THIS process holds it.
/// Mirrors Python's `release_lock`: removes the file while it still names
/// `std::process::id()`, then closes the descriptor (which drops the flock).
/// If a foreign pid is named (the file was replaced by hand), the file stays
/// — we don't yank someone else's lock. No-op when this process holds no
/// lock here (idempotent shutdown path).
pub fn release(workdir: &Path) -> KernelResult<()> {
    let path = workdir.join(super::persistence::LOCK_PATH);
    let held = {
        let mut held = HELD.lock().expect("lock registry poisoned");
        let Some(i) = held.iter().position(|(p, _)| *p == path) else {
            return Ok(());
        };
        held.remove(i).1
    };
    // python `release_lock`: `cur = _read_lock()` is `None` on ANY read/parse
    // failure, and `if cur and cur.get("pid") == os.getpid(): unlink()` — a
    // malformed file is left ALONE, never removed.
    let owned = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| parse_lock(&raw))
        .is_some_and(|lock| lock.pid == std::process::id());
    if owned {
        fs::remove_file(&path).map_err(|e| KernelError::LockIo { path, source: e })?;
    }
    drop(held);
    Ok(())
}

/// Is the workdir lock HELD — the attach probe? `Some(pid)`
/// while a live process holds the flock on `lock.yaml` (the pid read from the
/// file, for the message); `None` when nobody holds it — no file, or a file a
/// dead holder left, whatever pid it names (a reused pid is not a holder).
///
/// The decision is the flock, exactly as [`acquire`] judges it: a SHARED
/// non-blocking flock on its own descriptor, dropped before returning. It
/// conflicts with the holder's exclusive one; for the instant it is granted, a
/// racing [`acquire`] reads the file held and is refused — the probe never takes
/// a lock it keeps. This process's own lock reads as held.
pub fn held(workdir: &Path) -> KernelResult<Option<u32>> {
    let path = workdir.join(super::persistence::LOCK_PATH);
    let io = |e: std::io::Error| KernelError::LockIo {
        path: path.clone(),
        source: e,
    };
    let mut cur = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(e)),
    };
    match flock(&cur, libc::LOCK_SH | libc::LOCK_NB) {
        Ok(()) => Ok(None), // granted: nobody holds it; closing `cur` drops it
        Err(e) if e.kind() == ErrorKind::WouldBlock => {
            let mut raw = Vec::new();
            cur.read_to_end(&mut raw).map_err(io)?;
            let lock = std::str::from_utf8(&raw)
                .ok()
                .and_then(parse_lock)
                .ok_or_else(|| {
                    io(std::io::Error::new(
                        ErrorKind::InvalidData,
                        "lock.yaml malformed",
                    ))
                })?;
            Ok(Some(lock.pid))
        }
        Err(e) => Err(io(e)),
    }
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
