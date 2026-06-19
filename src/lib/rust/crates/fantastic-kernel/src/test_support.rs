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
        _id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Vec<u8>), BundleError> {
        let verb = header.get("type").and_then(Value::as_str).unwrap_or("");
        let path = header.get("path").and_then(Value::as_str).unwrap_or("");
        let target = self.root.join(path);
        match verb {
            "read_stream" => match std::fs::read(&target) {
                Ok(bytes) => Ok((Some(json!({"size": bytes.len()})), bytes)),
                Err(_) => Ok((Some(json!({"error": "not found"})), Vec::new())),
            },
            "write_stream" => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&target, &blob).ok();
                Ok((Some(json!({"written": blob.len()})), Vec::new()))
            }
            other => Ok((
                Some(json!({"error": format!("FakeStore: {other:?}")})),
                Vec::new(),
            )),
        }
    }
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
        .send(
            &AgentId::from(
                kernel
                    .root()
                    .map(|r| r.id.0.clone())
                    .unwrap_or_default()
                    .as_str(),
            ),
            json!({
                "type": "create_agent",
                "handler_module": "file_bridge.tools",
                "id": "store",
                "root": store_root.to_string_lossy(),
                "ingress_rule": "allow_all",
            }),
        )
        .await;
}
