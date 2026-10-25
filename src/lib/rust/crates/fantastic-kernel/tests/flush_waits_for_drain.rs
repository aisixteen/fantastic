//! ONE RECORD DRAIN AT A TIME.
//!
//! `flush_records` must not just take the dirty queues and return: when the
//! debounced writer's own drain had already taken them, the flush would find
//! them empty and return AT ONCE — with that drain's record write still in
//! flight, so a caller would not see `agent.yaml` on disk.
//!
//! The race is pinned here without a timing window: the store HOLDS the probe's
//! record write until the test opens its gate, so the writer's drain is provably
//! mid-write when the flush is called.

use async_trait::async_trait;
use fantastic_kernel::test_support::{boot_root, register_root_loader, wire_fake_store, FakeStore};
use fantastic_kernel::StorageMode;
use fantastic_kernel::{Agent, AgentId, Blob, Bundle, BundleRegistry, Item, Kernel, Reply};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::{Notify, Semaphore};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A bundle that ships a readme — none is copied beside its record.
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
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BoxError> {
        Ok(None)
    }
    fn readme(&self) -> Option<&'static str> {
        Some("shipped readme text\n")
    }
}

/// The fake provider, with the probe's record write HELD: it announces the
/// write on `entered`, then waits until `gate` is closed.
struct GatedStore {
    inner: FakeStore,
    gate: Arc<Semaphore>,
    entered: Arc<Notify>,
}

#[async_trait]
impl Bundle for GatedStore {
    fn name(&self) -> &str {
        "file_bridge"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BoxError> {
        self.inner.handle(id, payload, kernel).await
    }
    async fn handle_binary(
        &self,
        id: &AgentId,
        header: Value,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BoxError> {
        let probe_write = header.get("type").and_then(Value::as_str) == Some("write_stream")
            && header
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|p| p.starts_with("agents/probe/agent.yaml"));
        if probe_write {
            self.entered.notify_one();
            // closed ⇒ `Err` at once: the gate is open
            let _ = self.gate.acquire().await;
        }
        self.inner.handle_binary(id, header, body, kernel).await
    }
}

fn kernel_at(store: &Path, gate: &Arc<Semaphore>, entered: &Arc<Notify>) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(StorageMode::disk(store.to_path_buf()));
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register("readme_bundle.tools", ReadmeBundle);
    reg.register(
        "file_bridge.tools",
        GatedStore {
            inner: FakeStore {
                root: store.to_path_buf(),
            },
            gate: Arc::clone(gate),
            entered: Arc::clone(entered),
        },
    );
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some("kernel_state.tools".to_string()),
        None,
        Map::new(),
        store.to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

/// `flush_records` returns once everything dirty has LANDED — also what the
/// debounced writer took first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flush_waits_for_the_drain_the_writer_already_took() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&store).unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let entered = Arc::new(Notify::new());
    let kernel = kernel_at(&store, &gate, &entered);
    boot_root(&kernel).await;
    wire_fake_store(&kernel, &store).await;
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "kernel_state",
                   "handler_module": "readme_bundle.tools", "id": "probe"}),
        )])
        .await;

    // The debounced writer's drain has taken the probe and is mid-write.
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the writer's drain never wrote the probe's record");

    let agent_dir = store.join("agents").join("probe");
    let k = Arc::clone(&kernel);
    let dir = agent_dir.clone();
    let flush = tokio::spawn(async move {
        k.flush_records().await;
        (
            dir.join("agent.yaml").exists(),
            dir.join("readme.md").exists(),
        )
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let returned_early = flush.is_finished();
    gate.close();
    let (record, readme) = tokio::time::timeout(Duration::from_secs(10), flush)
        .await
        .expect("the flush never returned")
        .expect("the flush task panicked");
    assert!(
        !returned_early,
        "flush_records returned while the writer's drain still held the record write"
    );
    assert!(record, "flush_records returned before agent.yaml landed");
    assert!(!readme, "a record write copies no readme.md");
    kernel.shutdown().await;
}
