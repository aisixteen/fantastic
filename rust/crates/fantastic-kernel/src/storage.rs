//! [`StorageMode`] — picks the *medium* for a kernel's state.
//!
//! Both modes hold the same [`crate::state::KernelState`]; the only
//! difference is whether the kernel auto-flushes it to disk on every
//! mutation.
//!
//! - [`StorageMode::Disk`] — auto-flush every state change to
//!   `<workdir>/.fantastic/state.json` (atomic write via tmp+rename).
//!   On boot, [`crate::bootstrap::bootstrap`] reads `state.json` if
//!   present; otherwise it starts virgin and writes on first
//!   mutation. The workdir lock (`lock.json`) lives in the same
//!   `.fantastic/` directory.
//! - [`StorageMode::InMemory`] — no filesystem I/O ever. The
//!   consumer extracts a snapshot on demand via
//!   [`crate::Kernel::save`] and restores via
//!   [`crate::Kernel::load`].

use std::path::{Path, PathBuf};

/// The state medium for a kernel. See module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageMode {
    /// Persist state to `<workdir>/.fantastic/state.json`. Lock
    /// acquisition happens here too (see
    /// [`crate::bootstrap::BootstrapOptions::acquire_lock`]).
    Disk(PathBuf),
    /// State lives only in process memory. No fs I/O. The kernel
    /// still answers `kernel.save()` (returns the snapshot) and
    /// `kernel.load(snapshot)` (restores), but never touches disk.
    InMemory,
}

impl StorageMode {
    /// True if this mode auto-flushes to disk.
    pub fn is_disk(&self) -> bool {
        matches!(self, Self::Disk(_))
    }

    /// True if this mode never touches the filesystem.
    pub fn is_in_memory(&self) -> bool {
        matches!(self, Self::InMemory)
    }

    /// The workdir root, if this mode has one. Returns `None` for
    /// [`Self::InMemory`].
    pub fn workdir(&self) -> Option<&Path> {
        match self {
            Self::Disk(p) => Some(p.as_path()),
            Self::InMemory => None,
        }
    }

    /// The full path to the `state.json` snapshot file for this mode,
    /// if it has one. `<workdir>/.fantastic/state.json` for
    /// [`Self::Disk`]; `None` for [`Self::InMemory`].
    pub fn state_file(&self) -> Option<PathBuf> {
        self.workdir()
            .map(|w| w.join(".fantastic").join("state.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_predicate() {
        let s = StorageMode::Disk(PathBuf::from("/tmp/foo"));
        assert!(s.is_disk());
        assert!(!s.is_in_memory());
        assert_eq!(s.workdir(), Some(Path::new("/tmp/foo")));
        assert_eq!(
            s.state_file(),
            Some(PathBuf::from("/tmp/foo/.fantastic/state.json")),
        );
    }

    #[test]
    fn in_memory_predicate() {
        let s = StorageMode::InMemory;
        assert!(!s.is_disk());
        assert!(s.is_in_memory());
        assert_eq!(s.workdir(), None);
        assert_eq!(s.state_file(), None);
    }
}
