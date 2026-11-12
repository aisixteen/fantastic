//! Test-only support — NOT part of the stable API (`#[doc(hidden)]`).
//!
//! Persistence is INVERTED: the substrate persists records THROUGH a discovered
//! `file_bridge` provider's stream verbs (no direct `fs::write`). Kernel tests
//! therefore need a provider to exercise disk persistence — but they can't
//! depend on the real `fantastic-file` bundle (that crate depends on this one,
//! so it would be a cycle). [`FakeStore`] is a minimal stand-in: it answers
//! `read_stream`/`write_stream` (raw bytes via the binary channel) + `delete`
//! over a real directory, registered under `file_bridge.tools` so
//! `persistence::find_store` discovers it. It is NOT gated (a fake — the real
//! bundle's gate lives in `fantastic-file`).

use crate::agent::AgentId;
use crate::bundle::{BundleError, BundleRegistry, Handler, Reply};
use crate::kernel::Kernel;
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A JSON number python writes bare and Rust's `f64` cannot hold: `NaN`,
/// `Infinity` or `-Infinity`, as the exact token (see [`crate::jsonsafe`]).
#[must_use]
pub fn non_finite(token: &str) -> Value {
    Value::Number(serde_json::Number::from_string_unchecked(token.to_string()))
}

/// A stand-in's call, as the payload it was parsed from: the verb as `type`, then its
/// arguments.
#[must_use]
pub fn tagged<T: serde::Serialize>(verb: &str, args: &T) -> Value {
    let mut m = Map::new();
    m.insert("type".to_string(), Value::from(verb));
    if let Ok(Value::Object(a)) = serde_json::to_value(args) {
        // an argument left out was never in the payload: no null stands for it
        m.extend(a.into_iter().filter(|(_, v)| !v.is_null()));
    }
    Value::Object(m)
}

/// The record of a stand-in bundle: freeform.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AnyRecord {}

