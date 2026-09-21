//! #10 — the BLOB CHANNEL, through the REAL persistence path.
//!
//! The unit tests in `persistence/blob/tests.rs` prove the name and owner
//! guards in isolation. This one proves the part that only breaks when the
//! pieces are joined: a consumer that names ONLY itself and a filename, and a
//! sidecar that lands where the loader decided — beside the agent's own record.
//!
//! Uses `FakeStore` (the same stand-in `persistence_roundtrip` uses — the real
//! `fantastic-file` bundle cannot be a dev-dep here without a cycle).

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Reply, StorageMode};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

struct CountingBundle;

#[async_trait]
impl Bundle for CountingBundle {
    fn name(&self) -> &str {
        "counting"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Ok(None),
            "work" => Ok(Some(json!({ "ok": true }))),
            "boom" => Ok(Some(json!({"error": "always fails"}))),
            other => Ok(Some(json!({ "error": format!("unknown verb {other:?}") }))),
        }
    }
}

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
        Ok(Some(json!({"ok": true, "verb": payload.get("type")})))
    }
    async fn handle_binary(
        &self,
        _id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Vec<u8>>), Box<dyn std::error::Error + Send + Sync>> {
        let path = header.get("path").and_then(Value::as_str).unwrap_or("");
        let target = self.root.join(path);
        match header.get("type").and_then(Value::as_str).unwrap_or("") {
            "write_stream" => {
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p).ok();
                }
                std::fs::write(&target, &blob).ok();
                Ok((Some(json!({"written": blob.len()})), None))
            }
            "read_stream" => match std::fs::read(&target) {
                // One whole file, `eof: true` — the pump stops on the first pass.
                Ok(b) => Ok((
                    Some(json!({"path": path, "eof": true, "size": b.len()})),
                    Some(b),
                )),
                Err(_) => Ok((
                    Some(json!({"error": format!("no such path {path:?}")})),
                    None,
                )),
            },
            other => Ok((
                Some(json!({"error": format!("FakeStore: {other:?}")})),
                None,
            )),
        }
    }
}

fn kernel_at(store_root: &Path) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(StorageMode::Disk(store_root.to_path_buf()));
    let mut reg = BundleRegistry::new();
    reg.register("counting.tools", CountingBundle);
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

/// Wire the store + a counting agent, exactly as an operator would.
async fn wire(kernel: &Arc<Kernel>, store_root: &Path, desk: &str) {
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "file_bridge.tools",
                "id": "store",
                "root": store_root.to_string_lossy(),
                "ingress_rule": "allow_all_ingress",
            }),
        )])
        .await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "counting.tools", "id": desk,
            }),
        )])
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sidecar_round_trips_and_lands_beside_the_record() {
    const DESK: &str = "desk_blob";
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");

    let kernel = kernel_at(&store);
    wire(&kernel, &store, DESK).await;

    let w = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "persist_blob", "agent_id": DESK,
                "name": "state.yaml", "content": "k: v\n",
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(w["ok"], true, "{w}");

    // The CONSUMER never computed this path — the loader did.
    let on_disk = store.join("agents").join(DESK).join("state.yaml");
    assert!(on_disk.exists(), "sidecar not at {on_disk:?}");
    assert_eq!(std::fs::read_to_string(&on_disk).unwrap(), "k: v\n");

    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "load_blob", "agent_id": DESK, "name": "state.yaml"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["content"], "k: v\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_never_written_sidecar_reads_empty_not_error() {
    const DESK: &str = "desk_blob_absent";
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let kernel = kernel_at(&store);
    wire(&kernel, &store, DESK).await;

    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "load_blob", "agent_id": DESK, "name": "nothing.yaml"}),
        )])
        .await
        .remove(0)
        .result;
    // "nothing saved yet" is the normal FIRST state, not a failure.
    assert!(r["content"].is_null(), "{r}");
    assert!(r.get("error").is_none(), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_cannot_write_another_agents_namespace() {
    // THE CONFUSED DEPUTY. `agent_id` arrives in the PAYLOAD, so without the
    // owner guard any agent granted `persist_blob` could forge another agent's
    // memory — measured in python, where the victim read the forged content
    // back as its own.
    const DESK: &str = "desk_blob_guard";
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let kernel = kernel_at(&store);
    wire(&kernel, &store, DESK).await;

    let forged = fantastic_kernel::send::with_sender(AgentId::from("attacker"), async {
        kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({
                    "type": "persist_blob", "agent_id": DESK,
                    "name": "state.yaml", "content": "owned: true\n",
                }),
            )])
            .await
            .remove(0)
            .result
    })
    .await;

    assert_eq!(forged["reason"], "unauthorized", "{forged}");
    assert!(
        !store.join("agents").join(DESK).join("state.yaml").exists(),
        "the forged sidecar was written"
    );
}
