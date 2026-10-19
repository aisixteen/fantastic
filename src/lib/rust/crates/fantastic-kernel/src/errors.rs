//! Error types surfaced from substrate operations.
//!
//! Kept narrow on purpose — most kernel verbs return `Result<Reply,
//! KernelError>` where `Reply` is JSON. Domain-specific errors
//! (file path escape, bundle-specific 4xx) surface inside the JSON
//! reply, not as `KernelError`. This type is for substrate-level
//! failures: bad agent id, broken persistence, lock contention.

use std::path::PathBuf;
use thiserror::Error;

/// Substrate-level failure. Domain errors live in the JSON reply.
#[derive(Debug, Error)]
pub enum KernelError {
    /// The requested agent id isn't present in the live tree.
    #[error("no agent {0:?}")]
    NoAgent(String),

    /// `create_agent` was given an id already in use.
    #[error("agent {0:?} already exists")]
    DuplicateId(String),

    /// Disk write failed while persisting an agent record.
    #[error("persistence at {path}: {source}")]
    Persistence {
        /// The file we were writing.
        path: PathBuf,
        /// Underlying I/O cause.
        #[source]
        source: std::io::Error,
    },

    /// `agent.yaml` exists but doesn't parse as a YAML mapping.
    #[error("corrupt agent.yaml at {path}: {reason}")]
    CorruptRecord {
        /// The file we tried to read.
        path: PathBuf,
        /// What the parse said.
        reason: String,
    },

    /// Another daemon holds the lock file in this workdir.
    ///
    /// Python's exact three lines (`P/kernel/_lock.py:52-64`) — name the
    /// pid and tell the operator how to recover (kill it, or remove a stale
    /// lock). `RCLI/src/main.rs` prints this as `[kernel] {e}` and exits 1,
    /// NOT through the generic `fantastic_kernel: {e}` wrapper every other
    /// error takes.
    #[error(
        "another fantastic owns this dir: pid={pid}\n  -> kill it:  kill {pid}\n  -> or, if stale, remove the lock:  rm .fantastic/lock.yaml"
    )]
    LockHeld {
        /// PID of the owning daemon.
        pid: u32,
    },

    /// Reading or writing `.fantastic/lock.yaml` failed.
    #[error("lock file at {path}: {source}")]
    LockIo {
        /// The lock file path.
        path: PathBuf,
        /// Underlying I/O cause.
        #[source]
        source: std::io::Error,
    },

    /// A disk bootstrap was asked to keep the kernel's state in the
    /// fantastic home ([`crate::bootstrap::refuse_home_as_state_dir`]).
    #[error(
        "{}: its .fantastic is the fantastic home ({}) — run fantastic in a project folder",
        dir.display(),
        home.display()
    )]
    HomeAsStateDir {
        /// The workdir the kernel was booted in.
        dir: PathBuf,
        /// The fantastic home.
        home: PathBuf,
    },

    /// A delete was refused because the record carries `delete_lock: true`.
    /// The substrate surfaces this as a JSON reply, not an error, but
    /// callers that want the typed variant get it here.
    #[error("agent {0:?} carries delete_lock")]
    DeleteLocked(String),

    /// A [`crate::Kernel::load`] or [`crate::Kernel::load_json`] call
    /// received a snapshot that couldn't be parsed, was missing a
    /// root, had a duplicate id, or had a schema version this kernel
    /// doesn't understand.
    #[error("invalid kernel snapshot: {0}")]
    InvalidSnapshot(String),
}

/// Convenience alias.
pub type KernelResult<T> = Result<T, KernelError>;
