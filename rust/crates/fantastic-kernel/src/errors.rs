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

    /// `agent.json` exists but doesn't parse as JSON.
    #[error("corrupt agent.json at {path}: {source}")]
    CorruptRecord {
        /// The file we tried to read.
        path: PathBuf,
        /// Underlying JSON cause.
        #[source]
        source: serde_json::Error,
    },

    /// Another daemon holds the lock file in this workdir.
    /// Mirrors Python's friendlier message — name the pid AND tell
    /// the user how to recover (stop the daemon or route through the
    /// daemon's WS surface).
    #[error(
        "fantastic: another fantastic owns this dir (pid={pid}). Stop it, or forward your call over the WS bridge served by the running daemon."
    )]
    LockHeld {
        /// PID of the owning daemon.
        pid: u32,
    },

    /// Reading or writing `.fantastic/lock.json` failed.
    #[error("lock file at {path}: {source}")]
    LockIo {
        /// The lock file path.
        path: PathBuf,
        /// Underlying I/O cause.
        #[source]
        source: std::io::Error,
    },

    /// A delete was refused because the record carries `delete_lock: true`.
    /// The substrate surfaces this as a JSON reply, not an error, but
    /// callers that want the typed variant get it here.
    #[error("agent {0:?} carries delete_lock")]
    DeleteLocked(String),
}

/// Convenience alias.
pub type KernelResult<T> = Result<T, KernelError>;
