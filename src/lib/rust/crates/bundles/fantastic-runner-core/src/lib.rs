//! fantastic-runner-core — shared `fantastic` lifecycle machinery behind
//! a [`Transport`] seam. The local + ssh runner backends supply only
//! their `Transport` impl + a thin `Bundle` that dispatches verbs
//! through [`RunnerCore`].
//!
//! Mirrors the Python `runner_core` dedup (shared lifecycle + Transport
//! seam) and the sibling `fantastic-ai-core` crate's layout.
//!
//! Two runners, one lifecycle:
//!
//! - SHARED ([`core`] + [`transport`] + [`meta`] + [`state`]): the verb
//!   dispatch skeleton (reflect/boot/start/stop/restart/status/
//!   get_webapp), the agent-record snapshot helpers, and the generic
//!   per-agent process-state map.
//! - PER-TRANSPORT (in each runner crate): how each verb does its work
//!   and the concrete reply it returns. local = subprocess + filesystem
//!   lock + OS signals; ssh = ssh exec + `ssh -L` tunnel.
//!
//! ## Runner contract (canonical reference)
//!
//! Every runner bundle implements the same lifecycle verbs so the
//! canvas can drive a local or remote project identically.
//!
//! ### Verbs (caller → runner, via `kernel.send`)
//!
//! - `reflect` — identity + every record field + live status. No args.
//! - `boot` — no-op (`Value::Null`); runners do NOT auto-start.
//! - `start` — bring the project up (idempotent), poll until the
//!   `lock.json` confirms the daemon is live.
//! - `stop` / `shutdown` — tear the project down (idempotent).
//! - `restart` — `stop` then `start`.
//! - `status` — liveness snapshot, no side effects.
//! - `get_webapp` — canvas-facing UI descriptor `{url, default_width,
//!   default_height, title}`.
//!
//! Reply *shapes* differ per transport (local carries `pid`/`port`,
//! ssh carries `remote_pid`/`tunnel_pid`/`tunnel_alive`) and are owned
//! by the transport — see each runner crate.

#![deny(missing_docs)]

pub mod core;
pub mod meta;
pub mod state;
pub mod transport;

pub use core::RunnerCore;
pub use meta::{meta_str, meta_u16, snapshot_meta};
pub use state::RunnerMap;
pub use transport::Transport;

#[cfg(test)]
mod tests;