/// A stand-in's declared verbs, each with the arguments it really takes:
/// `stand_in_verbs!(name_verbs { Ping "ping" {}, Work "work" { ms: Option<i64> } })`
/// is a module `name_verbs` with one struct per verb (closed: an unknown argument is
/// refused) and the `verbs!` list — `VERBS`, `Call`, `Call::parse`, `dynamic_verbs`.
#[macro_export]
macro_rules! stand_in_verbs {
    ($m:ident { $( $S:ident $name:literal { $( $(#[$fm:meta])* $f:ident : $t:ty ),* $(,)? } ),+ $(,)? }) => {
        #[allow(missing_docs, dead_code)]
        pub mod $m {
            $(
                #[derive(
                    Debug,
                    $crate::verbs::serde::Serialize,
                    $crate::verbs::serde::Deserialize,
                    $crate::verbs::schemars::JsonSchema,
                )]
                #[serde(crate = "fantastic_kernel::verbs::serde", deny_unknown_fields)]
                #[schemars(crate = "fantastic_kernel::verbs::schemars")]
                pub struct $S {
                    $( $(#[$fm])* pub $f: $t ),*
                }
            )+
            $crate::verbs! { $( $name => $S ),+ }

            impl Call {
                /// The call as the payload it was parsed from: a test stand-in reads
                /// its arguments from it by key.
                pub fn payload(&self) -> $crate::verbs::serde_json::Value {
                    match self {
                        $( Call::$S(a) => $crate::test_support::tagged($name, a), )+
                        Call::Reflect(a) => $crate::test_support::tagged("reflect", a),
                    }
                }
            }
        }
    };
}

/// A stand-in that answers no verb of its own: the explicit empty list.
#[must_use]
pub fn no_verbs() -> &'static [crate::Verb] {
    &[]
}

/// The call to a stand-in that answers no verb of its own: `reflect` alone.
#[derive(Debug)]
pub enum NoCall {
    /// The `reflect` every agent answers.
    Reflect(crate::declared::common::Reflect),
}

impl crate::verbs::Parse for NoCall {
    fn parse(agent: &str, payload: &Value, _dynamic: &[crate::DynVerb]) -> Result<NoCall, Value> {
        crate::verbs::resolve_builtin(agent, payload, &[crate::declared::common::VERBS])?;
        crate::verbs::decode("reflect", payload).map(NoCall::Reflect)
    }
}

/// A stand-in with no verbs built at runtime: the explicit empty list.
#[must_use]
pub fn no_dynamic(_agent: &crate::Agent) -> Vec<crate::DynVerb> {
    Vec::new()
}

/// `fixture_record!()` — a freeform record: a `&'static RecordDecl` for [`Bundle::record`].
#[macro_export]
macro_rules! fixture_record {
    () => {{
        static R: $crate::RecordDecl =
            $crate::RecordDecl::of::<$crate::test_support::AnyRecord>(true);
        &R
    }};
}

/// The verbs and record of the stand-ins below, declared as every bundle does.
#[doc(hidden)]
pub mod declared_fixtures {
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    /// Remove a file or a directory.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Delete {
        /// The path under the store's root.
        pub path: String,
        /// Remove a directory with what is in it.
        pub recursive: Option<bool>,
    }

    /// Move a file; the commit protocol's atomic step.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Rename {
        /// The file to move.
        pub old_path: String,
        /// Where it goes.
        pub new_path: String,
    }

    /// Read a text file.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Read {
        /// The path under the store's root.
        pub path: String,
    }

    /// Write a text file.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Write {
        /// The path under the store's root.
        pub path: String,
        /// The text.
        pub content: String,
    }

    /// Read a file's bytes; they ride the binary channel.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ReadStream {
        /// The path under the store's root.
        pub path: String,
        /// Where in the file the chunk starts.
        pub offset: Option<i64>,
    }

    /// Write bytes to a file (appended unless `truncate`); they ride the binary channel.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct WriteStream {
        /// The path under the store's root.
        pub path: String,
        /// Start the file over instead of appending.
        pub truncate: Option<bool>,
    }

    /// The record of the stand-in agents.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    pub struct StoreRecord {
        /// Where the store reads and writes.
        pub root: Option<String>,
        /// The paths it may touch.
        pub allow: Option<Vec<String>>,
        /// How calls reach it.
        pub ingress_rule: Option<String>,
    }

    /// Install the record writer, once.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Boot {}

    /// Store a sidecar file of an agent.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct PersistBlob {
        /// The agent the file belongs to.
        pub agent_id: String,
        /// One bare filename.
        pub name: String,
        /// The file's text.
        pub content: String,
    }

    /// Read an agent's sidecar file.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct LoadBlob {
        /// The agent the file belongs to.
        pub agent_id: String,
        /// One bare filename.
        pub name: String,
    }

    /// The loader record: free.
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    pub struct LoaderRecord {}

    pub mod store_verbs {
        use super::*;
        crate::verbs! {
            "delete" => Delete,
            "rename" => Rename,
            "read" => Read,
            "write" => Write,
            "read_stream" => ReadStream,
            "write_stream" => WriteStream,
        }

        impl Call {
            /// The call as the payload it was parsed from.
            pub fn payload(&self) -> serde_json::Value {
                use crate::test_support::tagged;
                match self {
                    Call::Delete(a) => tagged("delete", a),
                    Call::Rename(a) => tagged("rename", a),
                    Call::Read(a) => tagged("read", a),
                    Call::Write(a) => tagged("write", a),
                    Call::ReadStream(a) => tagged("read_stream", a),
                    Call::WriteStream(a) => tagged("write_stream", a),
                    Call::Reflect(a) => tagged("reflect", a),
                }
            }
        }
    }
    pub mod store_record {
        use super::*;
        crate::record!(StoreRecord, open);
    }
    pub mod loader_verbs {
        use super::*;
        crate::verbs! {
            "boot" => Boot,
            "persist_blob" => PersistBlob,
            "load_blob" => LoadBlob,
        }

        impl Call {
            /// The call as the payload it was parsed from.
            pub fn payload(&self) -> serde_json::Value {
                use crate::test_support::tagged;
                match self {
                    Call::Boot(a) => tagged("boot", a),
                    Call::PersistBlob(a) => tagged("persist_blob", a),
                    Call::LoadBlob(a) => tagged("load_blob", a),
                    Call::Reflect(a) => tagged("reflect", a),
                }
            }
        }
    }
    pub mod loader_record {
        use super::*;
        crate::record!(LoaderRecord, open);
    }
}

