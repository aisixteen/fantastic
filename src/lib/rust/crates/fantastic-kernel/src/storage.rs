//! [`StorageMode`] — picks the *medium* for a kernel's state.
//!
//! Both modes hold the same [`crate::state::KernelState`]; the only
//! difference is whether the kernel auto-flushes it to disk on every
//! mutation.
//!
//! - [`StorageMode::Disk`] — auto-flush every state change to
//!   `<state_dir>/state.json` (atomic write via tmp+rename).
//!   On boot, [`crate::bootstrap::bootstrap`] reads `state.json` if
//!   present; otherwise it starts virgin and writes on first
//!   mutation. The lock (`lock.yaml`) lives in the same state folder.
//!   Boot is told BOTH folders and keeps exactly what it was given: the
//!   state folder (the kernel's own records) and the work folder (the base
//!   of every non-state path). Nothing is derived here; the one derivation
//!   of a default state folder is [`crate::bootstrap::state_dir_for`].
//! - [`StorageMode::InMemory`] — no filesystem I/O ever. The
//!   consumer extracts a snapshot on demand via
//!   [`crate::Kernel::save`] and restores via
//!   [`crate::Kernel::load`].

use std::path::{Path, PathBuf};

/// The state medium for a kernel. See module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageMode {
    /// Persist state under `state_dir`. Lock acquisition happens here too (see
    /// [`crate::bootstrap::BootstrapOptions::acquire_lock`]).
    Disk {
        /// The dir holding the kernel's own state: `lock.yaml`, the root
        /// `agent.yaml`, its `agents/` and its store.
        state_dir: PathBuf,
        /// The kernel's work folder: the base of every non-state disk path.
        work_dir: PathBuf,
    },
    /// State lives only in process memory. No fs I/O. The kernel
    /// still answers `kernel.save()` (returns the snapshot) and
    /// `kernel.load(snapshot)` (restores), but never touches disk.
    InMemory,
}

impl StorageMode {
    /// True if this mode auto-flushes to disk.
    pub fn is_disk(&self) -> bool {
        matches!(self, Self::Disk { .. })
    }

    /// True if this mode never touches the filesystem.
    pub fn is_in_memory(&self) -> bool {
        matches!(self, Self::InMemory)
    }

    /// The work folder, if this mode has one. Returns `None` for
    /// [`Self::InMemory`].
    pub fn work_dir(&self) -> Option<&Path> {
        match self {
            Self::Disk { work_dir, .. } => Some(work_dir.as_path()),
            Self::InMemory => None,
        }
    }

    /// The kernel's state dir, if this mode has one. Returns `None` for
    /// [`Self::InMemory`].
    pub fn state_dir(&self) -> Option<&Path> {
        match self {
            Self::Disk { state_dir, .. } => Some(state_dir.as_path()),
            Self::InMemory => None,
        }
    }

    /// The full path to the `state.json` snapshot file for this mode,
    /// if it has one. `<state_dir>/state.json` for [`Self::Disk`]; `None`
    /// for [`Self::InMemory`].
    pub fn state_file(&self) -> Option<PathBuf> {
        self.state_dir().map(|d| d.join("state.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_predicate() {
        let s = StorageMode::Disk {
            state_dir: PathBuf::from("/tmp/foo/.fantastic"),
            work_dir: PathBuf::from("/tmp/foo"),
        };
        assert!(s.is_disk());
        assert!(!s.is_in_memory());
        assert_eq!(s.work_dir(), Some(Path::new("/tmp/foo")));
        assert_eq!(s.state_dir(), Some(Path::new("/tmp/foo/.fantastic")));
        assert_eq!(
            s.state_file(),
            Some(PathBuf::from("/tmp/foo/.fantastic/state.json")),
        );
    }

    #[test]
    fn disk_with_its_own_state_dir() {
        let s = StorageMode::Disk {
            state_dir: PathBuf::from("/tmp/home/app"),
            work_dir: PathBuf::from("/tmp/work"),
        };
        assert_eq!(s.work_dir(), Some(Path::new("/tmp/work")));
        assert_eq!(s.state_dir(), Some(Path::new("/tmp/home/app")));
        assert_eq!(
            s.state_file(),
            Some(PathBuf::from("/tmp/home/app/state.json")),
        );
    }

    #[test]
    fn in_memory_predicate() {
        let s = StorageMode::InMemory;
        assert!(!s.is_disk());
        assert!(s.is_in_memory());
        assert_eq!(s.work_dir(), None);
        assert_eq!(s.state_dir(), None);
        assert_eq!(s.state_file(), None);
    }
}
