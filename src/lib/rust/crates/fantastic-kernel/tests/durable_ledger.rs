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
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
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
    assert_eq!(doc["v"], 2, "the sidecar is version-stamped");
    let rows = doc["rows"].as_array().expect("v2 rows are an array");
    // v2 carries the KERNEL id as an int64 `id` field (an object key would
    // stringify it) and the caller's name beside it — which is what lets that
    // name still join its execution after the restart.
    assert!(
        rows.iter().any(|r| r["name"] == "settled-1"),
        "the settled row is missing"
    );
    // EVERY persisted row carries a terminal outcome — nothing in flight
    // crosses the restart. Work that never completed did not complete.
    for r in rows {
        let status = r["outcome"]["status"].as_str().unwrap_or("");
        assert!(
            ["completed", "errored", "failed", "cancelled", "timed_out"].contains(&status),
            "row {:?} persisted as {status:?} — only settled rows may cross",
            r["id"]
        );
    }
}

/// C46: an `errored` execution — a handler that RAN and said no — survives the
/// reboot. Rust used to drop it on rehydrate, which re-armed exactly the call
/// the ledger exists to never run twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_errored_execution_survives_reboot() {
    const DESK: &str = "desk_err";
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let k1 = kernel_at(&store);
    wire(&k1, &store, DESK).await;
    let out = k1
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "boom"}),
        )
        .with_id("bad-1")])
        .await
        .remove(0);
    assert_eq!(out.status.as_str(), "errored", "{:?}", out.result);
    fantastic_kernel::persistence::flush_ledger(&k1).await;

    let k2 = kernel_at(&store);
    wire(&k2, &store, DESK).await;
    assert!(
        fantastic_kernel::persistence::rehydrate_ledger(&k2).await >= 1,
        "the errored row must come back"
    );
    // Re-sending the SAME name hands back the recorded outcome, not a re-run.
    let again = k2
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "boom"}),
        )
        .with_id("bad-1")])
        .await
        .remove(0);
    assert_eq!(again.status.as_str(), "errored");
}

/// C46: the rehydrate RAISES the row counter past every settled id, so a call
/// the message log never saw cannot have its id re-minted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rehydrate_raises_the_id_counter() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join("ledger.json"),
        r#"{"v": 2, "rows": [{"id": 900, "target": "desk", "retries": 0, "attempts": 1,
            "named": true, "name": "old", "origin": null,
            "outcome": {"status": "completed", "result": {}, "attempts": 1}}]}"#,
    )
    .unwrap();
    let kernel = kernel_at(&store);
    wire(&kernel, &store, "desk").await;
    assert_eq!(
        fantastic_kernel::persistence::rehydrate_ledger(&kernel).await,
        1
    );
    assert!(
        kernel.rows.next_id() > 900,
        "the counter must clear every rehydrated id: {}",
        kernel.rows.next_id()
    );
}

/// C54: a clean rehydrate says NOTHING. Only python's loud lines survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_rehydrate_prints_nothing() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join("ledger.json"),
        r#"{"v": 2, "rows": [{"id": 7, "target": "desk", "retries": 0, "attempts": 1,
            "named": false, "name": null, "origin": null,
            "outcome": {"status": "completed", "result": {}, "attempts": 1}}]}"#,
    )
    .unwrap();
    let kernel = kernel_at(&store);
    wire(&kernel, &store, "desk").await;
    // The proof is the STORE, not a line: one row back, nothing announced.
    assert_eq!(
        fantastic_kernel::persistence::rehydrate_ledger(&kernel).await,
        1
    );
    // Installing the write-through is likewise silent (the `rehydrated N` line
    // is deleted).
    fantastic_kernel::persistence::install_ledger_persistence(&kernel).await;
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

