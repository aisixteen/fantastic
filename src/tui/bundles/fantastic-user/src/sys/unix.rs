//! The Unix side of [`crate::sys`]: file modes.

use std::fs::Metadata;
use std::os::unix::fs::PermissionsExt;

/// Whether `m` carries any execute bit.
pub(crate) fn is_executable(m: &Metadata) -> bool {
    m.permissions().mode() & 0o111 != 0
}

/// Set the permission bits of the file at `p` (tests build files of a given mode).
#[cfg(test)]
pub(crate) fn set_mode(p: &std::path::Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}