/// A minimal stand-in for the real `file_bridge` provider, rooted at `root`.
pub struct FakeStore {
    /// The directory this fake store reads/writes under (the loader's
    /// `.fantastic`). Paths in stream verbs are joined onto it.
    pub root: PathBuf,
}

impl FakeStore {
    /// The text-channel verbs; the call is already parsed by the agent's entry.
    fn run(&self, call: declared_fixtures::store_verbs::Call, id: &AgentId) -> Reply {
        use declared_fixtures::store_verbs::Call;
        match call {
            Call::Delete(a) => {
                let target = self.root.join(&a.path);
                let _ = if target.is_dir() {
                    std::fs::remove_dir_all(&target)
                } else {
                    std::fs::remove_file(&target)
                };
                Some(json!({"deleted": true}))
            }
            // The commit protocol's atomic op (`persistence::persist`
            // writes a tmp sibling then renames it over the target) — a
            // text-channel verb, no blob.
            Call::Rename(a) => {
                let old = self.root.join(&a.old_path);
                let new = self.root.join(&a.new_path);
                if let Some(parent) = new.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                match std::fs::rename(&old, &new) {
                    Ok(()) => Some(json!({"renamed": true})),
                    Err(e) => Some(json!({"error": format!("FakeStore rename: {e}")})),
                }
            }
            // Text read/write — the verbs `yaml_state` (and ai_core) route
            // through. Mirror the real `file_bridge`'s `{content}`/`{written}`.
            Call::Read(a) => match std::fs::read_to_string(self.root.join(&a.path)) {
                Ok(content) => Some(json!({"path": a.path, "content": content})),
                Err(_) => Some(json!({"error": format!("path {:?} not found", a.path)})),
            },
            Call::Write(a) => {
                let target = self.root.join(&a.path);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                match std::fs::write(&target, &a.content) {
                    Ok(_) => Some(json!({"path": a.path, "written": true})),
                    Err(e) => Some(json!({"error": format!("write: {e}")})),
                }
            }
            Call::Reflect(_) => Some(json!({"id": id.0, "sentence": "A stand-in file store."})),
            Call::ReadStream(_) | Call::WriteStream(_) => Some(json!({
                "error": "FakeStore: a stream verb rides the binary channel",
            })),
        }
    }
}

#[async_trait]
impl Handler for FakeStore {
    type Call = declared_fixtures::store_verbs::Call;
    fn name(&self) -> &str {
        "file_bridge"
    }
    fn man(&self) -> &'static str {
        "A stand-in file store for kernel tests."
    }
    fn verbs(&self) -> &'static [crate::verbs::Verb] {
        declared_fixtures::store_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &crate::agent::Agent) -> Vec<crate::verbs::DynVerb> {
        declared_fixtures::store_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static crate::verbs::RecordDecl {
        &declared_fixtures::store_record::RECORD
    }
    async fn handle(
        &self,
        id: &AgentId,
        call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(self.run(call, id))
    }
    async fn handle_binary(
        &self,
        id: &AgentId,
        call: Self::Call,
        body: Option<crate::outcome::Blob>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<crate::outcome::Blob>), BundleError> {
        use declared_fixtures::store_verbs::Call;
        match call {
            // A CHUNK-RETURNING verb answers `Some(bytes)` even when the chunk
            // is empty; anything else answers `None`.
            Call::ReadStream(a) => match std::fs::read(self.root.join(&a.path)) {
                Ok(bytes) => Ok((
                    Some(json!({"size": bytes.len()})),
                    Some(crate::outcome::Blob::key("bytes", bytes)),
                )),
                Err(_) => Ok((Some(json!({"error": "not found"})), None)),
            },
            Call::WriteStream(a) => {
                // the chunk is the payload's `bytes` — python's `payload.get("bytes", b"")`
                let blob = crate::outcome::body_at(body.as_ref(), "bytes").unwrap_or_default();
                let target = self.root.join(&a.path);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                // APPEND unless told to truncate — the real provider's
                // contract (a second pump flush must not wipe the log's header).
                let truncate = a.truncate == Some(true);
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(!truncate)
                    .truncate(truncate)
                    .open(&target)
                    .expect("FakeStore: open");
                std::io::Write::write_all(&mut f, blob).expect("FakeStore: write");
                Ok((Some(json!({"written": blob.len()})), None))
            }
            other => Ok((self.run(other, id), None)),
        }
    }
}

