//! Cold hydration and the loud failures. The pump's own round trip is the
//! integration test (`tests/message_log.rs`); these are the file-level rules.

use super::*;
use crate::messages_xml::row_to_xml;
use serde_json::json;

fn row(v: Value) -> Row {
    v.as_object().unwrap().clone()
}

fn write(dir: &Path, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(MESSAGES_FILE), text).unwrap();
}

fn log_of(rows: &[Value]) -> String {
    let mut doc = String::from(HEADER);
    for r in rows {
        doc.push_str(&row_to_xml(&row(r.clone())));
    }
    doc
}

#[test]
fn a_missing_log_hydrates_nothing_and_starts_the_counter_at_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (rows, next) = read_messages(tmp.path()).expect("a virgin dir is not an error");
    assert!(rows.is_empty());
    assert_eq!(next, 1);
}

#[test]
fn hydration_sets_the_counter_above_every_id_it_read() {
    let tmp = tempfile::TempDir::new().unwrap();
    write(
        tmp.path(),
        &log_of(&[
            json!({"id": 3, "agent": "ai", "role": "user", "content": "one"}),
            json!({"id": 5, "agent": "ai", "role": "assistant", "content": "two"}),
            json!({"id": 9, "agent": "other", "kind": "message", "content": "three"}),
        ]),
    );
    let kernel = crate::Kernel::new();
    assert_eq!(hydrate_messages(&kernel, tmp.path()).unwrap(), 3);
    // THE COUNTER FIRST: every hydrated id is a pre-minted one, and the next
    // mint continues above all of them rather than colliding.
    assert_eq!(kernel.rows.next_id(), 10);
    assert_eq!(kernel.rows.mint_id(), 10);
    let mine = kernel.rows.for_agent(&AgentId::from("ai"));
    assert_eq!(mine.len(), 2);
    assert_eq!(mine[0]["content"], "one");
    assert_eq!(kernel.rows.for_agent(&AgentId::from("other")).len(), 1);
}

#[test]
fn a_second_ingestion_of_the_same_file_is_loud() {
    // It is the caller's mistake, not something to de-duplicate silently: the
    // store is the only allocator and it says so on the first duplicate id.
    let tmp = tempfile::TempDir::new().unwrap();
    write(
        tmp.path(),
        &log_of(&[json!({"id": 1, "agent": "ai", "content": "x"})]),
    );
    let kernel = crate::Kernel::new();
    assert_eq!(hydrate_messages(&kernel, tmp.path()).unwrap(), 1);
    let err = hydrate_messages(&kernel, tmp.path()).expect_err("a re-ingest must be loud");
    assert!(err.contains("pre-minted"), "{err}");
}

#[test]
fn an_unknown_log_version_refuses() {
    let tmp = tempfile::TempDir::new().unwrap();
    write(
        tmp.path(),
        "<messages v=\"9\">\n<row id=\"1\" agent=\"ai\" />\n",
    );
    let err = read_messages(tmp.path()).expect_err("a foreign version must refuse");
    // C55: python `f"{head!r}"` — python repr quoting (single quotes), not
    // rust's Debug `{:?}`.
    assert_eq!(
        err,
        "messages.xml: unknown log version — first line is '<messages v=\"9\">', \
         this loader writes '<messages v=\"1\">'"
    );
}

#[test]
fn a_cut_last_row_is_loud_and_names_the_last_good_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let doc = log_of(&[
        json!({"id": 1, "agent": "ai", "content": "whole"}),
        json!({"id": 2, "agent": "ai", "content": "cut"}),
    ]);
    write(tmp.path(), &doc[..doc.len() - 10]);
    let err = read_messages(tmp.path()).expect_err("a cut row must be loud");
    assert!(err.contains("row 1"), "{err}");
    assert!(err.contains("nothing is repaired automatically"), "{err}");
}

