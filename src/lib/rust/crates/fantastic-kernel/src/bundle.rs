//! Bundle trait + compile-time registry.
//!
//! Bundles aren't dynamically loaded — every Rust bundle is a crate
//! linked into the binary at compile time. The CLI crate links the
//! default set; an embedding build may link a platform-appropriate
//! subset. Adding a bundle to a build means adding its crate to the workspace
//! and calling `reg.register(...)` in the relevant
//! `register_default_bundles()` site.
//!
//! ## Contract
//!
//! Every bundle implements [`Bundle`]. The substrate calls `handle`
//! for any verb the Agent's system-verb table doesn't claim. Bundle
//! authors return `None` to mean "no reply" (fire-and-forget — the
//! Python equivalent is returning `None` from the handler).
//!
//! Lifecycle hooks:
//! - `on_delete` fires depth-first during cascade-delete BEFORE the
//!   agent unregisters from `kernel.agents`. Tear down process-memory
//!   state (open files, in-flight tasks). Substrate then rmtrees the
//!   agent's directory unless `ephemeral` is set.
//! - `on_shutdown` fires during graceful kernel shutdown. The default
//!   delegates to `on_delete`. Override if "tearing down, will
//!   restart" should behave differently from "gone forever".
//! - `on_cancel` fires when a `cancel` kills a task of this agent, BEFORE the
//!   task is aborted — release the REAL resources that task holds (an OS
//!   subprocess, a PTY, a socket). Dropping a future frees none of them.

use crate::agent::AgentId;
use crate::kernel::Kernel;
use crate::tasks::TaskId;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// Reply envelope. `None` = no reply (fire-and-forget); `Some(v)` =
/// caller receives `v` as the `kernel.send` return value.
pub type Reply = Option<Value>;

/// Boxed error returned from bundle handler logic. Aliased so the
/// trait signature stays readable.
pub type BundleError = Box<dyn std::error::Error + Send + Sync>;

/// Bundle trait. Every Rust bundle implements this.
#[async_trait]
pub trait Bundle: Send + Sync {
    /// Stable bundle name. Matches what's stored in agent.json's
    /// `handler_module` (e.g. `"file_bridge.tools"`, `"web.tools"`).
    fn name(&self) -> &str;

    /// Dispatch a verb. `payload["type"]` names the verb. Substrate
    /// guarantees `payload` is an `Object` shape.
    ///
    /// `kernel` is passed so the handler can `kernel.send(...)` to
    /// other agents or read shared state. Avoids storing back-refs
    /// on each `Agent` (would create Arc cycles).
    ///
    /// Return `Ok(Some(value))` for normal replies, `Ok(None)` for
    /// fire-and-forget verbs. `Err` surfaces a substrate-level
    /// failure; domain errors should be returned as
    /// `Ok(Some(json!({"error": "..."})))`.
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError>;

    /// Refuse an `update_agent` patch this bundle cannot honour. Called by the
    /// substrate BEFORE the record changes; `Some(reason)` REFUSES the patch.
    ///
    /// A record carries meta only its bundle understands, so the substrate cannot
    /// judge it — but the bundle can. Default: accept everything (most bundles have
    /// nothing to check). Mirrors python's duck-typed `validate_meta(meta, record)`.
    ///
    /// Exists because `update_agent` used to accept anything: an agent set
    /// `ingress_rule="password:kernel-internal-transcripts"`, got `{"updated":
    /// true}` back, and believed a security control was in place that no resolver
    /// implements. Accepting a value you cannot honour AND reporting success is the
    /// worst of both.
    ///
    /// `record` is the agent's CURRENT meta (empty at create), so a bundle
    /// validates the state the write WILL produce, not the patch in isolation —
    /// py-canonical: a cross-field rule (exec_bridge's `interpreter` against
    /// `interpreters`) is unenforceable from the patch alone, and checking the
    /// patch falsely refused `update_agent id=x interpreter=<already allowed>`.
    /// Called at BOTH create and update; create used to bypass it entirely,
    /// which let a record be born with meta its own bundle refuses.
    fn validate_meta(
        &self,
        _meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        None
    }

