//! A `removed` forgets only a dir the record writer may have written
//! (python `kernel_state._FlushLoop._unwritten`).
//!
//! An agent created and deleted between two drains has nothing on disk: the
//! drain sends its store no `delete` for it (a real `file_bridge` answers a
//! missing path with `does not exist`, logged as `persist through … failed`).
//! An agent whose record a drain wrote is still forgotten.

use async_trait::async_trait;
use fantastic_kernel::test_support::{boot_root, register_root_loader, wire_fake_store, FakeStore};
use fantastic_kernel::StorageMode;
use fantastic_kernel::{Agent, AgentId, Blob, Bundle, BundleRegistry, Item, Kernel, Reply};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The fake provider, recording every `delete` path it is sent.
struct DeleteLog {
    inner: FakeStore,
    deletes: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Bundle for DeleteLog {
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
        if header.get("type").and_then(Value::as_str) == Some("delete") {
            let path = header.get("path").and_then(Value::as_str).unwrap_or("");
            self.deletes.lock().unwrap().push(path.to_string());
        }
        self.inner.handle_binary(id, header, body, kernel).await
    }
}

struct Plain;

#[async_trait]
impl Bundle for Plain {
    fn name(&self) -> &str {
        "plain"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BoxError> {
        Ok(None)
    }
}

fn kernel_at(store: &Path, deletes: &Arc<Mutex<Vec<String>>>) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(StorageMode::Disk(store.to_path_buf()));
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register("plain.tools", Plain);
    reg.register(
        "file_bridge.tools",
        DeleteLog {
            inner: FakeStore {
                root: store.to_path_buf(),
            },
            deletes: Arc::clone(deletes),
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

async fn kernel_verb(kernel: &Arc<Kernel>, payload: Value) {
    let out = kernel
        .send(vec![Item::new(AgentId::from("kernel"), payload.clone())])
        .await;
    assert!(
        out[0].result.get("error").is_none(),
        "{payload}: {:?}",
        out[0].result
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_added_and_removed_between_drains_forgets_nothing() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&store).unwrap();
    let deletes: Arc<Mutex<Vec<String>>> = Arc::default();
    let kernel = kernel_at(&store, &deletes);
    boot_root(&kernel).await;
    wire_fake_store(&kernel, &store).await;
    kernel.flush_records().await;

    // Written by a drain, then deleted: its dir is forgotten.
    kernel_verb(
        &kernel,
        json!({"type": "create_agent", "parent_id": "kernel_state",
               "handler_module": "plain.tools", "id": "kept"}),
    )
    .await;
    kernel.flush_records().await;
    assert!(store.join("agents/kept/agent.yaml").exists());
    kernel_verb(&kernel, json!({"type": "delete_agent", "id": "kept"})).await;

    // Created (with a child) and deleted before any drain: nothing to forget.
    kernel_verb(
        &kernel,
        json!({"type": "create_agent", "parent_id": "kernel_state",
               "handler_module": "plain.tools", "id": "brief"}),
    )
    .await;
    kernel_verb(
        &kernel,
        json!({"type": "create_agent", "parent_id": "brief",
               "handler_module": "plain.tools", "id": "inner"}),
    )
    .await;
    kernel_verb(&kernel, json!({"type": "delete_agent", "id": "brief"})).await;
    kernel.flush_records().await;

    assert_eq!(*deletes.lock().unwrap(), vec!["agents/kept".to_string()]);
    assert!(!store.join("agents/kept").exists());
    assert!(!store.join("agents/brief").exists());
    assert_eq!(fantastic_kernel::persistence::persist_error(&kernel), None);
    kernel.shutdown().await;
}
