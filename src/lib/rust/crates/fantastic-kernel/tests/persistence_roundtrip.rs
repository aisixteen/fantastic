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
use fantastic_kernel::Item;
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

/// Like [`NoopBundle`], but ships a readme — the only way `seed_readme` (C45)
/// ever runs: `flush_records` looks the created agent's `handler_module` up in
/// the registry and only calls it when `Bundle::readme()` answers `Some`.
struct ReadmeBundle;

#[async_trait]
impl Bundle for ReadmeBundle {
    fn name(&self) -> &str {
        "readme_bundle"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &std::sync::Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        Ok(None)
    }
    fn readme(&self) -> Option<&'static str> {
        Some("shipped readme text\n")
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
        if verb == "rename" {
            // The C43 commit protocol's atomic op — a text-channel verb, no blob.
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
            return match std::fs::rename(&old, &new) {
                Ok(()) => Ok(Some(json!({"renamed": true}))),
                Err(e) => Ok(Some(json!({"error": format!("FakeStore rename: {e}")}))),
            };
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
    ) -> Result<(Reply, Option<Vec<u8>>), Box<dyn std::error::Error + Send + Sync>> {
        let verb = header.get("type").and_then(Value::as_str).unwrap_or("");
        let path = header.get("path").and_then(Value::as_str).unwrap_or("");
        let target = self.root.join(path);
        match verb {
            "read_stream" => match std::fs::read(&target) {
                Ok(bytes) => Ok((Some(json!({"size": bytes.len()})), Some(bytes))),
                Err(_) => Ok((Some(json!({"error": "not found"})), None)),
            },
            "write_stream" => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&target, &blob).ok();
                Ok((Some(json!({"written": blob.len()})), None))
            }
            other => Ok((
                Some(json!({"error": format!("FakeStore: {other:?}")})),
                None,
            )),
        }
    }
}

fn fresh_workdir() -> TempDir {
    TempDir::new().expect("tempdir")
}

/// Build a Disk-mode kernel rooted at `store_root` (root id `kernel_state`) with the
/// noop + FakeStore bundles registered. The store is NOT wired yet — callers
/// wire it through the real `create_agent` verb when they want persistence.
fn kernel_at(store_root: &Path) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(StorageMode::Disk(store_root.to_path_buf()));
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", NoopBundle);
    reg.register("readme_bundle.tools", ReadmeBundle);
    reg.register(
        "file_bridge.tools",
        FakeStore {
            root: store_root.to_path_buf(),
        },
    );
    fantastic_kernel::test_support::register_root_loader(&mut reg);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some("kernel_state.tools".to_string()),
        None,
        Map::new(),
        store_root.to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

/// Wire the FakeStore as a `file_bridge.tools` child of root rooted at
/// `store_root`, THROUGH the real `create_agent` verb (so `children` is
/// populated for `find_store` exactly as in production).
async fn wire_store(kernel: &Arc<Kernel>, store_root: &Path) {
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "file_bridge.tools",
                "id": "store",
                "root": store_root.to_string_lossy(),
                "ingress_rule": "allow_all_ingress",
            }),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn persist_through_provider_writes_record() {
    let tmp = fresh_workdir();
    let store_root = tmp.path();
    let kernel = kernel_at(store_root);
    // THE ROOT BOOTS FIRST (registration order): its `boot` subscribes the
    // record writer, which is the only path a record reaches disk by now.
    fantastic_kernel::test_support::boot_root(&kernel).await;
    wire_store(&kernel, store_root).await;

    // Create a child of root through the real lifecycle — its record persists
    // THROUGH the discovered provider (no direct fs::write in the substrate).
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "noop.tools",
                "id": "test_1",
                "display_name": "Testy",
                "port": 18181,
            }),
        )])
        .await
        .remove(0)
        .result;

    // DEBOUNCED (python's 150 ms `_FlushLoop` rhythm).
    fantastic_kernel::test_support::settle_records().await;
    let rec =
        fantastic_kernel::persistence::read_record_at(&store_root.join("agents/test_1/agent.yaml"))
            .expect("read ok")
            .expect("file exists — persisted through the provider");
    assert_eq!(rec.id, "test_1");
    assert_eq!(rec.handler_module.as_deref(), Some("noop.tools"));
    assert_eq!(rec.parent_id.as_deref(), Some("kernel_state"));
    assert_eq!(rec.meta["port"], json!(18181));
    assert_eq!(rec.meta["display_name"], json!("Testy"));
    // The provider also persisted its OWN record through itself (self-persist).
    assert!(store_root.join("agents/store/agent.yaml").exists());
}

