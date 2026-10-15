//! Unix (macOS, Linux): libc and `std::os::unix`.

use std::fs::{File, Metadata, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;

/// `kill(pid, 0)`: `true` when the process exists, ours or not.
pub(crate) fn pid_alive(pid: u32) -> bool {
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

/// The permission bits a file created through `opts` gets.
pub(crate) fn set_create_mode(opts: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    opts.mode(mode)
}

/// Do two metadata describe the same file — same device, same inode?
pub(crate) fn same_file(a: &Metadata, b: &Metadata) -> bool {
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

/// Blocking exclusive `flock(LOCK_EX)` on `file`'s descriptor.
pub(crate) fn flock_exclusive(file: &File) -> std::io::Result<()> {
    flock(file, libc::LOCK_EX)
}

/// Non-blocking exclusive `flock(LOCK_EX | LOCK_NB)`; held elsewhere is
/// [`std::io::ErrorKind::WouldBlock`].
pub(crate) fn try_flock_exclusive(file: &File) -> std::io::Result<()> {
    flock(file, libc::LOCK_EX | libc::LOCK_NB)
}

/// Non-blocking shared `flock(LOCK_SH | LOCK_NB)`; held exclusively elsewhere
/// is [`std::io::ErrorKind::WouldBlock`].
pub(crate) fn try_flock_shared(file: &File) -> std::io::Result<()> {
    flock(file, libc::LOCK_SH | libc::LOCK_NB)
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

/// Fill `buf` from the OS CSPRNG: `getrandom` on Linux (the libc crate has no
/// `getentropy` for musl), `getentropy` elsewhere (at most 256 bytes a call).
/// A failure is FATAL: there is no second source.
pub(crate) fn fill_random(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(256) {
        // SAFETY: `chunk` is a valid writable buffer of exactly its length.
        #[cfg(target_os = "linux")]
        let ok = unsafe { libc::getrandom(chunk.as_mut_ptr().cast(), chunk.len(), 0) }
            == chunk.len() as isize;
        #[cfg(not(target_os = "linux"))]
        let ok = unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) } == 0;
        assert!(ok, "the OS randomness call failed: no randomness source");
    }
}
