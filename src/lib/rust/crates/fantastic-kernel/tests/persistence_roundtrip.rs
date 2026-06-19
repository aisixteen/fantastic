//! Persistence integration — Agent records survive write→read cycles
//! and weak-load skip+log fires the documented contract line.
//!
//! Persistence is now INVERTED: the substrate persists records THROUGH a
//! DISCOVERED `file_bridge` provider's stream verbs (no direct `fs::write`).
//! These tests wire a minimal provider (`FakeStore`, a stand-in for the real
//! `fantastic-file` bundle — which can't be a dev-dep here without a cycle)
//! and assert the record lands on disk via it. With NO provider wired, persist
//! is a no-op (RAM) — also asserted.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Kernel, Reply, StorageMode};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

// A noop bundle so we can register a recognized handler_module for
// the weak-load contrast tests (known vs unknown).
struct NoopBundle;

#[async_trait]
impl Bundle for NoopBundle {
    fn name(&self) -> &str {
        "noop"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &std::sync::Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        Ok(None)
    }
}

/// A minimal stand-in for the real `file_bridge` provider — answers
/// `read_stream`/`write_stream` (raw bytes via the binary channel) and `delete`
/// over a real directory (`root`). Registered under `file_bridge.tools` so
/// `persistence::find_store` discovers it. NOT gated (a fake) — the real
/// bundle's gate is tested in `fantastic-file`.
struct FakeStore {
    root: PathBuf,
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
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if verb == "delete" {
            let path = payload.get("path").and_then(Value::as_str).unwrap_or("");
            let target = self.root.join(path);
            let _ = if target.is_dir() {
                std::fs::remove_dir_all(&target)
            } else {
                std::fs::remove_file(&target)
            };
            return Ok(Some(json!({"deleted": true})));
        }
        Ok(Some(
            json!({"error": format!("FakeStore: text verb {verb:?}")}),
        ))
    }
    async fn handle_binary(
        &self,
        _id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
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

fn fresh_workdir() -> TempDir {
    TempDir::new().expect("tempdir")
}

/// Build a Disk-mode kernel rooted at `store_root` (root id `core`) with the
/// noop + FakeStore bundles registered. The store is NOT wired yet — callers
/// wire it through the real `create_agent` verb when they want persistence.
fn kernel_at(store_root: &Path) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(StorageMode::Disk(store_root.to_path_buf()));
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", NoopBundle);
    reg.register(
        "file_bridge.tools",
        FakeStore {
            root: store_root.to_path_buf(),
        },
    );
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        store_root.to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

/// Wire the FakeStore as a `file_bridge.tools` child of root rooted at
/// `store_root`, THROUGH the real `create_agent` verb (so `children` is
/// populated for `find_store` exactly as in production).
async fn wire_store(kernel: &Arc<Kernel>, store_root: &Path) {
    kernel
        .send(
            &AgentId::from("core"),
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

#[tokio::test]
async fn persist_through_provider_writes_record() {
    let tmp = fresh_workdir();
    let store_root = tmp.path();
    let kernel = kernel_at(store_root);
    wire_store(&kernel, store_root).await;

    // Create a child of root through the real lifecycle — its record persists
    // THROUGH the discovered provider (no direct fs::write in the substrate).
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": "noop.tools",
                "id": "test_1",
                "display_name": "Testy",
                "port": 18181,
            }),
        )
        .await;

    let rec =
        fantastic_kernel::persistence::read_record_at(&store_root.join("agents/test_1/agent.json"))
            .expect("read ok")
            .expect("file exists — persisted through the provider");
    assert_eq!(rec.id, "test_1");
    assert_eq!(rec.handler_module.as_deref(), Some("noop.tools"));
    assert_eq!(rec.parent_id.as_deref(), Some("core"));
    assert_eq!(rec.meta["port"], json!(18181));
    assert_eq!(rec.meta["display_name"], json!("Testy"));
    // The provider also persisted its OWN record through itself (self-persist).
    assert!(store_root.join("agents/store/agent.json").exists());
}

#[tokio::test]
async fn persist_with_no_provider_is_ram_noop() {
    let tmp = fresh_workdir();
    let store_root = tmp.path();
    let kernel = kernel_at(store_root); // NO store wired ⇒ find_store → None.

    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": "noop.tools",
                "id": "test_1",
            }),
        )
        .await;

    // No provider ⇒ nothing written (RAM). NO FALLBACK to direct fs.
    assert!(
        !store_root.join("agents/test_1/agent.json").exists(),
        "no provider wired ⇒ record must stay in RAM (no direct-fs fallback)"
    );
    // But the agent IS live in RAM.
    assert!(kernel.agents.contains_key(&AgentId::from("test_1")));
}

