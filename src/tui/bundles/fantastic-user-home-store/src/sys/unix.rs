//! The Unix side of [`crate::sys`]: owner-only file modes.

use std::fs::{self, File, Metadata};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Create the directory `d` (its parent exists), `0700` from the start.
pub(crate) fn create_private_dir(d: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(d)
}

/// Create the new file `p` for writing, `0600` from the start; refused if it exists.
pub(crate) fn create_private_file(p: &Path) -> io::Result<File> {
    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(p)
}

/// The permission bits of `m` (`0o777` and below).
pub(crate) fn mode_bits(m: &Metadata) -> u32 {
    m.permissions().mode() & 0o777
}

/// Set the permission bits of the file at `p` (tests build files of a given mode).
#[cfg(test)]
pub(crate) fn set_mode(p: &std::path::Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}