#[test]
fn a_row_with_no_agent_refuses_rather_than_guessing_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    write(
        tmp.path(),
        &log_of(&[json!({"id": 1, "content": "orphan"})]),
    );
    let kernel = crate::Kernel::new();
    let err = hydrate_messages(&kernel, tmp.path()).expect_err("an orphan row must refuse");
    assert!(err.contains("no `agent`"), "{err}");
}

#[test]
fn every_shape_survives_the_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    write(
        tmp.path(),
        &log_of(&[
            json!({"id": 1, "agent": "ai", "kind": "message", "role": "user",
                   "content": "a call: <call name=\"send\"></call> & more\nline two\n"}),
            json!({"id": 2, "agent": "ai", "kind": "fold", "level": 1,
                   "refs": [1], "title": "the survey", "summary": "what it said"}),
            json!({"id": 3, "agent": "ai", "kind": "view", "budget": 12000,
                   "ids": [{"id": 1, "res": "full"}]}),
        ]),
    );
    let kernel = crate::Kernel::new();
    assert_eq!(hydrate_messages(&kernel, tmp.path()).unwrap(), 3);
    let rows = kernel.rows.for_agent(&AgentId::from("ai"));
    assert!(rows[0]["content"]
        .as_str()
        .unwrap()
        .contains("<call name=\"send\"></call>"));
    assert_eq!(rows[1]["refs"], json!([1]));
    assert_eq!(rows[1]["level"], json!(1));
    // The view row rebuilt its index on the way in, so it is CURRENT.
    let view = kernel
        .rows
        .view_get(&AgentId::from("ai"))
        .expect("the view");
    assert_eq!(view["budget"], json!(12000));
    assert_eq!(view["ids"], json!([{"id": 1, "res": "full"}]));
}

// ── the two gated-pump repros (2026-09-24) ─────────────────────────────────
//
// The 0.15 s debounce boundary is a race a sleep only SAMPLES. These drive the
// pump BY HAND — its flush loop halted and its append to `messages.xml` stalled
// on a gate — so each half of the loss is deterministic.