#[tokio::test]
async fn delete_removes_record_through_provider() {
    let tmp = fresh_workdir();
    let store_root = tmp.path();
    let kernel = kernel_at(store_root);
    wire_store(&kernel, store_root).await;
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type": "create_agent", "handler_module": "noop.tools", "id": "doomed"}),
        )
        .await;
    assert!(store_root.join("agents/doomed/agent.json").exists());
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type": "delete_agent", "id": "doomed"}),
        )
        .await;
    // The provider's recursive delete removed the dir.
    assert!(
        !store_root.join("agents/doomed").exists(),
        "delete must remove the agent dir through the provider"
    );
}

#[test]
fn load_children_hydrates_registered_agents() {
    let tmp = fresh_workdir();
    let root = tmp.path();
    // Stage two children under root: agents/known + agents/known/agents/grandchild.
    let known_dir = root.join("agents/known");
    let grand_dir = known_dir.join("agents/grandchild");
    std::fs::create_dir_all(&grand_dir).unwrap();
    std::fs::write(
        known_dir.join("agent.json"),
        r#"{"id":"known","handler_module":"noop.tools","parent_id":"core"}"#,
    )
    .unwrap();
    std::fs::write(
        grand_dir.join("agent.json"),
        r#"{"id":"grandchild","handler_module":"noop.tools","parent_id":"known"}"#,
    )
    .unwrap();
    let kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", NoopBundle);
    // Synthetic parent that points at the workdir root.
    let parent = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        root.to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&parent));
    let loaded = fantastic_kernel::persistence::load_children(&kernel, &reg, Arc::clone(&parent))
        .expect("hydrate");
    assert_eq!(loaded.len(), 2);
    assert!(kernel.agents.contains_key(&AgentId::from("known")));
    assert!(kernel.agents.contains_key(&AgentId::from("grandchild")));
    // Grandchild also wired into known's children map:
    let known = kernel.agents.get(&AgentId::from("known")).unwrap().clone();
    assert!(known.has_child(&AgentId::from("grandchild")));
}

#[test]
fn load_children_weak_load_skips_unknown_handler_module_and_subtree() {
    // Plant a ghost agent whose handler_module isn't registered, AND
    // a real child underneath it. Both must be skipped; neither should
    // appear in kernel.agents. The on-disk records stay untouched.
    let tmp = fresh_workdir();
    let root = tmp.path();
    let ghost_dir = root.join("agents/ghost_1");
    let ghost_child = ghost_dir.join("agents/ghost_child");
    std::fs::create_dir_all(&ghost_child).unwrap();
    std::fs::write(
        ghost_dir.join("agent.json"),
        r#"{"id":"ghost_1","handler_module":"unknown.tools","parent_id":"core"}"#,
    )
    .unwrap();
    std::fs::write(
        ghost_child.join("agent.json"),
        r#"{"id":"ghost_child","handler_module":"noop.tools","parent_id":"ghost_1"}"#,
    )
    .unwrap();
    // And a sibling with a recognized handler — must STILL load.
    let real_dir = root.join("agents/real_1");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::write(
        real_dir.join("agent.json"),
        r#"{"id":"real_1","handler_module":"noop.tools","parent_id":"core"}"#,
    )
    .unwrap();

    let kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", NoopBundle);
    let parent = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        root.to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&parent));
    let loaded = fantastic_kernel::persistence::load_children(&kernel, &reg, Arc::clone(&parent))
        .expect("hydrate");

    // Only `real_1` loaded; ghost branch skipped wholesale.
    assert_eq!(loaded, vec![AgentId::from("real_1")]);
    assert!(kernel.agents.contains_key(&AgentId::from("real_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("ghost_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("ghost_child")));

    // On-disk records survived untouched (Reboot under a runtime with
    // unknown.tools and ghost_1 rehydrates intact).
    assert!(ghost_dir.join("agent.json").exists());
    assert!(ghost_child.join("agent.json").exists());
}

#[test]
fn load_children_tolerates_corrupt_agent_json() {
    let tmp = fresh_workdir();
    let root = tmp.path();
    let bad = root.join("agents/broken");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("agent.json"), "{NOT_JSON").unwrap();

    let kernel = Kernel::new();
    let reg = BundleRegistry::new();
    let parent = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        root.to_path_buf(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&parent));
    let loaded = fantastic_kernel::persistence::load_children(&kernel, &reg, Arc::clone(&parent))
        .expect("hydrate");
    assert!(loaded.is_empty());
    assert!(!kernel.agents.contains_key(&AgentId::from("broken")));
}
