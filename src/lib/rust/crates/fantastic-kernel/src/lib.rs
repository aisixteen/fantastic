//! Recursive `Agent` substrate + `Kernel` shared context.
//!
//! Stable surface: `send`, `emit`, `watch`, `create`/`delete`/`update`,
//! `reflect`, on-disk `.fantastic/` layout. One runtime is active per
//! workdir at a time, locked by `.fantastic/lock.json`. Agents whose
//! bundle isn't installed in this runtime are skipped + logged at boot.
//!
//! ## Sub-phase A scope (current commit)
//!
//! - [`agent::Agent`] + [`agent::AgentId`] + [`agent::AgentRecord`]
//! - [`kernel::Kernel`] context (agents/inboxes/state subs/root)
//! - [`bundle::Bundle`] trait + [`bundle::BundleRegistry`]
//! - [`persistence`] load/persist with weak-load skip+log
//! - [`lock`] PID-keyed workdir lock
//! - [`errors::KernelError`]
//!
//! Sub-phase B (next commit) adds the verb dispatch path (`send`,
//! `emit`, fanout, watch, cascade delete, reflect output) and the
//! `_current_sender` task-local.

#![deny(missing_docs)]

pub mod agent;
pub mod bootstrap;
pub mod bundle;
pub mod errors;
pub mod kernel;
pub mod lifecycle;
pub mod lock;
pub mod persistence;
pub mod reflect;
pub mod send;
pub mod state;
pub mod storage;

#[doc(hidden)]
pub mod test_support;

pub use agent::{Agent, AgentId, AgentRecord};
pub use bundle::{Bundle, BundleRegistry, Reply};
pub use errors::{KernelError, KernelResult};
pub use kernel::{Kernel, StateSubscriber, SubscriberToken, DEFAULT_INBOX_BOUND};
pub use send::{current_sender, with_sender, CURRENT_SENDER};
pub use state::{KernelState, CURRENT_VERSION};
pub use storage::StorageMode;
