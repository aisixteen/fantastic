//! The Unix side of [`crate::sys`]: owner-only file modes.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// The variable naming the user's home dir.
pub(crate) const USER_HOME: &str = "HOME";

/// Create the directory `d` (its parent exists), `0700` from the start.
pub(crate) fn create_private_dir(d: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(d)
}

/// Create the new file `p` for writing, `0600` from the start; refused if it exists.
pub(crate) fn create_private_file(p: &Path) -> io::Result<File> {
    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(p)
}

/// The permission bits of the file at `p`, links followed (`0o777` and below).
pub(crate) fn mode_of(p: &Path) -> io::Result<u32> {
    Ok(fs::metadata(p)?.permissions().mode() & 0o777)
}

/// The command that makes `p` private again, for a refusal's hint.
pub(crate) fn private_fix(p: &Path, dir: bool) -> String {
    format!("chmod {} {}", if dir { "700" } else { "600" }, p.display())
}

/// Set the permission bits of the file at `p` (tests build files of a given mode).
#[cfg(test)]
pub(crate) fn set_mode(p: &std::path::Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}
