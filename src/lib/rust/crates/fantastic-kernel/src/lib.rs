//! Recursive `Agent` substrate + `Kernel` shared context.
//!
//! Stable surface: `send`, `emit`, `watch`, `create`/`delete`/`update`,
//! `reflect`, on-disk state-folder layout. One runtime is active per
//! state folder at a time, locked by `<state>/lock.yaml`. Agents whose
//! bundle isn't installed in this runtime are skipped + logged at boot.
//!
//! ## Sub-phase A scope (current commit)
//!
//! - [`agent::Agent`] + [`agent::AgentId`] + [`agent::AgentRecord`]
//! - [`kernel::Kernel`] context (agents/inboxes/state subs/root)
//! - [`bundle::Bundle`] trait + [`bundle::BundleRegistry`]
//! - [`persistence`] load/persist with weak-load skip+log
//! - [`lock`] PID-keyed state-folder lock
//! - [`errors::KernelError`]
//!
//! Sub-phase B (next commit) adds the verb dispatch path (`send`,
//! `emit`, fanout, watch, cascade delete, reflect output) and the
//! `_current_sender` task-local.

#![deny(missing_docs)]

// the macros below name this crate by its own name, in it as in a dependent one
extern crate self as fantastic_kernel;

pub mod agent;
pub mod authz;
pub mod await_set;
pub mod bootstrap;
pub mod bundle;
pub mod call_guard;
pub mod declared;
pub mod docs;
pub mod errors;
pub mod executions;
pub mod grants;
pub mod inbox;
pub mod inherit;
pub mod inquiry;
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
pub mod secret;
pub mod send;
pub mod state;
pub mod storage;
mod sys;
pub mod tasks;
pub mod typing;
pub mod verbs;
pub mod yaml_io;

#[doc(hidden)]
pub mod test_support;

pub use agent::{Agent, AgentId, AgentRecord, RESERVED_RECORD_KEYS};
pub use authz::{authorize_call, default_callable_by, default_can_call, Decision, FROM_ABOVE};
pub use bundle::{Bundle, BundleError, BundleRegistry, Handler, Reply};
pub use errors::{KernelError, KernelResult};
pub use kernel::{
    Kernel, StateSubscriber, SubscriberToken, DEFAULT_INBOX_BOUND, SELF_PLACEHOLDER,
    STATE_PLACEHOLDER,
};
pub use outcome::{body_at, Blob, Item, Outcome, Status, Step};
pub use paging::page;
pub use peers::{PeerCallFuture, PeerEmitFuture, PeerReply, PeerRoute};
pub use refusal::{bad, error_text, missing, not_here, refusal, refusal_hint, unknown_agent};
pub use rows::{Row, RowStore, RowTail};
pub use secret::{current_secret, note_reply_secret};
pub use send::{current_origin, current_sender, Signal, ON_BATCH_LAUNCHED};
pub use state::KernelState;
pub use storage::StorageMode;
pub use tasks::{current_killed, current_task, spawn_inherit, TaskId, TaskTable, CURRENT_TASK};
pub use verbs::{DynRegistry, DynVerb, Parse, RecordDecl, Verb};