/// A store whose `write_stream` to the LOG blocks until it is released.
struct GatedStore {
    root: std::path::PathBuf,
    entered: Arc<tokio::sync::Notify>,
    hold: Arc<tokio::sync::Notify>,
    gate: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl crate::Bundle for GatedStore {
    fn name(&self) -> &str {
        "file_bridge"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        Ok(Some(json!({"ok": true})))
    }
    async fn handle_binary(
        &self,
        _id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(crate::bundle::Reply, Option<Vec<u8>>), crate::bundle::BundleError> {
        let path = header.get("path").and_then(Value::as_str).unwrap_or("");
        let verb = header.get("type").and_then(Value::as_str).unwrap_or("");
        let target = self.root.join(path);
        if verb == "write_stream" {
            if path == MESSAGES_FILE && self.gate.load(std::sync::atomic::Ordering::SeqCst) {
                self.entered.notify_waiters();
                self.hold.notified().await;
            }
            use std::io::Write;
            if let Some(p) = target.parent() {
                std::fs::create_dir_all(p).ok();
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&target)
                .expect("GatedStore: open");
            f.write_all(&blob).expect("GatedStore: write");
            return Ok((Some(json!({"written": blob.len()})), None));
        }
        match std::fs::read(&target) {
            Ok(b) => Ok((Some(json!({"size": b.len()})), Some(b))),
            Err(_) => Ok((Some(json!({"error": "not found"})), None)),
        }
    }
}

struct Gated {
    kernel: Arc<Kernel>,
    pump: MessagePump,
    entered: Arc<tokio::sync::Notify>,
    hold: Arc<tokio::sync::Notify>,
    gate: Arc<std::sync::atomic::AtomicBool>,
}

/// A root whose pump is DRIVEN BY HAND (its flush loop halted) and whose append
/// to `messages.xml` stalls on an event — the deterministic seam for the two
/// races a debounce sweep only samples.
async fn gated_pump(store_root: &Path) -> Gated {
    let entered = Arc::new(tokio::sync::Notify::new());
    let hold = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut kernel = Kernel::with_storage(crate::StorageMode::Disk(store_root.to_path_buf()));
    let mut reg = crate::BundleRegistry::new();
    reg.register(
        "file_bridge.tools",
        GatedStore {
            root: store_root.to_path_buf(),
            entered: Arc::clone(&entered),
            hold: Arc::clone(&hold),
            gate: Arc::clone(&gate),
        },
    );
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = crate::Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        serde_json::Map::new(),
        store_root.to_path_buf(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    crate::test_support::wire_fake_store(&kernel, store_root).await;
    let pump = install_message_pump(&kernel, kernel.rows.next_id() - 1);
    // The header probe happens on the FIRST flush; run it before the gate, so
    // the stall under test is the append itself.
    kernel
        .rows
        .put(&AgentId::from("ai"), row(json!({"content": "warm"})))
        .unwrap();
    pump.catch_up(&kernel).await;
    // HALT THE FLUSH LOOP: the test IS the flusher from here on.
    pump.halt_flush.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    Gated {
        kernel,
        pump,
        entered,
        hold,
        gate,
    }
}

fn landed(store_root: &Path) -> Vec<i64> {
    let text = std::fs::read_to_string(store_root.join(MESSAGES_FILE)).unwrap_or_default();
    crate::messages_xml::parse_messages(&text)
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .collect()
}

/// `catch_up` used to read `pending` while a flush held the batch in a LOCAL
/// variable — empty queue, counter caught up, so it returned and the caller
/// stopped the writer mid-append. It takes the flush lock for the check now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catch_up_waits_out_an_in_flight_append() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let g = gated_pump(&store).await;
    let waiting = g.entered.notified();
    tokio::pin!(waiting);

    g.kernel
        .rows
        .put(&AgentId::from("ai"), row(json!({"content": "held"})))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await; // the tail picks it up
    let pump = Arc::new(g.pump);
    let p2 = Arc::clone(&pump);
    let k2 = Arc::clone(&g.kernel);
    let flush = tokio::spawn(async move { p2.flush(&k2).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .expect("the append never entered the store");
    // The batch is OUT of the queue and not yet written.
    assert!(pump.state.lock().unwrap().pending.is_empty());

    let p3 = Arc::clone(&pump);
    let k3 = Arc::clone(&g.kernel);
    let catch_up = tokio::spawn(async move { p3.catch_up(&k3).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        !catch_up.is_finished(),
        "catch_up walked past an append still in flight"
    );
    g.gate.store(false, std::sync::atomic::Ordering::SeqCst);
    g.hold.notify_waiters();
    tokio::time::timeout(std::time::Duration::from_secs(2), flush)
        .await
        .expect("the flush never finished")
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), catch_up)
        .await
        .expect("catch_up never converged")
        .unwrap();
}

/// Cancellation between taking the batch and the store's ack dropped those rows
/// on the floor. They return to the head of `pending`, and the stop's own
/// catch-up lands them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_flush_puts_its_batch_back() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = tmp.path().join(".fantastic");
    let g = gated_pump(&store).await;
    let waiting = g.entered.notified();
    tokio::pin!(waiting);

    let rid = g
        .kernel
        .rows
        .put(&AgentId::from("ai"), row(json!({"content": "requeued"})))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let pump = Arc::new(g.pump);
    let p2 = Arc::clone(&pump);
    let k2 = Arc::clone(&g.kernel);
    let flush = tokio::spawn(async move { p2.flush(&k2).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .expect("the append never entered the store");
    assert!(pump.state.lock().unwrap().pending.is_empty());

    // THE CANCEL: the flush future is dropped mid-append.
    flush.abort();
    let _ = flush.await;
    assert_eq!(
        pump.state.lock().unwrap().pending,
        vec![rid],
        "a cancelled flush ate its batch"
    );

    // The store answers again, and the catch-up lands what came back.
    g.gate.store(false, std::sync::atomic::Ordering::SeqCst);
    g.hold.notify_waiters();
    pump.catch_up(&g.kernel).await;
    assert!(
        landed(&store).contains(&rid),
        "the requeued row never reached the log"
    );
}
