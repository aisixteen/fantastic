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
    /// Accepting a value you cannot honour AND reporting success is the worst of
    /// both: an agent setting `ingress_rule="password:kernel-internal-transcripts"`
    /// would believe a security control was in place that no resolver implements.
    ///
    /// `record` is the agent's CURRENT meta (empty at create), so a bundle
    /// validates the state the write WILL produce, not the patch in isolation —
    /// py-canonical: a cross-field rule (exec_bridge's `interpreter` against
    /// `interpreters`) is unenforceable from the patch alone, and checking the
    /// patch would falsely refuse `update_agent id=x interpreter=<already allowed>`.
    /// Called at BOTH create and update, so no record is born with meta its own
    /// bundle refuses.
    fn validate_meta(
        &self,
        _meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        None
    }

    /// THE VERBS this bundle's agents answer, DECLARED (`verbs!{}` in its `verbs.rs`): the
    /// list the agent's own entry (`verbs::Call::parse`) checks every call against —
    /// verb known, arguments known, present and typed — and `reflect` and `docs`
    /// render. Required.
    fn verbs(&self) -> &'static [crate::verbs::Verb];

    /// THE VERBS THIS AGENT carries beside the builtin ones: built at runtime for the
    /// agent instance (an MCP client's tools), the same schema shape as a builtin
    /// verb's and checked by the same engine. `reflect` and `docs` render them with
    /// the builtin ones. Required: a bundle with none returns the explicit empty
    /// list its `verbs!` generates (`verbs::dynamic_verbs`).
    fn dynamic_verbs(&self, agent: &crate::agent::Agent) -> Vec<crate::verbs::DynVerb>;

    /// THE RECORD this bundle's agents carry, DECLARED (`record!` in its `record.rs`):
    /// the fields `create_agent` / `update_agent` may set on them, beside the fields
    /// every record has ([`crate::declared::record`]); also what `reflect bundles=all`
    /// lists for the kind. Required.
    fn record(&self) -> &'static crate::verbs::RecordDecl;

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

    /// SECRET PATHS of an incoming call (`crate::secret`): dotted paths into
    /// `payload` whose values the kernel keeps and shows only as `[secret]` — the
    /// call row, the state stream, every watcher's mirror — added to the item's
    /// own `secret` marks. The handler still gets the payload as sent. Default: none.
    fn payload_secret(&self, _payload: &Value) -> Vec<String> {
        Vec::new()
    }

    /// SECRET PATHS of a reply: dotted paths into `reply` (what the handler
    /// answered to `payload`) that the kernel records — the settle row, the
    /// `ledger` verb, the memory store — only as `[secret]`. The caller receives
    /// the reply whole. Default: none.
    ///
    /// KNOWN GAP (issue #285): both secret hooks run on the kernel that HOSTS the
    /// bundle. A call to an agent on a CONNECTED kernel is recorded by the CALLING
    /// kernel, which does not know that agent's marks — so it records the payload
    /// and the reply in clear.
    fn reply_secret(&self, _payload: &Value, _reply: &Value) -> Vec<String> {
        Vec::new()
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

    /// Pre-shutdown hook — the ONLY hook the shutdown walk calls (there is no
    /// `on_delete` fallback). Default: noop.
    async fn on_shutdown(
        &self,
        _agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        Ok(())
    }

    /// THE ROOT LOADER'S COLD PASS, before the tree loads — python
    /// `main._bootstrap` → `kernel_state.load_bundles`: the root's bundle reads
    /// what the kernel needs registered first (its folder bundles) from its own
    /// root, `fantastic_dir`. Default: nothing.
    fn cold_boot(&self, _kernel: &Kernel, _fantastic_dir: &std::path::Path) {}

    /// Can a FOLDER BUNDLE (`<root>/bundles/<name>/bundle.yaml`, `base: <this
    /// bundle's name>`) extend this bundle? Python: the module exposes
    /// `folder_bundle`. Default: no.
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
    /// bare hook call does — never swallowed, which would drop `persistence` and
    /// `guid` with nothing to say so.
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
    /// FOLDER BUNDLES of this kernel's root: name → the bundle
    /// its base built. Loaded at boot and re-loaded by `reload_bundle`, hence the
    /// lock — the installed map is fixed once the kernel exists.
    folders: std::sync::RwLock<HashMap<String, Arc<dyn Bundle>>>,
    /// What `reflect bundles=all` lists for each folder — a refused one too, with
    /// its `error`.
    folder_rows: std::sync::RwLock<std::collections::BTreeMap<String, Value>>,
    /// EMBEDDED META BUNDLES: a product's own `bundle.yaml` texts, registered from
    /// its code ([`Self::register_meta_yaml`]) and held in RAM only — nothing is
    /// written to a folder. A folder bundle of the same name wins over one; delete
    /// the folder and the embedded one stands again.
    embedded: std::sync::RwLock<std::collections::BTreeMap<String, Value>>,
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
    ///
    /// A META BUNDLE (a `bundle.yaml` with a `base` and no program) resolves to its
    /// base's bundle: an instance of it IS an instance of the base, with the meta
    /// bundle's fields under its own ([`Self::defaults`]).
    pub fn get(&self, handler_module: &str) -> Option<Arc<dyn Bundle>> {
        if let Some(base) = self.meta_base(handler_module) {
            return self.get(&base);
        }
        self.by_handler
            .get(handler_module)
            .cloned()
            .or_else(|| {
                self.folders
                    .read()
                    .expect("folders poisoned")
                    .get(handler_module)
                    .cloned()
            })
            // A bare bundle name (`ai_chat`) is accepted for its `.tools` module (owner).
            .or_else(|| {
                (!handler_module.contains('.'))
                    .then(|| {
                        self.by_handler
                            .get(&format!("{handler_module}.tools"))
                            .cloned()
                    })
                    .flatten()
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
            || self.folder_meta(name).is_some()
        {
            return;
        }
        self.folder_rows
            .write()
            .expect("folder rows poisoned")
            .insert(name.to_string(), row);
    }

    /// Every folder bundle this kernel read — loaded and refused — by name. A meta
    /// bundle's row is listed without its `record` (the fields, a directive among
    /// them, are the instances' business, not the catalog's).
    ///
    /// Embedded meta bundles are listed too (`source: embedded`), a folder's
    /// (`source: folder`) in place of an embedded one of the same name — unless the
    /// folder was refused: then the embedded one is live, listed with the folder's
    /// `folder_error`.
    #[must_use]
    pub fn folder_rows(&self) -> Vec<Value> {
        let folders = self.folder_rows.read().expect("folder rows poisoned");
        let embedded = self.embedded.read().expect("embedded poisoned");
        let mut out: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
        for (name, r) in embedded.iter() {
            let mut r = r.clone();
            r["source"] = Value::from("embedded");
            out.insert(name.clone(), r);
        }
        for (name, r) in folders.iter() {
            let mut r = r.clone();
            match (r.get("error").cloned(), out.get_mut(name)) {
                (Some(err), Some(live)) => {
                    live["folder_error"] = err;
                }
                _ => {
                    r["source"] = Value::from("folder");
                    out.insert(name.clone(), r);
                }
            }
        }
        out.into_values()
            .map(|mut r| {
                if let Some(o) = r.as_object_mut() {
                    o.remove("record");
                }
                r
            })
            .collect()
    }

    /// The SHAPE of a meta bundle, whatever its source (a folder's `bundle.yaml`, or
    /// a text a product embeds): `record` a mapping, `base` a bundle of this registry
    /// (a registered `handler_module`, a meta bundle, an installed bundle's name) that
    /// never leads back to `name`. `Ok` is the row to register, `base` resolved to its
    /// handler module; `Err` names the field.
    pub fn meta_row(
        &self,
        name: &str,
        base: &str,
        sentence: &str,
        spec: &Value,
        row: Value,
    ) -> Result<Value, String> {
        let record = match spec.get("record") {
            None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
            Some(r @ Value::Object(_)) => r.clone(),
            Some(_) => {
                return Err(
                    "record: must be a mapping of the fields every instance inherits".to_string(),
                )
            }
        };
        let Some(handler) = self.handler_of(base) else {
            let bases = self.folder_bases().join(", ");
            return Err(format!(
                "base: {} is no bundle of this kernel — a folder extends a registered \
                 handler_module or another folder bundle (folder bases with a program — \
                 known: {})",
                fantastic_bundle::py_repr(base),
                if bases.is_empty() { "none" } else { &bases }
            ));
        };
        // no cycle: following the bases from `base` never comes back to `name`
        let mut at = handler.clone();
        for _ in 0..17 {
            if at == name {
                return Err(format!(
                    "base: {} leads back to {} — a meta bundle cannot extend itself",
                    fantastic_bundle::py_repr(base),
                    fantastic_bundle::py_repr(name)
                ));
            }
            match self.meta_base(&at) {
                Some(next) => at = next,
                None => break,
            }
        }
        let mut r = row;
        r["base"] = Value::from(handler);
        r["sentence"] = Value::from(sentence);
        r["record"] = record;
        Ok(r)
    }

    /// Register an EMBEDDED META BUNDLE: `yaml` is a `bundle.yaml` text (`base`,
    /// `sentence`, `record`) a product ships in its code. It is checked like a
    /// folder's ([`Self::meta_row`]) and held in RAM — nothing is written to a folder;
    /// a folder bundle of the same name overrides it. Register its base first.
    pub fn register_meta_yaml(&self, name: &str, yaml: &str) -> Result<(), String> {
        let spec = crate::yaml_io::load(yaml).map_err(|e| format!("{name}: {e}"))?;
        let base = spec
            .get("base")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{name}: base: required — the bundle it extends"))?;
        let sentence = spec
            .get("sentence")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("{name}: sentence: required — one line saying what it is"))?;
        let row = serde_json::json!({"name": name, "handler_module": name});
        let row = self
            .meta_row(name, base, sentence, &spec, row)
            .map_err(|e| format!("{name}: {e}"))?;
        self.embedded
            .write()
            .expect("embedded poisoned")
            .insert(name.to_string(), row);
        Ok(())
    }

    /// Load a META BUNDLE: a `bundle.yaml` with a `base` (a registered
    /// `handler_module`, or another meta bundle) and no program. Its row, with the
    /// `base` and the `record` its instances inherit, is all there is of it: nothing
    /// is built or registered, and [`Self::get`] resolves `name` to the base.
    pub fn register_meta(&self, name: &str, row: Value) {
        self.folders.write().expect("folders poisoned").remove(name);
        self.folder_rows
            .write()
            .expect("folder rows poisoned")
            .insert(name.to_string(), row);
    }

    /// The meta bundles `handler_module` goes through on its way to the compiled
    /// base, itself first. Empty for anything that is not a meta bundle.
    #[must_use]
    pub fn meta_chain(&self, handler_module: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut at = handler_module.to_string();
        while let Some(base) = self.meta_base(&at) {
            if chain.contains(&at) || chain.len() > 16 {
                break;
            }
            chain.push(at.clone());
            at = base;
        }
        chain
    }

    /// The compiled (or program folder) bundle a meta bundle runs on, at the end of its
    /// chain; `name` itself when it is no meta bundle.
    #[must_use]
    pub fn base_of(&self, name: &str) -> String {
        let chain = self.meta_chain(name);
        chain
            .last()
            .and_then(|m| self.meta_base(m))
            .unwrap_or_else(|| name.to_string())
    }

    /// The `base` of the loaded meta bundle `name`, if it is one: its folder's (a
    /// folder with no program — its row holds its `record`, and no built bundle
    /// stands for it), else its embedded one.
    fn meta_base(&self, name: &str) -> Option<String> {
        self.meta_row_of(name)?
            .get("base")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// The live row of the meta bundle `name`: a loaded folder meta bundle wins over
    /// an embedded one. `None`: no meta bundle by that name (or a program folder).
    fn meta_row_of(&self, name: &str) -> Option<Value> {
        if self
            .folders
            .read()
            .expect("folders poisoned")
            .contains_key(name)
        {
            return None;
        }
        self.folder_meta(name).or_else(|| {
            self.embedded
                .read()
                .expect("embedded poisoned")
                .get(name)
                .cloned()
        })
    }

    /// A FOLDER meta bundle's row: loaded, no error, a `record`.
    fn folder_meta(&self, name: &str) -> Option<Value> {
        let rows = self.folder_rows.read().expect("folder rows poisoned");
        let row = rows.get(name)?;
        if row.get("error").is_some() || row.get("record").is_none() {
            return None;
        }
        Some(row.clone())
    }

    /// The fields an instance of `handler_module` inherits, whatever its source: a
    /// compiled bundle gives none; a folder bundle its `record`; a meta bundle its
    /// own and each bundle's on the way to the compiled base — the nearest winning.
    #[must_use]
    pub fn defaults(&self, handler_module: &str) -> serde_json::Map<String, Value> {
        let mut chain = self.meta_chain(handler_module);
        let last = chain.last().map_or(handler_module.to_string(), |m| {
            self.meta_base(m).unwrap_or_default()
        });
        if self.is_folder(&last) {
            chain.push(last);
        }
        let mut out = serde_json::Map::new();
        for name in chain.iter().rev() {
            // a meta bundle's live row (folder over embedded), or a program folder's own
            let row = self.meta_row_of(name).or_else(|| {
                self.folder_rows
                    .read()
                    .expect("folder rows poisoned")
                    .get(name)
                    .cloned()
            });
            if let Some(Value::Object(rec)) = row.as_ref().and_then(|r| r.get("record")) {
                for (k, v) in rec {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        out
    }

    /// The handler module a `base:` names: a registered `handler_module`, a loaded
    /// meta bundle, or an installed bundle's name (`anthropic_backend` →
    /// `anthropic_backend.tools`).
    #[must_use]
    pub fn handler_of(&self, base: &str) -> Option<String> {
        if self.get(base).is_some() {
            return Some(base.to_string());
        }
        self.by_handler
            .iter()
            .find(|(_, b)| b.name() == base)
            .map(|(hm, _)| hm.clone())
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
