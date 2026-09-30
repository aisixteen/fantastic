//! Recursive `Agent` substrate + `Kernel` shared context.
//!
//! Stable surface: `send`, `emit`, `watch`, `create`/`delete`/`update`,
//! `reflect`, on-disk `.fantastic/` layout. One runtime is active per
//! workdir at a time, locked by `.fantastic/lock.yaml`. Agents whose
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
pub mod authz;
pub mod bootstrap;
pub mod bundle;
pub mod call_guard;
pub mod errors;
pub mod executions;
pub mod inbox;
pub mod jsonsafe;
pub mod kernel;
pub mod ledger;
pub mod lifecycle;
pub mod lock;
pub mod outcome;
pub mod paging;
pub mod peers;
pub mod persistence;
pub mod reflect;
pub mod refusal;
pub mod rows;
pub mod scalar;
pub mod send;
pub mod state;
pub mod storage;
pub mod tasks;
pub mod yaml_io;

#[doc(hidden)]
pub mod test_support;

pub use agent::{Agent, AgentId, AgentRecord, RESERVED_RECORD_KEYS};
pub use authz::{
    authorize_call, default_callable_by, default_can_call, Decision, FROM_ABOVE, PRIVILEGED_VERBS,
};
pub use bundle::{Bundle, BundleRegistry, Reply};
pub use errors::{KernelError, KernelResult};
pub use kernel::{Kernel, StateSubscriber, SubscriberToken, DEFAULT_INBOX_BOUND};
pub use outcome::{Item, Outcome, Status};
pub use paging::page;
pub use peers::{PeerCallFuture, PeerReply, PeerRoute};
pub use refusal::{bad, missing, not_here, refusal, refusal_hint, unknown_agent};
pub use rows::{Row, RowStore, RowTail};
pub use scalar::{coerce_scalar, py_strip};
pub use send::{
    current_origin, current_remote_caller, current_sender, with_operator, with_remote_caller,
    with_sender, CURRENT_ORIGIN, CURRENT_SENDER,
};
pub use state::{KernelState, CURRENT_VERSION};
pub use storage::StorageMode;
pub use tasks::{current_killed, current_task, TaskId, TaskTable, CURRENT_TASK};