#[tokio::test]
async fn persist_with_no_provider_is_ram_noop() {
    let tmp = fresh_workdir();
    let store_root = tmp.path();
    let kernel = kernel_at(store_root); // NO store wired ⇒ find_store → None.

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "noop.tools",
                "id": "test_1",
            }),
        )])
        .await
        .remove(0)
        .result;

    fantastic_kernel::test_support::settle_records().await;
    // No provider ⇒ nothing written (RAM). NO FALLBACK to direct fs.
    assert!(
        !store_root.join("agents/test_1/agent.yaml").exists(),
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
    // THE ROOT BOOTS FIRST (registration order): its `boot` subscribes the
    // record writer, which is the only path a record reaches disk by now.
    fantastic_kernel::test_support::boot_root(&kernel).await;
    wire_store(&kernel, store_root).await;
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "noop.tools", "id": "doomed"}))])
        .await.remove(0).result;
    fantastic_kernel::test_support::settle_records().await;
    assert!(store_root.join("agents/doomed/agent.yaml").exists());
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": "doomed"}),
        )])
        .await
        .remove(0)
        .result;
    fantastic_kernel::test_support::settle_records().await;
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
        known_dir.join("agent.yaml"),
        r#"{"id":"known","handler_module":"noop.tools","parent_id":"kernel_state"}"#,
    )
    .unwrap();
    std::fs::write(
        grand_dir.join("agent.yaml"),
        r#"{"id":"grandchild","handler_module":"noop.tools","parent_id":"known"}"#,
    )
    .unwrap();
    let kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", NoopBundle);
    // Synthetic parent that points at the workdir root.
    let parent = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        root.to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&parent));
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
        ghost_dir.join("agent.yaml"),
        r#"{"id":"ghost_1","handler_module":"unknown.tools","parent_id":"kernel_state"}"#,
    )
    .unwrap();
    std::fs::write(
        ghost_child.join("agent.yaml"),
        r#"{"id":"ghost_child","handler_module":"noop.tools","parent_id":"ghost_1"}"#,
    )
    .unwrap();
    // And a sibling with a recognized handler — must STILL load.
    let real_dir = root.join("agents/real_1");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::write(
        real_dir.join("agent.yaml"),
        r#"{"id":"real_1","handler_module":"noop.tools","parent_id":"kernel_state"}"#,
    )
    .unwrap();

    let kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", NoopBundle);
    let parent = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        root.to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&parent));
    let loaded = fantastic_kernel::persistence::load_children(&kernel, &reg, Arc::clone(&parent))
        .expect("hydrate");

    // Only `real_1` loaded; ghost branch skipped wholesale.
    assert_eq!(loaded, vec![AgentId::from("real_1")]);
    assert!(kernel.agents.contains_key(&AgentId::from("real_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("ghost_1")));
    assert!(!kernel.agents.contains_key(&AgentId::from("ghost_child")));

    // On-disk records survived untouched (Reboot under a runtime with
    // unknown.tools and ghost_1 rehydrates intact).
    assert!(ghost_dir.join("agent.yaml").exists());
    assert!(ghost_child.join("agent.yaml").exists());
}

#[test]
fn load_children_tolerates_corrupt_agent_json() {
    let tmp = fresh_workdir();
    let root = tmp.path();
    let bad = root.join("agents/broken");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("agent.yaml"), "{NOT_JSON").unwrap();

    let kernel = Kernel::new();
    let reg = BundleRegistry::new();
    let parent = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        root.to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&parent));
    let loaded = fantastic_kernel::persistence::load_children(&kernel, &reg, Arc::clone(&parent))
        .expect("hydrate");
    assert!(loaded.is_empty());
    assert!(!kernel.agents.contains_key(&AgentId::from("broken")));
}

/// C14 (RP-09): the record writer's DIRTY SET SURVIVES a no-store flush. Python
/// returns BEFORE clearing the queues when nothing is wired, so everything
/// created before a store exists lands the moment one does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn records_created_before_the_store_persist_once_it_is_wired() {
    let tmp = fresh_workdir();
    let store_root = tmp.path();
    let kernel = kernel_at(store_root);
    fantastic_kernel::test_support::boot_root(&kernel).await;

    // NO store yet: the record is queued, not written, and NOT dropped.
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), json!({"type":"create_agent","parent_id":"kernel","handler_module":"noop.tools","id":"early"}))])
        .await.remove(0).result;
    fantastic_kernel::test_support::settle_records().await;
    assert!(!store_root.join("agents/early/agent.yaml").exists());

    // Wire one — the queued record drains on the next pass.
    wire_store(&kernel, store_root).await;
    fantastic_kernel::test_support::settle_records().await;
    kernel.flush_records().await;
    assert!(
        store_root.join("agents/early/agent.yaml").exists(),
        "the dirty set must survive a no-store flush"
    );
}