    /// Dispatch a binary-framed verb. The binary channel is **symmetric**:
    /// a request carries `(header, blob)` and the reply carries
    /// `(Reply, body)` — raw bytes flow in BOTH directions, never base64
    /// (mirrors Python's `read_stream`/`write_stream`, where `bytes` is a
    /// native `bytes` value in the dict). `header` is the JSON header
    /// object from the framed WS message (carries `target`, `type`,
    /// optional `id`, etc.); `blob` is the raw request body. The returned
    /// `Vec<u8>` is the raw reply body (empty when the verb returns no
    /// bytes, e.g. `write_stream`'s status reply).
    ///
    /// Default impl: a bundle with no binary surface gets its JSON `header`
    /// dispatched through [`Self::handle`] and the raw `blob` is DROPPED —
    /// never base64-encoded onto the payload. base64 on the wire is banned
    /// (pure streams): raw bytes ride the binary carrier only, and a bundle
    /// that actually wants them overrides this method
    /// (`file_bridge.read_stream`/`write_stream`,
    /// `terminal_backend.paste_image`). This mirrors the fallback the shipped
    /// overrides already use for their non-binary verbs (header→`handle`,
    /// blob unused). Returns an empty reply body.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        _blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Vec<u8>), BundleError> {
        let reply = self.handle(agent_id, &header, kernel).await?;
        Ok((reply, Vec::new()))
    }

    /// Pre-detach hook. Default: noop.
    async fn on_delete(
        &self,
        _agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        Ok(())
    }

    /// Cancel hook — the SIGTERM half of the `cancel` system verb (the abort is
    /// the SIGKILL half). Invoked once per agent in a cancelled subtree, BEFORE
    /// any task is stopped, with the id of the task being killed.
    ///
    /// Dropping a future frees NO real resource: it unwinds `async` frames and
    /// nothing else. A bundle holding an OS subprocess, a PTY, or a socket FOR
    /// THAT TASK must release it here — this is where the orphaned-subprocess
    /// class of bug is fixed. Pure-async bundles need nothing (default: noop,
    /// same optionality as [`Self::on_delete`]).
    ///
    /// A hook that errors is logged and the kill still happens: cancel is not
    /// refusable.
    async fn on_cancel(
        &self,
        _agent_id: &AgentId,
        _task_id: &TaskId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        Ok(())
    }

    /// Pre-shutdown hook. Default: delegate to on_delete.
    async fn on_shutdown(
        &self,
        agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        self.on_delete(agent_id, kernel).await
    }

    /// Optional readme text seeded into the agent's dir on creation.
    /// Default: empty (no readme). Bundles override with
    /// `include_str!("readme.md")` to ship their description.
    fn readme(&self) -> Option<&'static str> {
        None
    }
}

/// Compile-time bundle registry. Maps `handler_module` strings (as
/// stored in agent.json) to the implementing bundle.
///
/// Populated by the binary's `main()` before kernel bootstrap. Cloned
/// `Arc<dyn Bundle>` references go into the kernel's lookup table.
#[derive(Default)]
pub struct BundleRegistry {
    by_handler: HashMap<String, Arc<dyn Bundle>>,
}

impl BundleRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a bundle under its `handler_module` key.
    ///
    /// The key is what appears in `agent.json` — e.g. `"file_bridge.tools"`
    /// for the file bundle. Bundle authors choose this string; the
    /// substrate doesn't enforce a naming scheme.
    pub fn register<B: Bundle + 'static>(&mut self, handler_module: &str, bundle: B) {
        self.by_handler
            .insert(handler_module.to_string(), Arc::new(bundle));
    }

    /// Look up a bundle by `handler_module` key. Returns `None` for
    /// unknown — caller decides whether that's a hard error (live
    /// dispatch) or a weak-load skip (rehydration).
    pub fn get(&self, handler_module: &str) -> Option<Arc<dyn Bundle>> {
        self.by_handler.get(handler_module).cloned()
    }

    /// Iterate registered (handler_module, bundle) pairs. Used by the
    /// `reflect bundles=all|ids` flag to enumerate the bundle catalog.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Bundle>)> {
        self.by_handler.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests;
