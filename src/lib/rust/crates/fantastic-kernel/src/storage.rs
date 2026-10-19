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
//!   mutation. The workdir lock (`lock.yaml`) lives in the same
//!   `.fantastic/` directory. The state dir is the HOST'S choice:
//!   [`StorageMode::disk`] keeps `<workdir>/.fantastic`, and a host that
//!   keeps its state elsewhere names it ([`StorageMode::Disk::state_dir`]).
//!   The on-disk format is the same wherever it lives.
//! - [`StorageMode::InMemory`] — no filesystem I/O ever. The
//!   consumer extracts a snapshot on demand via
//!   [`crate::Kernel::save`] and restores via
//!   [`crate::Kernel::load`].

use std::path::{Path, PathBuf};

/// The state medium for a kernel. See module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageMode {
    /// Persist state under `state_dir` (`<workdir>/.fantastic` for every
    /// project host, [`Self::disk`]). Lock acquisition happens here too (see
    /// [`crate::bootstrap::BootstrapOptions::acquire_lock`]).
    Disk {
        /// The kernel's running dir: the clamp base of its disk paths.
        workdir: PathBuf,
        /// The dir holding the kernel's own state: `lock.yaml`, the root
        /// `agent.yaml`, its `agents/` and its store.
        state_dir: PathBuf,
    },
    /// State lives only in process memory. No fs I/O. The kernel
    /// still answers `kernel.save()` (returns the snapshot) and
    /// `kernel.load(snapshot)` (restores), but never touches disk.
    InMemory,
}

impl StorageMode {
    /// Disk mode with the project layout: `state_dir = <workdir>/.fantastic`.
    pub fn disk(workdir: impl Into<PathBuf>) -> Self {
        let workdir = workdir.into();
        let state_dir = workdir.join(".fantastic");
        Self::Disk { workdir, state_dir }
    }

    /// True if this mode auto-flushes to disk.
    pub fn is_disk(&self) -> bool {
        matches!(self, Self::Disk { .. })
    }

    /// True if this mode never touches the filesystem.
    pub fn is_in_memory(&self) -> bool {
        matches!(self, Self::InMemory)
    }

    /// The workdir root, if this mode has one. Returns `None` for
    /// [`Self::InMemory`].
    pub fn workdir(&self) -> Option<&Path> {
        match self {
            Self::Disk { workdir, .. } => Some(workdir.as_path()),
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
        let s = StorageMode::disk(PathBuf::from("/tmp/foo"));
        assert!(s.is_disk());
        assert!(!s.is_in_memory());
        assert_eq!(s.workdir(), Some(Path::new("/tmp/foo")));
        assert_eq!(
            s.state_file(),
            Some(PathBuf::from("/tmp/foo/.fantastic/state.json")),
        );
    }

    #[test]
    fn disk_with_its_own_state_dir() {
        let s = StorageMode::Disk {
            workdir: PathBuf::from("/tmp/home/app"),
            state_dir: PathBuf::from("/tmp/home/app"),
        };
        assert_eq!(s.workdir(), Some(Path::new("/tmp/home/app")));
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
        assert_eq!(s.workdir(), None);
        assert_eq!(s.state_dir(), None);
        assert_eq!(s.state_file(), None);
    }
}