/// THE LOADER'S OWN PERSISTENCE TRAFFIC DOES NOT DIRTY THE LEDGER (python
/// `_is_own_persistence`, 2026-09-21). The flush writes `ledger.json` THROUGH
/// the discovered store, and that write's settle used to reach the kick: settle
/// -> `ledger_flush` task row -> store write -> settle -> kick -> ... forever,
/// on an IDLE kernel. The mechanism, not a number: after a burst of real sends
/// the `ledger_flush` task-row count must STOP GROWING across a quiet window,
/// and a REAL settle after that window must still flush.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_persistence_settles_never_re_kick_the_flusher() {
    const DESK: &str = "desk_ownkick";
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let kernel = kernel_at(&store);
    wire(&kernel, &store, DESK).await;
    fantastic_kernel::persistence::install_ledger_persistence(&kernel).await;

    let flushes = |k: &Arc<Kernel>| {
        k.rows
            .for_agent(&AgentId::from("kernel_state"))
            .iter()
            .filter(|r| r["kind"] == json!("task") && r["verb"] == json!("ledger_flush"))
            .count()
    };

    for _ in 0..5 {
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from(DESK),
                json!({"type": "work"}),
            )])
            .await;
    }
    // Let the burst's flushes drain, then hold the kernel QUIET and measure.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let quiet_start = flushes(&kernel);
    assert!(
        quiet_start > 0,
        "the burst never flushed, so the window proves nothing"
    );
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    assert_eq!(
        flushes(&kernel),
        quiet_start,
        "the flusher re-kicked itself on its OWN persistence settles"
    );

    // A REAL settle still flushes: one more send, and both the task row and the
    // durable snapshot move.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "work"}),
        )])
        .await;
    let mut grew = false;
    for _ in 0..80 {
        if flushes(&kernel) > quiet_start {
            grew = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(grew, "a real settle must still flush the ledger");
    let doc: Value =
        serde_json::from_slice(&std::fs::read(store.join("ledger.json")).unwrap()).unwrap();
    assert!(
        doc["rows"].as_array().is_some_and(|r| !r.is_empty()),
        "the flush wrote an empty ledger: {doc}"
    );
}

/// C14 (RP-08): every flush pass is a REGISTERED kernel task on the root under
/// the verb `ledger_flush` — python writes `<row kind="task"
/// verb="ledger_flush" agent="kernel_state" />` for each one, so the writing is
/// visible in `ps` and in the log. A bare `tokio::spawn` is invisible to both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_flush_pass_is_a_ledger_flush_task() {
    const DESK: &str = "desk_flushrow";
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let kernel = kernel_at(&store);
    wire(&kernel, &store, DESK).await;
    fantastic_kernel::persistence::install_ledger_persistence(&kernel).await;

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "work"}),
        )])
        .await;
    // The flusher's own task row lands on the ROOT under python's verb.
    let mut saw = false;
    for _ in 0..80 {
        saw = kernel
            .rows
            .for_agent(&AgentId::from("kernel_state"))
            .iter()
            .any(|r| r["kind"] == json!("task") && r["verb"] == json!("ledger_flush"));
        if saw {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(saw, "no `ledger_flush` task row on the root");
}

/// C47: PERSISTENCE IO RIDES THE BATCH. Every store write the substrate makes —
/// the ledger flush here — is an ordinary execution: a `call` row on the store
/// agent, a settle, and a ledger entry. `send_with_binary` minted none of them,
/// so the one class of work that is always running left no trace at all.
///
/// The row carries NO `origin` (operator authority), which is exactly the test
/// the message pump uses to skip its own traffic — assert it here so the two
/// halves cannot drift apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_writes_are_ledger_executions() {
    const DESK: &str = "desk_storerows";
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let kernel = kernel_at(&store);
    wire(&kernel, &store, DESK).await;
    fantastic_kernel::persistence::install_ledger_persistence(&kernel).await;

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "work"}),
        )])
        .await;

    let mut writes = Vec::new();
    for _ in 0..80 {
        writes = kernel
            .rows
            .for_agent(&AgentId::from("store"))
            .into_iter()
            .filter(|r| r["kind"] == json!("call") && r["verb"] == json!("write_stream"))
            .collect();
        if !writes.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let row = writes
        .first()
        .expect("a ledger flush must leave a `write_stream` call row on the store");
    assert!(
        row.get("origin").is_none() || row["origin"].is_null(),
        "a substrate write runs as the operator: {row:?}"
    );
    let id = row["id"].as_i64().expect("a call row is named by its id");
    assert!(
        kernel.tasks.contains(&fantastic_kernel::TaskId(id))
            || kernel
                .rows
                .for_agent(&AgentId::from("kernel_state"))
                .iter()
                .any(|r| r["kind"] == json!("settle") && r["parent"] == json!(id)),
        "the write's execution neither ran nor settled"
    );
}