/// THE ROOT LOADER, stood in for. The root's `handler_module` is
/// `kernel_state.tools` — a BUNDLE (`fantastic-core`), which this crate cannot
/// depend on (that crate depends on this one). The verb surface it needs here
/// is small and the MACHINERY is not duplicated: `boot` drives the kernel's own
/// [`crate::persistence::boot_root_loader`], exactly as the shipped bundle
/// does, and the blob verbs call the kernel's own helpers.
#[derive(Default)]
pub struct RootLoader {
    installed: std::sync::Mutex<Option<crate::persistence::RootLoaderState>>,
}

#[async_trait]
impl Handler for RootLoader {
    type Call = declared_fixtures::loader_verbs::Call;
    fn name(&self) -> &str {
        "kernel_state.tools"
    }
    fn man(&self) -> &'static str {
        "A stand-in root loader for kernel tests."
    }
    fn verbs(&self) -> &'static [crate::verbs::Verb] {
        declared_fixtures::loader_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &crate::agent::Agent) -> Vec<crate::verbs::DynVerb> {
        declared_fixtures::loader_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static crate::verbs::RecordDecl {
        &declared_fixtures::loader_record::RECORD
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
            return Ok(Some(json!({"error": "no agent"})));
        };
        use declared_fixtures::loader_verbs::Call;
        match call {
            Call::Boot(_) => {
                let fresh = self.installed.lock().expect("loader poisoned").is_none();
                if fresh {
                    let state = crate::persistence::boot_root_loader(kernel, &agent).await;
                    *self.installed.lock().expect("loader poisoned") = Some(state);
                }
                Ok(None)
            }
            Call::PersistBlob(a) => Ok(Some(
                crate::persistence::persist_blob(kernel, &agent, &a.agent_id, &a.name, &a.content)
                    .await?,
            )),
            Call::LoadBlob(a) => Ok(Some(
                crate::persistence::load_blob(kernel, &agent, &a.agent_id, &a.name).await?,
            )),
            Call::Reflect(_) => Ok(Some(json!({
                "id": agent.id.0,
                "sentence": "A stand-in root loader.",
            }))),
        }
    }
    /// THE SHIPPED BUNDLE'S HOOK, stood in for: the pending RECORDS are written
    /// before the teardown walk, while the provider is still alive
    /// (`fantastic-core`'s `flush_pending`).
    async fn flush_pending(
        &self,
        _agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let writer = {
            let g = self.installed.lock().expect("loader poisoned");
            g.as_ref().and_then(|i| i.writer.clone())
        };
        if let Some(w) = writer {
            w.flush(kernel).await;
        }
        Ok(())
    }

    async fn on_shutdown(
        &self,
        _agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let taken = self.installed.lock().expect("loader poisoned").take();
        if let Some(state) = taken {
            crate::persistence::shutdown_root_loader(state, kernel).await;
        }
        Ok(())
    }
}

