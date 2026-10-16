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
use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use crate::kernel::Kernel;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A minimal stand-in for the real `file_bridge` provider, rooted at `root`.
pub struct FakeStore {
    /// The directory this fake store reads/writes under (the loader's
    /// `.fantastic`). Paths in stream verbs are joined onto it.
    pub root: PathBuf,
}

#[async_trait]
impl Bundle for FakeStore {
    fn name(&self) -> &str {
        "file_bridge"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let path = payload.get("path").and_then(Value::as_str).unwrap_or("");
        let target = self.root.join(path);
        match verb {
            "delete" => {
                let _ = if target.is_dir() {
                    std::fs::remove_dir_all(&target)
                } else {
                    std::fs::remove_file(&target)
                };
                Ok(Some(json!({"deleted": true})))
            }
            // The commit protocol's atomic op (`persistence::persist`
            // writes a tmp sibling then renames it over the target) — a
            // text-channel verb, no blob.
            "rename" => {
                let old_path = payload
                    .get("old_path")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let new_path = payload
                    .get("new_path")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let old = self.root.join(old_path);
                let new = self.root.join(new_path);
                if let Some(parent) = new.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                match std::fs::rename(&old, &new) {
                    Ok(()) => Ok(Some(json!({"renamed": true}))),
                    Err(e) => Ok(Some(json!({"error": format!("FakeStore rename: {e}")}))),
                }
            }
            // Text read/write — the verbs `yaml_state` (and ai_core) route
            // through. Mirror the real `file_bridge`'s `{content}`/`{written}`.
            "read" => match std::fs::read_to_string(&target) {
                Ok(content) => Ok(Some(json!({"path": path, "content": content}))),
                Err(_) => Ok(Some(json!({"error": format!("path {path:?} not found")}))),
            },
            "write" => {
                let content = payload.get("content").and_then(Value::as_str).unwrap_or("");
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                match std::fs::write(&target, content) {
                    Ok(_) => Ok(Some(json!({"path": path, "written": true}))),
                    Err(e) => Ok(Some(json!({"error": format!("write: {e}")}))),
                }
            }
            // boot/shutdown/reflect etc. — harmless no-op acks.
            _ => Ok(Some(Value::Null)),
        }
    }
    async fn handle_binary(
        &self,
        id: &AgentId,
        header: Value,
        body: Option<crate::outcome::Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<crate::outcome::Blob>), BundleError> {
        // the chunk is the payload's `bytes` — python's `payload.get("bytes", b"")`
        let blob = crate::outcome::body_at(body.as_ref(), "bytes").unwrap_or_default();
        let verb = header.get("type").and_then(Value::as_str).unwrap_or("");
        let path = header.get("path").and_then(Value::as_str).unwrap_or("");
        let target = self.root.join(path);
        match verb {
            // A CHUNK-RETURNING verb answers `Some(bytes)` even when the chunk
            // is empty; anything else answers `None`.
            "read_stream" => match std::fs::read(&target) {
                Ok(bytes) => Ok((
                    Some(json!({"size": bytes.len()})),
                    Some(crate::outcome::Blob::key("bytes", bytes)),
                )),
                Err(_) => Ok((Some(json!({"error": "not found"})), None)),
            },
            "write_stream" => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                // APPEND unless told to truncate — the real provider's
                // contract (a second pump flush must not wipe the log's header).
                let truncate = header.get("truncate").and_then(Value::as_bool) == Some(true);
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
            // every other verb is the JSON half — the kernel dispatches
            // EVERY call here
            _ => Ok((self.handle(id, &header, kernel).await?, None)),
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
impl Bundle for RootLoader {
    fn name(&self) -> &str {
        "kernel_state.tools"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
            return Ok(Some(json!({"error": "no agent"})));
        };
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" => {
                let fresh = self.installed.lock().expect("loader poisoned").is_none();
                if fresh {
                    let state = crate::persistence::boot_root_loader(kernel, &agent).await;
                    *self.installed.lock().expect("loader poisoned") = Some(state);
                }
                Ok(None)
            }
            "persist_blob" => Ok(Some(
                crate::persistence::persist_blob(kernel, &agent, payload).await?,
            )),
            "load_blob" => Ok(Some(
                crate::persistence::load_blob(kernel, &agent, payload).await?,
            )),
            other => Ok(Some(json!({
                "error": format!("kernel_state: unknown type '{other}'"),
                "reason": "unhandled",
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

/// The record writer is DEBOUNCED (150 ms, python's rhythm), so a test that
/// asserts on disk waits for a pass.
pub async fn settle_records() {
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
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
