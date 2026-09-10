//! #37 — the DURABLE LEDGER, through the REAL persistence path.
//!
//! The unit tests in `ledger/tests.rs` prove the ring, the scoping and the
//! snapshot shape in isolation. This one proves the part that only breaks when
//! the pieces are joined: a settled execution written THROUGH the discovered
//! provider, read back by a FRESH kernel, and deduping a re-send that would
//! otherwise re-run the side effect.
//!
//! Uses `FakeStore` (the same stand-in `persistence_roundtrip` uses — the real
//! `fantastic-file` bundle cannot be a dev-dep here without a cycle).

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Reply, StorageMode};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

/// How many times the handler ACTUALLY ran, PER AGENT — the only honest way to
/// assert "never re-executed".
///
/// Per agent, not one global: the tests in this binary share a process and run
/// concurrently, so a single counter makes them read each other's work.
static RUNS: std::sync::Mutex<Option<std::collections::HashMap<String, usize>>> =
    std::sync::Mutex::new(None);

fn bump(agent: &str) -> usize {
    let mut g = RUNS.lock().expect("runs poisoned");
    let m = g.get_or_insert_with(std::collections::HashMap::new);
    let n = m.entry(agent.to_string()).or_insert(0);
    *n += 1;
    *n
}

fn runs(agent: &str) -> usize {
    RUNS.lock()
        .expect("runs poisoned")
        .as_ref()
        .and_then(|m| m.get(agent).copied())
        .unwrap_or(0)
}

struct CountingBundle;

#[async_trait]
impl Bundle for CountingBundle {
    fn name(&self) -> &str {
        "counting"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Ok(None),
            "work" => {
                let n = bump(id.as_str());
                Ok(Some(json!({ "run": n })))
            }
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
    ) -> Result<(Reply, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        let path = header.get("path").and_then(Value::as_str).unwrap_or("");
        let target = self.root.join(path);
        match header.get("type").and_then(Value::as_str).unwrap_or("") {
            "write_stream" => {
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p).ok();
                }
                std::fs::write(&target, &blob).ok();
                Ok((Some(json!({"written": blob.len()})), Vec::new()))
            }
            "read_stream" => match std::fs::read(&target) {
                // One whole file, `eof: true` — the pump stops on the first pass.
                Ok(b) => Ok((Some(json!({"path": path, "eof": true, "size": b.len()})), b)),
                Err(_) => Ok((
                    Some(json!({"error": format!("no such path {path:?}")})),
                    Vec::new(),
                )),
            },
            other => Ok((
                Some(json!({"error": format!("FakeStore: {other:?}")})),
                Vec::new(),
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
async fn a_completed_execution_is_never_re_run_by_a_fresh_kernel() {
    const DESK: &str = "desk_a";
    // C9's full property: a completed step is not re-executed by a NEW process.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");

    let k1 = kernel_at(&store);
    wire(&k1, &store, DESK).await;
    let first = k1
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "work"}),
        )
        .with_id("job-1")])
        .await
        .remove(0);
    assert!(first.is_completed());
    assert_eq!(first.result["run"], 1);
    assert_eq!(runs(DESK), 1);

    // Write the sidecar THROUGH the provider (what the debounced flusher does).
    fantastic_kernel::persistence::flush_ledger(&k1).await;
    assert!(
        store.join("ledger.json").exists(),
        "the sidecar never reached the store"
    );
    drop(k1);

    // A FRESH kernel — a new process, as far as the ledger is concerned.
    let k2 = kernel_at(&store);
    wire(&k2, &store, DESK).await;
    // LEDGER-ALL: every execution is a row, so the operator's own wiring sends
    // are in there too. What matters is that the NAMED one came back.
    let n = fantastic_kernel::persistence::rehydrate_ledger(&k2).await;
    assert!(n >= 1, "nothing rehydrated");

    let again = k2
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "work"}),
        )
        .with_id("job-1")])
        .await
        .remove(0);
    assert_eq!(runs(DESK), 1, "a REBOOT re-executed a completed step");
    assert_eq!(again.result["run"], 1, "the recorded outcome must return");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_settled_rows_cross_the_restart() {
    const DESK: &str = "desk_b";
    // Work in flight at a crash never completed, so a post-reboot re-send
    // re-running it is the TRUTH, not a loss.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");

    let k1 = kernel_at(&store);
    wire(&k1, &store, DESK).await;
    let _ = k1
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "work"}),
        )
        .with_id("settled-1")])
        .await;
    fantastic_kernel::persistence::flush_ledger(&k1).await;

    let doc: Value =
        serde_json::from_slice(&std::fs::read(store.join("ledger.json")).unwrap()).unwrap();
    assert_eq!(doc["v"], 1, "the sidecar is version-stamped");
    let rows = doc["rows"].as_object().expect("rows object");
    // The sidecar keys by the KERNEL id and carries the caller's name as a
    // field — which is what lets that name still join its execution after the
    // restart.
    assert!(
        rows.values().any(|r| r["name"] == "settled-1"),
        "the settled row is missing"
    );
    // EVERY persisted row carries a terminal outcome — nothing in flight
    // crosses the restart. Work that never completed did not complete.
    for (id, r) in rows {
        let status = r["outcome"]["status"].as_str().unwrap_or("");
        assert!(
            ["completed", "failed", "cancelled", "timed_out"].contains(&status),
            "row {id:?} persisted as {status:?} — only settled rows may cross"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_store_wired_means_ram_only_and_no_error() {
    const DESK: &str = "desk_c";
    // The AUTO-FLUSH LAW: with nothing wired the tree does not persist either.
    // The ledger follows the same rule rather than inventing a second surface.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");

    let kernel = kernel_at(&store);
    // NO store agent — only the worker.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "counting.tools", "id": DESK,
            }),
        )])
        .await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "work"}),
        )
        .with_id("ram-1")])
        .await;

    // Does not panic, does not write, does not complain.
    fantastic_kernel::persistence::flush_ledger(&kernel).await;
    assert!(!store.join("ledger.json").exists());
    assert_eq!(
        fantastic_kernel::persistence::rehydrate_ledger(&kernel).await,
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupt_sidecar_starts_empty_instead_of_half_loading() {
    const DESK: &str = "desk_d";
    // A ledger that silently half-loads would re-arm exactly the side effects it
    // exists to prevent, so every failure is LOUD and starts empty.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&store).unwrap();

    let kernel = kernel_at(&store);
    wire(&kernel, &store, DESK).await;

    std::fs::write(store.join("ledger.json"), b"{\"v\": 1, \"rows\": {broken").unwrap();
    assert_eq!(
        fantastic_kernel::persistence::rehydrate_ledger(&kernel).await,
        0
    );

    // An unknown version is not a migration we can guess at.
    std::fs::write(
        store.join("ledger.json"),
        br#"{"v": 99, "rows": {"x": {}}}"#,
    )
    .unwrap();
    assert_eq!(
        fantastic_kernel::persistence::rehydrate_ledger(&kernel).await,
        0
    );

    // ...and the kernel is still perfectly usable.
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "work"}),
        )])
        .await
        .remove(0);
    assert!(out.is_completed(), "a bad sidecar bricked the kernel");
}
