//! The DURABLE MESSAGE LOG through the real path: work, tail, append, restart.
//!
//! `messages_xml/tests.rs` proves the format and `messages_log/tests.rs` proves
//! cold hydration. This one proves the part that only breaks when the pieces are
//! joined — rows written THROUGH the discovered store by the pump, read back by
//! a FRESH kernel, with the counter continuing above everything it read.
//!
//! Miniature of python's `e2e/tests/coding/E78_kernel_restart`.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Kernel, Reply, StorageMode};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

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
    ) -> Result<(Reply, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        let path = header.get("path").and_then(Value::as_str).unwrap_or("");
        let target = self.root.join(path);
        match header.get("type").and_then(Value::as_str).unwrap_or("") {
            "write_stream" => {
                use std::io::Write;
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p).ok();
                }
                let truncate = header
                    .get("truncate")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(!truncate)
                    .truncate(truncate)
                    .open(&target)?;
                f.write_all(&blob)?;
                Ok((Some(json!({"written": blob.len()})), Vec::new()))
            }
            "read_stream" => match std::fs::read(&target) {
                Ok(b) => Ok((Some(json!({"path": path, "eof": true, "size": b.len()})), b)),
                Err(_) => Ok((
                    Some(json!({"error": format!("no such path {path:?}")})),
                    Vec::new(),
                )),
            },
            other => Ok((
                Some(json!({"error": format!("AppendStore: {other:?}")})),
                Vec::new(),
            )),
        }
    }
}

