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
//! - `on_shutdown` fires during graceful kernel shutdown, and only it: the
//!   walk never runs `on_delete` in its place. A bundle that frees OS
//!   resources at shutdown declares it (usually by calling its own
//!   `on_delete`). Default: noop.
//! - `on_cancel` fires when a `cancel` kills a task of this agent, BEFORE the
//!   task is aborted — release the REAL resources that task holds (an OS
//!   subprocess, a PTY, a socket). Dropping a future frees none of them.

use crate::agent::AgentId;
use crate::kernel::Kernel;
use crate::tasks::TaskId;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
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
    /// Stable bundle name. Matches what's stored in agent.yaml's
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
    /// optional `id`, etc.); `blob` is the raw request body.
    ///
    /// The reply body is `Option<Vec<u8>>`, NOT `Vec<u8>`: `Some` means the
    /// verb RETURNED A CHUNK and `None` means it returned none, so a 0-byte
    /// chunk (`Some(vec![])`, an empty file read) stays distinguishable from
    /// `write_stream`'s bodiless status reply. Python has the distinction for
    /// free — `isinstance(reply["bytes"], bytes)` — and every wire leg branches
    /// on it (the REST octet decision, the WS binary-frame decision, the file
    /// proxy's "is this a SOURCE" probe), which is why an `is_empty()` test
    /// served an empty file as JSON and a denial as a 200.
    ///
    /// Default impl: a bundle with no binary surface gets its JSON `header`
    /// dispatched through [`Self::handle`] and the raw `blob` is DROPPED —
    /// never base64-encoded onto the payload. base64 on the wire is banned
    /// (pure streams): raw bytes ride the binary carrier only, and a bundle
    /// that actually wants them overrides this method
    /// (`file_bridge.read_stream`/`write_stream`,
    /// `exec_bridge.paste_image`). This mirrors the fallback the shipped
    /// overrides already use for their non-binary verbs (header→`handle`,
    /// blob unused). Returns NO reply body.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        _blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Vec<u8>>), BundleError> {
        let reply = self.handle(agent_id, &header, kernel).await?;
        Ok((reply, None))
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

    /// LAND THE PENDING WRITES, before the teardown walk and while every
    /// provider child is still alive (python's duck-typed
    /// `flush_pending(agent)` on the root's module). Only the ROOT loader
    /// implements it; every other bundle's default is a no-op.
    async fn flush_pending(
        &self,
        _agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        Ok(())
    }

    /// Pre-shutdown hook — the ONLY hook the shutdown walk calls (audit
    /// 2026-09-28, c9: there is no `on_delete` fallback). Default: noop.
    async fn on_shutdown(
        &self,
        _agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        Ok(())
    }

    /// Optional readme text seeded into the agent's dir on creation.
    /// Default: empty (no readme). Bundles override with
    /// `include_str!("readme.md")` to ship their description.
    fn readme(&self) -> Option<&'static str> {
        None
    }

    /// Optional keyed `docs/` topic tree seeded beside the readme (a `/` in a
    /// key is a nested tier). Default: empty. Python reads the bundle package's
    /// `docs/` dir via importlib; a rust bundle hands the table in.
    fn docs(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// Duck-typed ROOT-reflect contribution, merged by the substrate on the
    /// ROOT only (python `_agent.py`: `importlib.import_module(handler_module)
    /// .reflect_root_extra(target, full=full)`).
    ///
    /// The root's handler_module (the loader) contributes root-only fields the
    /// substrate cannot compute without coupling to bundle logic —
    /// `persistence: {provider}`, the instance `guid`, the doc-tree frame and
    /// the `self_extension` shape. A bundle with nothing to add returns `None`.
    fn reflect_root_extra(
        &self,
        _agent: &Arc<crate::agent::Agent>,
        _full: bool,
        _kernel: &Arc<Kernel>,
    ) -> Option<Map<String, Value>> {
        None
    }
}

/// Compile-time bundle registry. Maps `handler_module` strings (as
/// stored in agent.yaml) to the implementing bundle.
///
/// Populated by the binary's `main()` before kernel bootstrap. Cloned
/// `Arc<dyn Bundle>` references go into the kernel's lookup table.
///
/// TWO KINDS OF ENTRY, as python has two: an INSTALLED bundle (python's
/// `fantastic.bundles` entry point) dispatches AND is listed in the reflect
/// catalog; an UNLISTED one (python's importable module with no entry point —
/// the e2e scenario probes) dispatches only. The catalog tells a caller what it
/// may `create_agent` from, so a test fixture must not appear in it.
#[derive(Default)]
pub struct BundleRegistry {
    by_handler: HashMap<String, Arc<dyn Bundle>>,
    unlisted: HashSet<String>,
}

impl BundleRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an INSTALLED bundle under its `handler_module` key — it
    /// dispatches and is listed in the `bundles` catalog.
    ///
    /// The key is what appears in `agent.yaml` — e.g. `"file_bridge.tools"`
    /// for the file bundle. Bundle authors choose this string; the
    /// substrate doesn't enforce a naming scheme.
    pub fn register<B: Bundle + 'static>(&mut self, handler_module: &str, bundle: B) {
        self.unlisted.remove(handler_module);
        self.by_handler
            .insert(handler_module.to_string(), Arc::new(bundle));
    }

    /// Register a bundle that DISPATCHES but is not installable — python's
    /// importable module without a `fantastic.bundles` entry point (the e2e
    /// scenario probes). It never appears in the `bundles` catalog.
    pub fn register_unlisted<B: Bundle + 'static>(&mut self, handler_module: &str, bundle: B) {
        self.unlisted.insert(handler_module.to_string());
        self.by_handler
            .insert(handler_module.to_string(), Arc::new(bundle));
    }

    /// Look up a bundle by `handler_module` key. Returns `None` for
    /// unknown — caller decides whether that's a hard error (live
    /// dispatch) or a weak-load skip (rehydration).
    pub fn get(&self, handler_module: &str) -> Option<Arc<dyn Bundle>> {
        self.by_handler.get(handler_module).cloned()
    }

    /// The INSTALLED (handler_module, bundle) pairs — python's entry-point
    /// scan. The `reflect bundles=all|ids` catalog; unlisted bundles are
    /// skipped.
    pub fn catalog(&self) -> impl Iterator<Item = (&str, &Arc<dyn Bundle>)> {
        self.by_handler
            .iter()
            .filter(|(k, _)| !self.unlisted.contains(k.as_str()))
            .map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests;
