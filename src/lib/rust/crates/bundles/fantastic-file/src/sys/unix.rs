//! The POSIX side of [`crate::sys`].

use std::ffi::OsStr;
use std::fs::{File, Metadata, OpenOptions, Permissions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// The raw bytes of an OS string.
pub(crate) fn os_bytes(s: &OsStr) -> &[u8] {
    s.as_bytes()
}

/// `open(path, O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW, mode or 0o666)`.
pub(crate) fn create_new_nofollow(path: &Path, mode: Option<u32>) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(mode.unwrap_or(0o666))
        .open(path)
}

/// `fchmod(fd, mode)`.
pub(crate) fn set_mode(f: &File, mode: u32) -> io::Result<()> {
    f.set_permissions(Permissions::from_mode(mode))
}

/// `open(path, O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC)` — a symlink in the
/// leaf's place fails with `ELOOP` ([`is_symlink_refusal`]), a FIFO cannot
/// block the open.
pub(crate) fn open_private(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

/// Whether a no-follow open failed because the leaf is a symlink (`ELOOP`).
pub(crate) fn is_symlink_refusal(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ELOOP)
}

/// The permission bits of `st` (`S_IMODE`).
pub(crate) fn mode_bits(st: &Metadata) -> u32 {
    st.mode() & 0o7777
}

/// The owner of `st`.
pub(crate) fn owner_uid(st: &Metadata) -> u32 {
    st.uid()
}

/// The real user id of this process.
pub(crate) fn current_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}
