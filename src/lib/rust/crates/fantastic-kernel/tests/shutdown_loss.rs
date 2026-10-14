//! THE RECORD-WRITER SHUTDOWN LOSS.
//!
//! The record writer's final flush runs in `flush_pending`, before the
//! teardown walk, while the provider lives — inside `on_shutdown` the store
//! would already be gone, and a `create_agent` + `update_agent` followed
//! straight away by a shutdown would never reach disk. (There is no message
//! tape to lose: the kernel flushes its execution rows into its memory store.)

use fantastic_kernel::test_support::{register_fake_store, register_root_loader, wire_fake_store};
use fantastic_kernel::{Agent, AgentId, BundleRegistry, Item, Kernel, StorageMode};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

/// A fresh root in `dir` with the store wired and the record loop running.
async fn root_with_store(dir: &Path) -> Arc<Kernel> {
    let store = dir.join(".fantastic");
    std::fs::create_dir_all(&store).unwrap();
    let mut kernel = Kernel::with_storage(StorageMode::Disk(store.clone()));
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    register_fake_store(&mut reg, &store);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some("kernel_state.tools".to_string()),
        None,
        Map::new(),
        store.clone(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    wire_fake_store(&kernel, &store).await;
    kernel
        .send(vec![Item::new(root.id.clone(), json!({"type": "boot"}))])
        .await;
    kernel
}

/// The record writer's final flush runs in `flush_pending` — before the walk,
/// while the provider is alive — so a create+update immediately followed by a
/// shutdown is on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_record_updated_just_before_shutdown_always_lands() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let kernel = root_with_store(tmp.path()).await;
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "file_bridge.tools", "allow": ["."], "id": "sp_late",
                "root": store.to_string_lossy(),
            }),
        )])
        .await;
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "update_agent", "id": "sp_late",
                "grants": {"can_call": ["store"]},
            }),
        )])
        .await;
    kernel.shutdown().await;

    let f = store.join("agents").join("sp_late").join("agent.yaml");
    assert!(f.exists(), "the record never reached disk");
    let rec: Value =
        fantastic_kernel::yaml_io::load(&std::fs::read_to_string(&f).unwrap()).unwrap();
    assert_eq!(rec["grants"], json!({"can_call": ["store"]}));
}