/// C42: `json.dumps(existing, indent=2)` bytes — 2-space indent, NO trailing
/// newline, non-ASCII escaped as lowercase `\uXXXX` (surrogate pairs above the
/// BMP), never raw UTF-8.
#[tokio::test]
async fn persisted_record_bytes_are_python_yaml_dump() {
    let tmp = fresh_workdir();
    let store_root = tmp.path();
    let kernel = kernel_at(store_root);
    fantastic_kernel::test_support::boot_root(&kernel).await;
    wire_store(&kernel, store_root).await;

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "noop.tools", "id": "readings",
                "note": "Sensor readings \u{2014} one integer per file. \u{1F600}",
            }),
        )])
        .await
        .remove(0)
        .result;
    fantastic_kernel::test_support::settle_records().await;

    let bytes = std::fs::read(store_root.join("agents/readings/agent.yaml")).expect("on disk");
    let text = String::from_utf8(bytes).expect("utf-8");
    // python `yaml_io.dump(record)`: block mapping, record key order,
    // BMP text verbatim (`allow_unicode`), non-BMP escaped in a double-quoted
    // scalar, a final newline.
    assert!(text.starts_with("id: readings\n"), "{text}");
    assert!(text.ends_with('\n'), "{text:?}");
    assert!(
        text.contains("\nnote: \"Sensor readings \u{2014} one integer per file. \\U0001F600\"\n"),
        "em dash verbatim, emoji as \\U0001F600: {text}"
    );
}

/// C43: the merged bytes land at a `<af>.tmp.<8 hex>` sibling first, then a
/// `rename` onto `agent.yaml` — never a direct truncating write onto the live
/// name (a crash mid-write must never leave a torn or half-written record).
#[tokio::test]
async fn a_record_commits_via_tmp_and_rename() {
    let tmp = fresh_workdir();
    let store_root = tmp.path();
    let kernel = kernel_at(store_root);
    fantastic_kernel::test_support::boot_root(&kernel).await;
    wire_store(&kernel, store_root).await;

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "noop.tools", "id": "committed",
            }),
        )])
        .await
        .remove(0)
        .result;
    fantastic_kernel::test_support::settle_records().await;

    let dir = store_root.join("agents/committed");
    assert!(dir.join("agent.yaml").exists(), "the final name exists");
    let tmp_debris: Vec<_> = std::fs::read_dir(&dir)
        .expect("dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(
        tmp_debris.is_empty(),
        "no tmp sibling survives a successful commit: {tmp_debris:?}"
    );
}

/// A provider whose `read_stream` always answers a FIXED-SIZE PREFIX of the
/// file, regardless of the file's real size — a stand-in for a real
/// provider's default chunk cap (`file_bridge`'s is 65536 bytes; this test
/// uses a tiny one so the fixture stays small). Counts calls so the test can
/// assert the merge issues exactly ONE.
struct TruncatingStore {
    root: PathBuf,
    cap: usize,
    read_stream_paths: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl Bundle for TruncatingStore {
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
        if verb == "rename" {
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
            let _ = std::fs::rename(&old, &new);
            return Ok(Some(json!({"renamed": true})));
        }
        Ok(Some(
            json!({"error": format!("TruncatingStore: text verb {verb:?}")}),
        ))
    }
    async fn handle_binary(
        &self,
        _id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Vec<u8>>), Box<dyn std::error::Error + Send + Sync>> {
        let verb = header.get("type").and_then(Value::as_str).unwrap_or("");
        let path = header.get("path").and_then(Value::as_str).unwrap_or("");
        let target = self.root.join(path);
        match verb {
            "read_stream" => {
                self.read_stream_paths
                    .lock()
                    .expect("lock")
                    .push(path.to_string());
                match std::fs::read(&target) {
                    Ok(bytes) => {
                        let n = bytes.len().min(self.cap);
                        Ok((Some(json!({"size": n})), Some(bytes[..n].to_vec())))
                    }
                    Err(_) => Ok((Some(json!({"error": "not found"})), None)),
                }
            }
            "write_stream" => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&target, &blob).ok();
                Ok((Some(json!({"written": blob.len()})), None))
            }
            other => Ok((
                Some(json!({"error": format!("TruncatingStore: {other:?}")})),
                None,
            )),
        }
    }
}

