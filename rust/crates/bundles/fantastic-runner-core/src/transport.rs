//! The [`Transport`] seam — the only thing a runner bundle implements.
//!
//! Both runners share the *lifecycle dispatch* (which verbs exist, that
//! `boot` is a no-op, that `restart` is `stop` then `start`, the
//! unknown-verb error). They differ entirely in *how each verb's work
//! is carried out and what concrete reply it produces*:
//!
//! - local — subprocess of `fantastic` + filesystem `lock.json` +
//!   OS signals; replies carry `pid` / `port`.
//! - ssh — `ssh` exec + `ssh -L` tunnel; replies carry `remote_pid` /
//!   `tunnel_pid` / `tunnel_alive`.
//!
//! Because the reply *shapes* differ (and must stay byte-identical to
//! the pre-refactor wire), the transport owns each verb's reply body.
//! [`crate::core::RunnerCore`] owns the dispatch skeleton that routes
//! verbs to these methods.
//!
//! A transport is built fresh per call from the agent's record (the
//! `Transport(record)` shape mirrored from the Python runner_core),
//! so impls are cheap to construct and hold no long-lived borrows.

use async_trait::async_trait;
use serde_json::Value;

/// One runner transport. Built per call from the agent record; carries
/// whatever the impl needs (agent id, snapshotted meta, a handle to the
/// process-global state map).
#[async_trait]
pub trait Transport: Send + Sync {
    /// `reflect` reply — identity + every record field + live status.
    async fn reflect(&self) -> Value;

    /// `start` reply — bring the project up (idempotent) and report the
    /// resulting pid/port (local) or remote_pid/tunnel_pid (ssh).
    async fn start(&self) -> Value;

    /// `stop` reply — tear the project down (idempotent).
    async fn stop(&self) -> Value;

    /// `status` reply — liveness snapshot. No side effects.
    async fn status(&self) -> Value;

    /// `get_webapp` reply — the canvas-facing UI descriptor
    /// (`{url, default_width, default_height, title}`) or an error.
    async fn get_webapp(&self) -> Value;
}
