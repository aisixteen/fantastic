//! ONE TAPE — the durable execution ledger IS `messages.xml`.
//!
//! `ledger.json` is gone (2026-09-24). A `call` row carries the dispatch payload
//! verbatim, its retry budget and its attempt count; the `settle` row on the ROOT
//! carries the WHOLE outcome body and the final attempts. At boot the loader
//! rebuilds `executions` from those two rows, so dedup ("a completed step is
//! never re-executed") holds across a restart out of ONE file.
//!
//! The unit tests in `ledger/tests.rs` prove the ring, the scoping and the
//! rebuild in isolation. This one proves the part that only breaks when the
//! pieces are joined: rows written THROUGH the discovered provider by the pump,
//! read back by a FRESH kernel, and deduping a re-send that would otherwise
//! re-run the side effect. Twin of python
//! `tests/kernel/test_execution_ledger.py`'s persistence cases.

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
            "slow" => {
                bump(id.as_str());
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(Some(json!({"never": true})))
            }
            "boom" => Ok(Some(json!({"error": "always fails"}))),
            other => Ok(Some(json!({ "error": format!("unknown verb {other:?}") }))),
        }
    }
    /// RAW BYTES BOTH WAYS — the tape's one reduction, in and out. The reply
    /// keeps `bytes` at `null` in its python position (C37) and hands the body
    /// back on the binary channel.
    async fn handle_binary(
        &self,
        _id: &AgentId,
        _header: Value,
        blob: Vec<u8>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Vec<u8>>), Box<dyn std::error::Error + Send + Sync>> {
        Ok((Some(json!({"note": "verbatim", "bytes": null})), Some(blob)))
    }
}

/// A store that APPENDS, because the log is append-only — `write_stream` with no
/// `truncate` continues the file, which is exactly what the pump relies on.
struct AppendStore {
    root: PathBuf,
}