fn kernel_at(store_root: &Path) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(StorageMode::Disk(store_root.to_path_buf()));
    let mut reg = BundleRegistry::new();
    reg.register(
        "file_bridge.tools",
        AppendStore {
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
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

async fn wire_store(kernel: &Arc<Kernel>, store_root: &Path) {
    let _ = kernel
        .send(vec![fantastic_kernel::Item::new(
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
}

fn turn(role: &str, content: &str) -> fantastic_kernel::Row {
    json!({"kind": "turn", "role": role, "content": content})
        .as_object()
        .unwrap()
        .clone()
}

/// Wait for the log to hold `n` rows — the pump debounces, so a fixed sleep is
/// either flaky or slow.
async fn until_rows(path: &Path, n: usize) -> String {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.matches("<row ").count() + text.matches("<row>").count() >= n {
                return text;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rows_survive_a_restart_and_the_counter_continues() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let log = store.join("messages.xml");

    // ── life 1: work, tailed and appended ──────────────────────────────
    let k1 = kernel_at(&store);
    wire_store(&k1, &store).await;
    fantastic_kernel::messages_log::install_message_pump(&k1, 0);

    let ai = AgentId::from("ai");
    k1.rows.put(&ai, turn("user", "what is the code?")).unwrap();
    let pid = k1.rows.mint_id();
    k1.rows
        .put(&ai, turn("assistant", "reading it now"))
        .unwrap();
    k1.rows.body_put(&ai, pid, "the long reply body").unwrap();
    k1.rows
        .view_put(
            &ai,
            json!({"budget": 12000, "ids": [{"id": 1, "res": "full"}]})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
    // The AI's OWN rows — what life 2 must find under that id again. The store
    // holds more than these: a dispatch settles with a `settle` row on the ROOT
    // (the ending the log would otherwise never record at all), so the log's
    // count and this agent's count are no longer the same number.
    let live = k1.rows.for_agent(&ai);
    let next_before = k1.rows.next_id();

    let text = until_rows(&log, live.len()).await;
    assert!(
        text.starts_with("<messages v=\"1\">\n"),
        "no root: {text:.60}"
    );
    assert_eq!(
        text.matches("<messages").count(),
        1,
        "the root is written ONCE and never closed"
    );
    assert!(!text.contains("</messages>"), "the root must stay open");

    // ── life 2: a FRESH kernel on the same dir ─────────────────────────
    let k2 = kernel_at(&store);
    let n = fantastic_kernel::messages_log::hydrate_messages(&k2, &store).expect("hydrates");
    // EVERY row the log wrote comes back — its own `<row` count is the truth,
    // not one agent's slice of it.
    assert_eq!(n, text.matches("<row").count(), "every row came back");
    assert!(
        n >= live.len(),
        "at least the AI's own rows: {n} < {}",
        live.len()
    );

    // THE COUNTER CONTINUES above everything it read — a fresh mint can never
    // collide with a hydrated id.
    assert_eq!(k2.rows.next_id(), next_before);
    assert!(k2.rows.mint_id() >= next_before);

    // Each agent's rows are ITS OWN, in order, with their content intact.
    let back = k2.rows.for_agent(&ai);
    assert_eq!(back.len(), live.len());
    assert_eq!(back[0]["content"], "what is the code?");
    assert_eq!(back[0]["role"], "user");

    // A dehydrated body still PAGES by its parent's pid.
    assert_eq!(
        k2.rows.body_get(pid, 0, 3),
        Some(("the".to_string(), 3, false, 19))
    );
    // …and the view is back and resolves.
    let view = k2.rows.view_get(&ai).expect("the view");
    assert_eq!(view["budget"], json!(12000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_log_refuses_to_boot_rather_than_hydrating_half_a_conversation() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let log = store.join("messages.xml");

    let k1 = kernel_at(&store);
    wire_store(&k1, &store).await;
    fantastic_kernel::messages_log::install_message_pump(&k1, 0);
    let ai = AgentId::from("ai");
    k1.rows.put(&ai, turn("user", "first")).unwrap();
    k1.rows.put(&ai, turn("assistant", "second")).unwrap();
    let text = until_rows(&log, 2).await;

    // A crash mid-append: the last row opened and never closed.
    std::fs::write(&log, &text[..text.len() - 12]).unwrap();
    let k2 = kernel_at(&store);
    let err = fantastic_kernel::messages_log::hydrate_messages(&k2, &store)
        .expect_err("a cut log must be loud");
    assert!(err.contains("never closed"), "{err}");
    assert!(err.contains("nothing is repaired automatically"), "{err}");
    // NOTHING was hydrated: half a conversation is worse than none.
    assert!(k2.rows.for_agent(&ai).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_store_wired_keeps_the_log_in_ram_and_says_so_once() {
    // The same law records live under. A kernel with nothing wired still works;
    // it just does not persist, and it does not pretend to.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let k = kernel_at(&store);
    fantastic_kernel::messages_log::install_message_pump(&k, 0);
    let ai = AgentId::from("ai");
    k.rows.put(&ai, turn("user", "into the void")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        !store.join("messages.xml").exists(),
        "a log was written with no store wired"
    );
    assert_eq!(k.rows.for_agent(&ai).len(), 1, "the row is still in RAM");
}

// ── C36: what the pump writes, and what it never writes ────────────────

/// A bundle that answers `work` and, on `nope`, says no like a real one.
struct Worker;

#[async_trait]
impl Bundle for Worker {
    fn name(&self) -> &str {
        "worker"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "nope" => Ok(Some(
                json!({"error": "no such path 'x'", "reason": "not_found"}),
            )),
            _ => Ok(Some(json!({"ok": true}))),
        }
    }
}

/// `kernel_at` plus a `worker.tools` bundle, so a dispatch can be made that is
/// not the pump's own traffic.
fn kernel_with_worker(store_root: &Path) -> Arc<Kernel> {
    let mut kernel = Kernel::with_storage(StorageMode::Disk(store_root.to_path_buf()));
    let mut reg = BundleRegistry::new();
    reg.register(
        "file_bridge.tools",
        AppendStore {
            root: store_root.to_path_buf(),
        },
    );
    reg.register("worker.tools", Worker);
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
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

async fn mk_worker(kernel: &Arc<Kernel>, id: &str) {
    let r = kernel
        .send(vec![fantastic_kernel::Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "worker.tools", "id": id,
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], json!(id), "create_agent failed: {r}");
}

/// Every row in the log, parsed back.
fn logged(log: &Path) -> Vec<fantastic_kernel::Row> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    fantastic_kernel::messages_xml::parse_messages(&text).unwrap_or_default()
}

/// Wait until the log holds a row satisfying `pred` (or give up).
async fn until_row(
    log: &Path,
    pred: impl Fn(&fantastic_kernel::Row) -> bool,
) -> Vec<fantastic_kernel::Row> {
    for _ in 0..200 {
        let rows = logged(log);
        if rows.iter().any(&pred) {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    logged(log)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pump_appends_every_row_but_its_own_calls() {
    // Twin of python `test_the_pump_appends_every_row_but_its_own_calls`.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let log = store.join("messages.xml");
    let k = kernel_with_worker(&store);
    wire_store(&k, &store).await;
    fantastic_kernel::messages_log::install_message_pump(&k, 0);
    mk_worker(&k, "sp_p").await;

    for _ in 0..3 {
        k.send(vec![fantastic_kernel::Item::new(
            AgentId::from("sp_p"),
            json!({"type": "work"}),
        )])
        .await;
    }
    let ai = AgentId::from("ai");
    let rid = k.rows.put(&ai, turn("user", "planted")).unwrap();
    k.rows.body_put(&ai, rid, &"a body ".repeat(100)).unwrap();

    let rows = until_row(&log, |r| {
        r.get("kind") == Some(&json!("body")) && r.get("parent") == Some(&json!(rid))
    })
    .await;
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(ids, sorted, "in id order, once each");
    assert!(ids.contains(&rid));

    // A dispatch writes TWO rows now: the `call` at launch and its `settle`.
    for r in &rows {
        if r.get("agent") == Some(&json!("sp_p")) && r.contains_key("kind") {
            assert!(
                matches!(r["kind"].as_str(), Some("call")),
                "unexpected row on the worker: {r:?}"
            );
        }
    }
    // THE PUMP'S OWN WRITES never enter the log — neither the calls it makes on
    // the store nor the task row it runs under.
    assert!(
        rows.iter()
            .all(|r| !(r.get("kind") == Some(&json!("call"))
                && r.get("agent") == Some(&json!("store")))),
        "the pump's own writes to the store reached the log"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pump_skips_its_own_calls_tasks_and_their_settles() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let log = store.join("messages.xml");
    let k = kernel_with_worker(&store);
    wire_store(&k, &store).await;
    fantastic_kernel::messages_log::install_message_pump(&k, 0);
    mk_worker(&k, "sp_s").await;
    k.send(vec![fantastic_kernel::Item::new(
        AgentId::from("sp_s"),
        json!({"type": "work"}),
    )])
    .await;

    let rows = until_row(&log, |r| {
        r.get("agent") == Some(&json!("sp_s")) && r.get("kind") == Some(&json!("call"))
    })
    .await;
    // The pump's own kernel task never reaches the log…
    assert!(
        rows.iter().all(|r| r.get("kind") != Some(&json!("task"))
            || r.get("verb") != Some(&json!("message_pump"))),
        "the pump's own task row reached the log"
    );
    // …and neither do the SETTLES of the calls it skipped: a settle follows its
    // call, or persisting a row produces a row to persist, forever.
    let skipped_parents: Vec<i64> = k
        .rows
        .for_agent(&AgentId::from("store"))
        .iter()
        .filter(|r| r.get("kind") == Some(&json!("call")))
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .collect();
    assert!(
        !skipped_parents.is_empty(),
        "the pump did write to the store"
    );
    for r in &rows {
        if r.get("kind") == Some(&json!("settle")) {
            let parent = r.get("parent").and_then(Value::as_i64).unwrap_or(-1);
            assert!(
                !skipped_parents.contains(&parent),
                "a settle of a skipped call reached the log: {r:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settle_row_outcome_is_the_python_stamp() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let log = store.join("messages.xml");
    let k = kernel_with_worker(&store);
    wire_store(&k, &store).await;
    fantastic_kernel::messages_log::install_message_pump(&k, 0);
    mk_worker(&k, "sp_o").await;
    let out = k
        .send(vec![fantastic_kernel::Item::new(
            AgentId::from("sp_o"),
            json!({"type": "nope"}),
        )])
        .await
        .remove(0);
    let pid = json!(out.pid.map(fantastic_kernel::TaskId::get));

    let is_ours = |r: &fantastic_kernel::Row| {
        r.get("kind") == Some(&json!("settle")) && r.get("parent") == Some(&pid)
    };
    let rows = until_row(&log, is_ours).await;
    let settle = rows
        .iter()
        .find(|r| is_ours(r))
        .expect("the settle reached disk");
    assert_eq!(
        settle["outcome"],
        json!({"status": "errored", "error": "no such path 'x'", "reason": "not_found"}),
        "the log carries python's stamp, not the ledger Outcome"
    );
    // The call row it ended carries the SAME stamp, nested.
    let call = rows
        .iter()
        .find(|r| r.get("kind") == Some(&json!("call")) && r.get("id") == Some(&pid))
        .expect("the call row");
    assert_eq!(call["outcome"], settle["outcome"]);
}