/// Register the [`RootLoader`] stand-in under the root's `handler_module`.
pub fn register_root_loader(reg: &mut BundleRegistry) {
    reg.register("kernel_state.tools", RootLoader::default());
}

/// `boot` the root, as `kernel_start` does first — the call that installs the
/// record writer and mints the guid.
pub async fn boot_root(kernel: &Arc<Kernel>) {
    let Some(root) = kernel.root() else { return };
    let _ = kernel
        .send(vec![crate::outcome::Item::new(
            root.id.clone(),
            json!({"type": "boot"}),
        )])
        .await;
}

/// Register the fake `file_bridge` provider (rooted at `store_root`) into a
/// registry before boot. Call on EVERY boot of a kernel that should persist.
pub fn register_fake_store(reg: &mut BundleRegistry, store_root: &Path) {
    reg.register(
        "file_bridge.tools",
        FakeStore {
            root: store_root.to_path_buf(),
        },
    );
}

/// Create + wire the fake store as a `file_bridge.tools` child of root rooted at
/// `store_root`, THROUGH the real `create_agent` verb (so `find_store` discovers
/// it exactly as in production). The bundle must already be registered.
pub async fn wire_fake_store(kernel: &Arc<Kernel>, store_root: &Path) {
    kernel
        .send(vec![crate::outcome::Item::new(
            "kernel",
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "file_bridge.tools", "allow": ["."],
                "id": "store",
                "root": store_root.to_string_lossy(),
                "ingress_rule": "allow_all_ingress",
            }),
        )])
        .await;
}

/// The execution rows still in RAM that are ABOUT `agent` (its `agent` field), in
/// id order — what a test reads where the kernel never started (nothing flushed).
#[must_use]
pub fn rows_about(kernel: &Arc<Kernel>, agent: &AgentId) -> Vec<crate::rows::Row> {
    kernel
        .rows
        .all()
        .into_iter()
        .filter(|r| r.get("agent").and_then(Value::as_str) == Some(agent.0.as_str()))
        .collect()
}

/// THE ONE GRANT HELPER for tests (minimal access model): the
/// operator merges `key: verbs` into `holder`'s `field` (`can_call` or
/// `callable_by`) — every other key kept. Grants are explicit; a test wires the
/// edges it exercises, both ends, through this.
pub fn grant(kernel: &Arc<Kernel>, holder: &str, field: &str, key: &str, verbs: Value) {
    let agent = kernel
        .resolve_target(holder)
        .unwrap_or_else(|| panic!("grant: no agent {holder}"));
    let mut g = agent
        .meta
        .read()
        .expect("meta lock poisoned")
        .get(field)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    g.insert(key.to_string(), verbs);
    let mut patch = serde_json::Map::new();
    patch.insert(field.to_string(), Value::Object(g));
    kernel.update(&agent.id, patch);
}

/// Both ends of one edge: `sender`'s `can_call` names `target` with `verbs`, and
/// `target`'s `callable_by` names `sender` with the same.
pub fn wire(kernel: &Arc<Kernel>, sender: &str, target: &str, verbs: Value) {
    let t = kernel
        .resolve_target(target)
        .unwrap_or_else(|| panic!("wire: no agent {target}"));
    grant(kernel, sender, "can_call", t.id.as_str(), verbs.clone());
    grant(kernel, t.id.as_str(), "callable_by", sender, verbs);
}

/// The test standing in for `agent`'s HANDLER: the sends `work` makes are that
/// agent's, exactly as when the kernel runs the agent's own code (the dispatch
/// identity — the only identity there is). A send made outside it is the
/// operator's. Python `_testkit.as_agent`. A test driver, never a product path.
pub async fn as_agent<F: std::future::Future>(agent: impl Into<AgentId>, work: F) -> F::Output {
    crate::send::as_agent(&agent.into(), work).await
}

/// A symlink at `link` pointing at `target`, on every OS — the fixture a test of
/// the clamp plants (on Windows a typed link, see the kernel's `sys`).
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
    crate::sys::symlink(target.as_ref(), link.as_ref())
}