#[async_trait]
impl Bundle for AppendStore {
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
                use std::io::Write;
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p).ok();
                }
                let truncate = header.get("truncate").and_then(Value::as_bool) == Some(true);
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(!truncate)
                    .truncate(truncate)
                    .open(&target)?;
                f.write_all(&blob)?;
                Ok((Some(json!({"written": blob.len()})), None))
            }
            "read_stream" => match std::fs::read(&target) {
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
                Some(json!({"error": format!("AppendStore: {other:?}")})),
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
        AppendStore {
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

/// A live kernel with the store wired AND the pump running — the tape is the
/// only durable record now, so a restart test has to actually write it. The pump
/// handle stays with the caller, which is what a `_reboot` needs to land the
/// last window (the loader owns it in production).
async fn boot_with_tape(
    store: &Path,
    desk: &str,
) -> (Arc<Kernel>, fantastic_kernel::messages_log::MessagePump) {
    let k = kernel_at(store);
    // COLD HYDRATION FIRST, before any agent exists or any id is minted — the
    // bootstrap's order, and the only way the counter continues correctly.
    let after = match fantastic_kernel::messages_log::hydrate_messages(&k, store) {
        Ok(_) => k.rows.next_id() - 1,
        Err(e) => panic!("hydrate: {e}"),
    };
    wire(&k, store, desk).await;
    // THE TAPE IS THE LEDGER: the settled calls become `executions` again.
    fantastic_kernel::messages_log::hydrate_executions(&k);
    let pump = fantastic_kernel::messages_log::install_message_pump(&k, after);
    (k, pump)
}

/// Land everything, drop the process, come back up off `messages.xml`.
async fn reboot(
    k1: Arc<Kernel>,
    pump: fantastic_kernel::messages_log::MessagePump,
    store: &Path,
    desk: &str,
) -> (Arc<Kernel>, fantastic_kernel::messages_log::MessagePump) {
    pump.stop(&k1).await;
    drop(k1);
    boot_with_tape(store, desk).await
}

/// Every row on the tape, parsed back off disk.
fn tape(store: &Path) -> Vec<fantastic_kernel::Row> {
    let text = std::fs::read_to_string(store.join("messages.xml")).unwrap_or_default();
    fantastic_kernel::messages_xml::parse_messages(&text).expect("the tape parses")
}

async fn ledger_rows(kernel: &Arc<Kernel>) -> Vec<Value> {
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ledger"}),
        )])
        .await
        .remove(0);
    out.result["executions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_completed_execution_is_never_re_run_by_a_fresh_kernel() {
    const DESK: &str = "desk_a";
    // C9's full property: a completed step is not re-executed by a NEW process.
    // The settled call rebuilds from its `call` + `settle` rows with its
    // outcome; a re-send of the NAME returns it and the handler does not run.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");

    let (k1, p1) = boot_with_tape(&store, DESK).await;
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

    let (k2, p2) = reboot(k1, p1, &store, DESK).await;
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
    assert_eq!(again.attempts, 1);
    p2.stop(&k2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tape_carries_the_payload_and_the_full_outcome() {
    const DESK: &str = "desk_tape";
    // ONE TAPE. `messages.xml` holds the call WITH its dispatch payload and the
    // settle WITH the whole result body — no second file, nothing RAM-only.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");

    let (k1, p1) = boot_with_tape(&store, DESK).await;
    let out = k1
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "work"}),
        )
        .with_id("tape-1")])
        .await
        .remove(0);
    let (k2, p2) = reboot(k1, p1, &store, DESK).await;

    let rows = tape(&store);
    let call = rows
        .iter()
        .find(|r| r.get("name") == Some(&json!("tape-1")))
        .expect("the call row is on the tape");
    assert_eq!(call["payload"], json!({"type": "work"}));
    assert_eq!(call["named"], json!(true));
    assert_eq!(call["retries"], json!(0));
    assert_eq!(call["attempts"], json!(1));
    let settle = rows
        .iter()
        .find(|r| r.get("parent") == call.get("id") && r.get("kind") == Some(&json!("settle")))
        .expect("the settle row is on the tape");
    assert_eq!(
        settle["outcome"],
        json!({"status": "completed", "result": out.result}),
    );
    assert_eq!(settle["attempts"], json!(1));
    assert_eq!(settle["settled"], DESK);

    // ...and the `ledger` verb answers the FULL outcome after the restart.
    let led = ledger_rows(&k2).await;
    let row = led
        .iter()
        .find(|r| r.get("name") == Some(&json!("tape-1")))
        .expect("the rebuilt execution is in the ledger");
    assert_eq!(row["outcome"]["result"], out.result);
    assert_eq!(row["status"], "completed");
    p2.stop(&k2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_bytes_on_the_tape_are_a_length_not_base64() {
    const DESK: &str = "desk_blob";
    // The ONE reduction: a raw body anywhere in a payload or a result is written
    // as its LENGTH. Base64 is banned; text and JSON stay verbatim.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");

    let (k1, p1) = boot_with_tape(&store, DESK).await;
    let out = k1
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "write_stream", "path": "probe.bin"}),
        )
        .with_id("bin-1")
        .with_binary(b"\x00\x01\x02\x03\x04".to_vec())])
        .await
        .remove(0);
    assert!(out.is_completed(), "{:?}", out.result);
    let (k2, p2) = reboot(k1, p1, &store, DESK).await;

    let rows = tape(&store);
    let call = rows
        .iter()
        .find(|r| r.get("name") == Some(&json!("bin-1")))
        .expect("the call row is on the tape");
    assert_eq!(call["payload"]["bytes"], json!({"len": 5}));
    // everything else verbatim
    assert_eq!(call["payload"]["path"], "probe.bin");
    let settle = rows
        .iter()
        .find(|r| r.get("parent") == call.get("id") && r.get("kind") == Some(&json!("settle")))
        .expect("the settle row is on the tape");
    assert_eq!(
        settle["outcome"]["result"],
        json!({"note": "verbatim", "bytes": {"len": 5}}),
    );
    // NEVER THE BYTES, and never base64 of them.
    let text = std::fs::read_to_string(store.join("messages.xml")).unwrap();
    assert!(!text.contains("AAECAwQ="), "base64 reached the tape");
    p2.stop(&k2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_letter_survives_reboot_and_stays_dead() {
    const DESK: &str = "desk_dead";
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");

    let (k1, p1) = boot_with_tape(&store, DESK).await;
    let dead = k1
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "boom"}),
        )
        .with_id("bad-1")])
        .await
        .remove(0);
    assert_eq!(dead.status.as_str(), "errored", "{:?}", dead.result);

    let (k2, p2) = reboot(k1, p1, &store, DESK).await;
    let led = ledger_rows(&k2).await;
    let row = led
        .iter()
        .find(|r| r.get("name") == Some(&json!("bad-1")))
        .expect("the dead letter must come back");
    assert_eq!(row["status"], "errored");
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
    p2.stop(&k2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_flight_work_does_not_survive_a_crash() {
    const DESK: &str = "desk_crash";
    // Only SETTLED calls rebuild an execution. Work in flight at the crash never
    // completed; a re-send after reboot RE-EXECUTING it is the truth, not a
    // loss. The call row is on the tape either way — with no settle after it.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");

    let (k1, p1) = boot_with_tape(&store, DESK).await;
    let k = Arc::clone(&k1);
    let batch = tokio::spawn(async move {
        k.send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "slow"}),
        )
        .with_id("mid-1")])
            .await
    });
    for _ in 0..200 {
        if runs(DESK) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(runs(DESK), 1, "the slow handler never started");
    // THE "CRASH" happens with mid-1 unsettled: the window lands, the process
    // goes.
    p1.catch_up(&k1).await;
    batch.abort();
    let rows = tape(&store);
    let call = rows
        .iter()
        .find(|r| r.get("name") == Some(&json!("mid-1")))
        .expect("the call row is written at dispatch, settled or not");
    assert!(
        !rows
            .iter()
            .any(|r| r.get("parent") == call.get("id") && r.get("kind") == Some(&json!("settle"))),
        "an unsettled call must have no settle row"
    );
    drop(k1);

    let (k2, p2) = boot_with_tape(&store, DESK).await;
    let led = ledger_rows(&k2).await;
    assert!(
        led.iter().all(|r| r.get("name") != Some(&json!("mid-1"))),
        "in-flight work crossed the restart: {led:?}"
    );
    p2.stop(&k2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_store_wired_means_ram_only_and_no_error() {
    const DESK: &str = "desk_ram";
    // The AUTO-FLUSH LAW: with nothing wired the tree does not persist either.
    // The tape follows the same rule rather than inventing a second surface.
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
    let pump = fantastic_kernel::messages_log::install_message_pump(&kernel, 0);
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(DESK),
            json!({"type": "work"}),
        )
        .with_id("ram-1")])
        .await
        .remove(0);
    assert!(out.is_completed());
    pump.stop(&kernel).await;
    assert!(
        !store.join("messages.xml").exists(),
        "a tape was written with no store wired"
    );
    assert_eq!(
        fantastic_kernel::messages_log::hydrate_executions(&kernel),
        0,
        "nothing to rebuild, and no error saying so"
    );
}