/// C44/D6: the merge read is ONE `read_stream` call, mirroring python's
/// `_persist_via_store` exactly — a record bigger than the provider's chunk
/// truncates (an accepted python quirk, not pumped to EOF).
#[tokio::test]
async fn record_merge_reads_one_chunk_like_python() {
    let tmp = fresh_workdir();
    let store_root = tmp.path();
    let mut kernel = Kernel::with_storage(StorageMode::Disk(store_root.to_path_buf()));
    let mut reg = BundleRegistry::new();
    reg.register("noop.tools", NoopBundle);
    let store = Arc::new(TruncatingStore {
        root: store_root.to_path_buf(),
        cap: 40,
        read_stream_paths: std::sync::Mutex::new(Vec::new()),
    });
    // `BundleRegistry::register` takes ownership, so keep a second Arc to read
    // the call counter back after the send.
    reg.register("file_bridge.tools", ArcBundle(Arc::clone(&store)));
    fantastic_kernel::test_support::register_root_loader(&mut reg);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some("kernel_state.tools".to_string()),
        None,
        Map::new(),
        store_root.to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    fantastic_kernel::test_support::boot_root(&kernel).await;
    wire_store(&kernel, store_root).await;

    // Existing on-disk record with a sidecar key that sits PAST the 40-byte
    // cap once the id/handler_module head is written — the merge must not see
    // it, exactly as python's one-chunk read would not.
    let dir = store_root.join("agents/big");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("agent.yaml"),
        format!(
            "{{\n  \"id\": \"big\",\n  \"handler_module\": \"noop.tools\",\n  \"padding\": \"{}\",\n  \"sidecar_beyond_the_cap\": true\n}}",
            "x".repeat(64)
        ),
    )
    .unwrap();

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "noop.tools", "id": "big",
                "display_name": "Big",
            }),
        )])
        .await
        .remove(0)
        .result;
    fantastic_kernel::test_support::settle_records().await;

    let text = std::fs::read_to_string(dir.join("agent.yaml")).expect("on disk");
    assert!(
        !text.contains("sidecar_beyond_the_cap"),
        "a key past the one-chunk read must not survive the merge: {text}"
    );
    let reads_of_big = store
        .read_stream_paths
        .lock()
        .expect("lock")
        .iter()
        .filter(|p| p.as_str() == "agents/big/agent.yaml")
        .count();
    assert_eq!(
        reads_of_big, 1,
        "the merge issues exactly one read_stream call for this record — no offset pump"
    );
}

/// Wraps an `Arc<TruncatingStore>` so the same instance can be registered
/// (the registry needs an owned `Bundle`) while the test keeps its own `Arc`
/// to read `read_stream_paths` back afterward.
struct ArcBundle(Arc<TruncatingStore>);

#[async_trait]
impl Bundle for ArcBundle {
    fn name(&self) -> &str {
        self.0.name()
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        self.0.handle(id, payload, kernel).await
    }
    async fn handle_binary(
        &self,
        id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Vec<u8>>), Box<dyn std::error::Error + Send + Sync>> {
        self.0.handle_binary(id, header, blob, kernel).await
    }
}

/// C45: an operator-EMPTIED readme (present, zero bytes) is never reseeded —
/// only a genuinely ABSENT file (an error reply) triggers the copy.
#[tokio::test]
async fn an_emptied_readme_is_not_reseeded() {
    let tmp = fresh_workdir();
    let store_root = tmp.path();
    let kernel = kernel_at(store_root);
    fantastic_kernel::test_support::boot_root(&kernel).await;
    wire_store(&kernel, store_root).await;

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "readme_bundle.tools", "id": "documented",
            }),
        )])
        .await
        .remove(0)
        .result;
    fantastic_kernel::test_support::settle_records().await;
    let readme_path = store_root.join("agents/documented/readme.md");
    assert_eq!(
        std::fs::read_to_string(&readme_path).expect("seeded"),
        "shipped readme text\n",
        "first boot seeds the shipped text"
    );

    // The operator empties it — python's `_seed_doc_via_store` reads this as
    // PRESENT (`isinstance(b"", bytes)` is true) and must leave it alone.
    std::fs::write(&readme_path, b"").unwrap();
    // Re-dirty the agent (an `update_agent` fires the "updated" state event
    // the debounced writer watches) — `flush_records` re-seeds on every
    // drained write, not only the create, see its own comment.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": "documented", "display_name": "Documented"}),
        )])
        .await
        .remove(0)
        .result;
    fantastic_kernel::test_support::settle_records().await;
    assert_eq!(
        std::fs::read_to_string(&readme_path).expect("still there"),
        "",
        "an operator-emptied readme is NOT reseeded"
    );
}
