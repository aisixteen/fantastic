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
//! Every bundle implements [`Bundle`]. The substrate calls
//! [`Bundle::handle_binary`] — whose default is `handle` — for any verb the
//! Agent's system-verb table doesn't claim; a bundle whose replies never hold
//! raw bytes implements `handle` alone. Bundle authors return `None` to mean
//! "no reply" (fire-and-forget — the Python equivalent is returning `None`
//! from the handler).
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

    /// Dispatch a verb whose reply is JSON alone — the default
    /// [`Self::handle_binary`] calls it. `payload["type"]` names the verb.
    /// Substrate guarantees `payload` is an `Object` shape.
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

    /// What this bundle KIND's record takes — `{fields:[{name, type, required,
    /// enum?, default?, hint}]}` — readable before any agent of it exists (lib
    /// request #3, 2026-09-30); a key is a `credentials` field (`{store, ref}`),
    /// never an environment variable. The substrate attaches it to this
    /// bundle's entry in the `reflect bundles=all` catalog, and nowhere else.
    /// Python's duck-typed module-level `RECORD_SCHEMA`.
    ///
    /// ONE TRUTH: a bundle that publishes a schema enforces the same one in
    /// [`Self::validate_meta`] (the AI backends build both from
    /// `fantastic_ai_core::schema`). Default: none — most bundles publish no
    /// record contract.
    fn record_schema(&self) -> Option<Value> {
        None
    }

    /// HOW TO CALL this bundle's agents — `{verb: doc}` in the bundle's order,
    /// python's module-level `VERBS` docstrings. Served by the kernel's `docs`
    /// verb (oneliners: each doc's signature; detailed: the whole doc) and read
    /// by [`crate::kernel::Kernel::advertises`]. Never on a reflect (owner,
    /// 2026-10-05). Default: no verbs.
    fn verb_docs(&self) -> Value {
        Value::Object(serde_json::Map::new())
    }

    /// THIS AGENT'S verb docs, when its bundle serves different verbs per agent
    /// (an MCP server's tools) — python's duck-typed `instance_verbs(agent)`.
    /// `docs of=<agent id>` reads it before [`Self::verb_docs`]; `None` keeps the
    /// bundle's (owner, 2026-10-08). Default: none.
    fn instance_verb_docs(&self, _agent: &crate::agent::Agent) -> Option<Value> {
        None
    }

    /// The rest of the DETAILED docs — python's duck-typed module-level `EMITS`,
    /// `CANCEL` and `NOTES`, served by `docs` (tier detailed, no `verb`).
    fn emits_doc(&self) -> Option<Value> {
        None
    }
    /// How a caller stops this bundle's work (python `CANCEL`). Default: none.
    fn cancel_doc(&self) -> Option<Value> {
        None
    }
    /// Bundle notes for the detailed tier (python `NOTES`). Default: none.
    fn notes_doc(&self) -> Option<Value> {
        None
    }

    /// THE DISPATCH — the kernel calls this for EVERY call, a request body or
    /// not; python's one `handle`, whose reply dict may hold `bytes` at any key.
    /// Raw bytes flow in BOTH directions, never base64. `header` is the payload
    /// with NOTHING at the request body's path (python holds the bytes there);
    /// `body` is the raw request body AT ITS PATH ([`crate::outcome::Blob`]) — a
    /// handler reads ITS OWN key with [`crate::outcome::Blob::at`], exactly as
    /// python's reads `payload.get("data")` / `payload.get("bytes")`, so a body
    /// placed under another key is not its body. `None` = the call carried no
    /// request body (a plain text call: `read_stream`, `events`, …).
    ///
    /// The reply body is an `Option<Blob>` — the chunk AND ITS SLOT in the
    /// reply, as python's reply dict holds a `bytes` value at a key: a
    /// `read_stream` chunk is `Blob::key("bytes", …)`, a bundle's own verb puts
    /// its chunk at its own key (`Blob::key("chunk", …)`), a bridge relaying a
    /// peer's reply keeps the slot the peer's frame named (`["x.y"]` is the
    /// reply key `"x.y"`, never `x` → `y`). ANY verb may return one — nothing is
    /// declared, no verb list exists — and every wire leg answers binary exactly
    /// when it is `Some` (the REST octet reply, the WS binary frame, a bridge's
    /// binary reply frame). `Some` means the verb RETURNED A CHUNK and `None`
    /// means it returned none, so a 0-byte chunk (an empty file read) stays
    /// distinguishable from `write_stream`'s bodiless status reply. Python has
    /// the distinction for free — `isinstance(reply["bytes"], bytes)` — which is
    /// why an `is_empty()` test served an empty file as JSON and a denial as a
    /// 200.
    ///
    /// Default impl: a bundle whose verbs never carry bytes gets its JSON
    /// `header` dispatched through [`Self::handle`] and a request body is
    /// DROPPED — never base64-encoded onto the payload. A bundle that takes or
    /// answers raw bytes overrides this method and answers EVERY verb here
    /// (`file_bridge.read_stream`/`write_stream`, `exec_bridge.paste_image`, a
    /// bridge's `forward`), routing the rest to its own `handle`. Returns NO
    /// reply body.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        _body: Option<crate::outcome::Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<crate::outcome::Blob>), BundleError> {
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

    /// Can a FOLDER BUNDLE (`<root>/bundles/<name>/bundle.yaml`, `base: <this
    /// bundle's name>`) extend this bundle? Python: the module exposes
    /// `folder_bundle`. Default: no (owner, 2026-10-08).
    fn folder_base(&self) -> bool {
        false
    }

    /// Build the bundle a folder runs as — `name` its folder (the handler_module of
    /// its instances), `folder` the folder's absolute path, `spec` its parsed
    /// `bundle.yaml` — or the refusal text naming the field. Called only on a
    /// [`Self::folder_base`].
    fn folder_bundle(
        &self,
        _name: &str,
        _folder: &std::path::Path,
        _spec: &Value,
    ) -> Result<Arc<dyn Bundle>, String> {
        Err(format!("{} is not a base a folder can extend", self.name()))
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
    /// the `self_extension` shape. A bundle with nothing to add returns
    /// `Ok(None)`.
    ///
    /// `Err` = the hook RAISED — the raised reply (`{"error": "<Class>:
    /// <message>", "raised": true}`), which fails the whole reflect, as python's
    /// bare hook call does. Swallowed, it dropped `persistence` and `guid` with
    /// nothing to say so (audit 2026-10-03, ARCH2-9).
    fn reflect_root_extra(
        &self,
        _agent: &Arc<crate::agent::Agent>,
        _full: bool,
        _kernel: &Arc<Kernel>,
    ) -> Result<Option<Map<String, Value>>, Value> {
        Ok(None)
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
/// catalog; an UNLISTED one (python's `register_unlisted` — a module with no
/// entry point, the e2e scenario probes) dispatches only. The catalog tells a
/// caller what it may `create_agent` from, so a test fixture must not appear in
/// it.
#[derive(Default)]
pub struct BundleRegistry {
    by_handler: HashMap<String, Arc<dyn Bundle>>,
    unlisted: HashSet<String>,
    /// FOLDER BUNDLES of this kernel's root (owner, 2026-10-08): name → the bundle
    /// its base built. Loaded at boot and re-loaded by `reload_bundle`, hence the
    /// lock — the installed map is fixed once the kernel exists.
    folders: std::sync::RwLock<HashMap<String, Arc<dyn Bundle>>>,
    /// What `reflect bundles=all` lists for each folder — a refused one too, with
    /// its `error`.
    folder_rows: std::sync::RwLock<std::collections::BTreeMap<String, Value>>,
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
    /// `register_unlisted`: a module without a `fantastic.bundles` entry point
    /// (the e2e scenario probes). It never appears in the `bundles` catalog.
    pub fn register_unlisted<B: Bundle + 'static>(&mut self, handler_module: &str, bundle: B) {
        self.unlisted.insert(handler_module.to_string());
        self.by_handler
            .insert(handler_module.to_string(), Arc::new(bundle));
    }

    /// Look up a bundle by `handler_module` key. Returns `None` for
    /// unknown — caller decides whether that's a hard error (live
    /// dispatch) or a weak-load skip (rehydration).
    pub fn get(&self, handler_module: &str) -> Option<Arc<dyn Bundle>> {
        self.by_handler.get(handler_module).cloned().or_else(|| {
            self.folders
                .read()
                .expect("folders poisoned")
                .get(handler_module)
                .cloned()
        })
    }

    /// Load a folder bundle: `name` resolves to `bundle` (a reload replaces it).
    pub fn register_folder(&self, name: &str, bundle: Arc<dyn Bundle>, row: Value) {
        self.folders
            .write()
            .expect("folders poisoned")
            .insert(name.to_string(), bundle);
        self.folder_rows
            .write()
            .expect("folder rows poisoned")
            .insert(name.to_string(), row);
    }

    /// A folder that could not load: listed (with its `error`), never resolvable —
    /// unless an earlier load stands.
    pub fn refuse_folder(&self, name: &str, row: Value) {
        // a failed RELOAD keeps an earlier load (module and row): its live
        // instances keep running it; the error goes to the reload's caller
        if self
            .folders
            .read()
            .expect("folders poisoned")
            .contains_key(name)
        {
            return;
        }
        self.folder_rows
            .write()
            .expect("folder rows poisoned")
            .insert(name.to_string(), row);
    }

    /// Every folder bundle this kernel read — loaded and refused — by name.
    #[must_use]
    pub fn folder_rows(&self) -> Vec<Value> {
        self.folder_rows
            .read()
            .expect("folder rows poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Is `handler_module` a loaded folder bundle?
    #[must_use]
    pub fn is_folder(&self, handler_module: &str) -> bool {
        self.folders
            .read()
            .expect("folders poisoned")
            .contains_key(handler_module)
    }

    /// Is `name` a registered bundle's handler module, its package, or its name?
    /// A folder may not shadow one.
    #[must_use]
    pub fn clashes(&self, name: &str) -> bool {
        self.by_handler
            .iter()
            .any(|(hm, b)| hm == name || hm.split('.').next() == Some(name) || b.name() == name)
    }

    /// The installed bundles a folder can extend, by name, sorted.
    #[must_use]
    pub fn folder_bases(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .catalog()
            .filter(|(_, b)| b.folder_base())
            .map(|(_, b)| b.name().to_string())
            .collect();
        out.sort();
        out
    }

    /// The installed bundle named `name` (its `name()`), if any.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<Arc<dyn Bundle>> {
        self.catalog()
            .find(|(_, b)| b.name() == name)
            .map(|(_, b)| Arc::clone(b))
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
