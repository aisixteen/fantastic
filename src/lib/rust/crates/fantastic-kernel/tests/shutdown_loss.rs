//! THE TWO SHUTDOWN LOSSES (2026-09-24) — twins of python
//! `tests/kernel/test_message_persistence.py`'s repros.
//!
//! Both writers behind the root loader debounce, and both used to close on the
//! wrong hook:
//!
//! 1. THE PUMP. A shutdown landing ON the debounce boundary stopped the flush
//!    loop mid-append and the batch went with it. The flush holds a lock the
//!    catch-up waits on now, and a dropped flush re-queues its rows.
//! 2. THE RECORD WRITER. Its final flush ran inside `on_shutdown` — mid-
//!    teardown-walk, with the store already gone — so a `create_agent` +
//!    `update_agent` followed straight away by a shutdown never reached disk.
//!    It runs in `flush_pending` now, before the walk, while the provider lives.

use fantastic_kernel::test_support::{register_fake_store, register_root_loader, wire_fake_store};
use fantastic_kernel::{Agent, AgentId, BundleRegistry, Item, Kernel, StorageMode};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

/// A fresh root in `dir` with the store wired and both loops running.
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

fn logged_ids(store: &Path) -> Vec<i64> {
    let text = std::fs::read_to_string(store.join("messages.xml")).unwrap_or_default();
    fantastic_kernel::messages_xml::parse_messages(&text)
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .collect()
}

/// A shutdown that lands ON the debounce boundary used to stop the flush loop
/// mid-append and drop the batch with it. SWEEP the boundary — the loss window
/// is the handful of ms the flush spends inside the store's `write_stream`, and
/// a fixed sleep either always misses it or always hits it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_row_put_just_before_shutdown_always_lands() {
    for run in 0..20u64 {
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join(".fantastic");
        let kernel = root_with_store(tmp.path()).await;
        let rid = kernel
            .rows
            .put(
                &AgentId::from("ai"),
                json!({"role": "user", "content": format!("row {run}")})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_micros(150_000 + run * 500)).await;
        kernel.shutdown().await;
        assert!(
            logged_ids(&store).contains(&rid),
            "run {run}: the last row before shutdown never reached the log"
        );
    }
}

/// The record writer's final flush used to run mid-teardown-walk, with the store
/// already gone. It runs in `flush_pending` now — before the walk, while the
/// provider is alive — so a create+update immediately followed by a shutdown is
/// on disk.
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
                "handler_module": "file_bridge.tools", "id": "sp_late",
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

    let f = store.join("agents").join("sp_late").join("agent.json");
    assert!(f.exists(), "the record never reached disk");
    let rec: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
    assert_eq!(rec["grants"], json!({"can_call": ["store"]}));
}
