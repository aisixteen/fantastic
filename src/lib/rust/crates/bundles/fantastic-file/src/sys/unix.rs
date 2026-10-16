//! The POSIX side of [`crate::sys`].

use std::cmp::Ordering;
use std::fs::{File, OpenOptions, Permissions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::Private;

/// python's order of two `PosixPath`s sharing a parent: the name's code
/// points, i.e. its utf-8 bytes.
pub(crate) fn path_order(a: &Path, b: &Path) -> Ordering {
    a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes())
}

/// python `os.unlink` — `unlink(2)`; a symlink goes, never its target.
pub(crate) fn unlink(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)
}

/// `open(path, O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW, mode or 0o666)`, then
/// `fchmod(fd, mode)` when a mode is given (the umask may have narrowed it;
/// never widened).
pub(crate) fn create_new_nofollow(path: &Path, mode: Option<u32>) -> io::Result<File> {
    let f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(mode.unwrap_or(0o666))
        .open(path)?;
    if let Some(mode) = mode {
        f.set_permissions(Permissions::from_mode(mode))?;
    }
    Ok(f)
}

/// `open(path, O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC)`, judged by `fstat`
/// on that descriptor: a regular file, owned by `getuid()`, no group/other
/// bits. A symlink in the leaf's place fails the open with `ELOOP` and is
/// judged by `lstat` — never followed; a FIFO cannot block the open.
pub(crate) fn open_private(path: &Path) -> io::Result<Private> {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path);
    let f = match opened {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            // the symlink itself, named in the refusal — never read
            let st = std::fs::symlink_metadata(path)?;
            return Ok(Private::Not {
                mode: st.mode() & 0o7777,
                uid: st.uid(),
            });
        }
        Err(e) => return Err(e),
    };
    let st = f.metadata()?;
    let mode = st.mode() & 0o7777;
    if !st.file_type().is_file() || st.uid() != current_uid() || mode & 0o077 != 0 {
        return Ok(Private::Not {
            mode,
            uid: st.uid(),
        });
    }
    Ok(Private::File(f))
}

/// The real user id of this process.
fn current_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

// ── the private-file fixtures (`crate::test_support`) ───────────────────────

/// `chmod(path, mode)`.
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    std::fs::set_permissions(path, Permissions::from_mode(mode))
}

/// The permission bits of `path`, followed (`S_IMODE` of `stat`).
pub(crate) fn mode_of(path: &Path) -> io::Result<u32> {
    Ok(std::fs::metadata(path)?.mode() & 0o7777)
}

/// The uid a file this process creates is owned by, as [`open_private`]
/// reports it.
pub(crate) fn own_uid() -> u32 {
    current_uid()
}

/// `mkfifo(path, 0o600)` — a FIFO, which only POSIX has.
#[cfg(test)]
pub(crate) fn mkfifo(path: &Path) -> io::Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: a valid NUL-terminated path; mkfifo has no other preconditions.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
