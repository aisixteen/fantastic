//! Test-only support — NOT part of the stable API (`#[doc(hidden)]`).
//!
//! The private-file fixtures, on every OS: a test of [`crate::fs::read_private`]
//! (or of a caller of it, as `credential_store`'s file backend) sets a file's
//! mode, reads it back and names this process's uid through here, and the OS
//! half lives in the crate's `sys` — on Windows a "mode" is the file's DACL.

use std::path::Path;

/// `chmod(path, mode)`. On Windows a mode with no group/other bits sets the
/// private DACL [`crate::fs::write_bytes`] gives a `0o600` file; any other mode
/// also grants Everyone read (read back as `0o644`).
///
/// # Errors
/// The OS error.
pub fn set_mode(path: impl AsRef<Path>, mode: u32) -> std::io::Result<()> {
    crate::sys::set_mode(path.as_ref(), mode)
}

/// The permission bits of `path` (symlinks followed), as
/// [`crate::fs::NotPrivate::mode`] reports them.
///
/// # Errors
/// The OS error.
pub fn mode_of(path: impl AsRef<Path>) -> std::io::Result<u32> {
    crate::sys::mode_of(path.as_ref())
}

/// The uid a file this process creates is owned by, as
/// [`crate::fs::NotPrivate::uid`] reports it.
#[must_use]
pub fn own_uid() -> u32 {
    crate::sys::own_uid()
}
