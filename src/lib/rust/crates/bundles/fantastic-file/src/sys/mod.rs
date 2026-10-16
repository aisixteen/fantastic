//! sys — the OS boundary of the clamped disk surface ([`crate::fs`]).
//!
//! Everything [`crate::fs`] needs beyond portable `std::fs`: the order python
//! sorts a listing's paths in, python's `os.unlink`, the exclusive no-follow
//! create with a permission mode, and the private-file open — ONE open that
//! never follows a link in the leaf's place, judged on its own handle. Plus the
//! private-file fixtures [`crate::test_support`] hands the tests (a mode set, a
//! mode read, this process's uid).

use std::fs::File;

/// What [`open_private`] found at a leaf.
pub(crate) enum Private {
    /// A private file — a regular file owned by this user that nobody else may
    /// access — open for reading.
    File(File),
    /// Anything else, never read: its permission bits and its owner.
    Not { mode: u32, uid: u32 },
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
