//! A binary item rides the ORDINARY batch.
//!
//! There is ONE send. A second, bypassing path for bytes once left a
//! `write_stream` with no task row (invisible to `ps`, unreachable by
//! `cancel`), no ledger id (no dedup, no join) and no timeout (measured
//! 2026-09-02); it is gone. An `Item` carries the blob (or asks for a raw reply
//! chunk), and the kernel dispatches it through `handle_binary` under the same
//! row, id and deadline as any other item — in python's payload shape.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Reply, Status};
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// Records what reached the BINARY channel, and answers the text channel with a
/// refusal so a mis-routed call cannot look like success.
struct Sink(Arc<Mutex<Vec<u8>>>);

#[async_trait]
impl Bundle for Sink {
    fn name(&self) -> &str {
        "sink"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Some(
            json!({"error": "text channel — the blob did not ride"}),
        ))
    }
    async fn handle_binary(
        &self,
        _id: &AgentId,
        header: Value,
        body: Option<fantastic_kernel::Blob>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<fantastic_kernel::Blob>), Box<dyn std::error::Error + Send + Sync>>
    {
        let blob: Vec<u8> = body.map(|b| b.bytes).unwrap_or_default();
        // A READ answers a chunk; it never received one.
        if header.get("type").and_then(Value::as_str) == Some("read_stream") {
            return Ok((
                Some(json!({"path": header.get("path").cloned(), "bytes": null,
                            "header_had_bytes": header.get("bytes").is_some()})),
                Some(fantastic_kernel::Blob::key("bytes", b"chunk".to_vec())),
            ));
        }
        let n = blob.len();
        *self.0.lock().unwrap() = blob;
        Ok((
            Some(json!({"written": n, "path": header.get("path").cloned(),
                        "header_had_bytes": header.get("bytes").is_some()})),
            None,
        ))
    }
}

async fn mk() -> (TempDir, Arc<Kernel>, Arc<Mutex<Vec<u8>>>) {
    let tmp = TempDir::new().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("sink.tools", Sink(Arc::clone(&seen)));
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "kernel_state", "id": "sink",
                   "handler_module": "sink.tools"}),
        )])
        .await;
    (tmp, kernel, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_binary_item_reaches_the_binary_channel_with_a_real_pid() {
    let (_t, kernel, seen) = mk().await;
    let payload: Vec<u8> = (0u8..=255).collect(); // non-utf8: proves it is raw
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("sink"),
            json!({"type": "write_stream", "path": "blob.bin"}),
        )
        .with_binary_key("bytes", payload.clone())])
        .await
        .remove(0);
    assert_eq!(out.status, Status::Completed, "{:?}", out.result);
    assert_eq!(out.result["written"], payload.len(), "{}", out.result);
    assert_eq!(*seen.lock().unwrap(), payload, "bytes must ride verbatim");
    let pid = out.pid.expect("a batched binary item has a real pid");
    assert!(pid.0 > 0, "{pid:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_binary_item_dedups_like_any_other() {
    let (_t, kernel, seen) = mk().await;
    let first: Vec<u8> = vec![1, 2, 3];
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("sink"),
            json!({"type": "write_stream", "path": "a"}),
        )
        .with_id("job-b")
        .with_binary_key("bytes", first.clone())])
        .await
        .remove(0);
    assert_eq!(out.result["written"], 3, "{}", out.result);
    let first_pid = out.pid.expect("a named item has a pid");
    assert!(first_pid.0 > 0);

    // The SAME id re-sent returns the recorded outcome and runs nothing.
    let again = kernel
        .send(vec![Item::new(
            AgentId::from("sink"),
            json!({"type": "write_stream", "path": "a"}),
        )
        .with_id("job-b")
        .with_binary_key("bytes", vec![9, 9, 9, 9, 9, 9])])
        .await
        .remove(0);
    assert_eq!(
        again.result["written"], 3,
        "the recorded outcome, not a rerun"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        first,
        "the handler must not have run a second time"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_binary_item_still_reaches_the_binary_channel() {
    // The item's own DEADLINE must not cost it its bytes. Swift had two
    // dispatch branches and only the untimed one forwarded the blob, so every
    // item carrying a timeout fell silently to the TEXT channel — and AI
    // routing stamps the layer's deadline on EVERY item, making that the only
    // shape a model could produce (e2e `E10_dialog:swift`, 2026-09-03). Rust
    // chooses the channel once, inside the spawned task, before the deadline
    // race; this pins that structure.
    let (_t, kernel, seen) = mk().await;
    let payload = b"42\n".to_vec();
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("sink"),
            json!({"type": "write_stream", "path": "answer"}),
        )
        .with_timeout(5.0)
        .with_binary_key("bytes", payload.clone())])
        .await
        .remove(0);
    assert_eq!(out.status, Status::Completed, "{:?}", out.result);
    assert_eq!(out.result["written"], payload.len(), "{}", out.result);
    assert_eq!(
        *seen.lock().unwrap(),
        payload,
        "a deadline must not divert the blob"
    );
}

/// The call row's payload for the execution `pid`.
fn row_payload(kernel: &Arc<Kernel>, pid: i64) -> Value {
    Value::Object(
        kernel.rows.get_one(pid).expect("the call row")["payload"]
            .as_object()
            .cloned()
            .expect("a payload object"),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_read_is_a_plain_call_on_the_row_and_to_watchers() {
    // python's `read_stream` payload carries no bytes: the call row is the plain
    // dict and a watcher gets a text frame. Only the REPLY carries a raw chunk —
    // and a plain item gets it back with nothing declared on the call.
    let (_t, kernel, _seen) = mk().await;
    let tap = AgentId::from("tap_read");
    let mut rx = kernel.ensure_inbox(&tap);
    assert!(kernel
        .watch(&AgentId::from("sink"), tap.clone())
        .await
        .is_none());
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("sink"),
            json!({"type": "read_stream", "path": "a", "offset": 0}),
        )])
        .await
        .remove(0);
    assert_eq!(out.body_bytes(), Some(&b"chunk"[..]), "{:?}", out.result);
    assert_eq!(out.result["header_had_bytes"], false);
    let pid = out.pid.expect("an execution").0;
    let keys: Vec<String> = row_payload(&kernel, pid)
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        keys,
        ["type", "path", "offset"],
        "no bytes key on a read's row"
    );
    let got = rx.try_recv().expect("mirrored");
    assert!(got.body.is_none(), "a read is a TEXT frame to a watcher");
    assert!(got.payload.get("bytes").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_keeps_the_bytes_slot_where_python_holds_it() {
    // python's write payload is `{type, path, bytes, truncate}`: the row reads
    // `bytes: {len}` IN THAT POSITION, the watcher frame keeps it there, and the
    // handler gets the body as the blob with no second `bytes` in its header.
    let (_t, kernel, seen) = mk().await;
    let tap = AgentId::from("tap_write");
    let mut rx = kernel.ensure_inbox(&tap);
    assert!(kernel
        .watch(&AgentId::from("sink"), tap.clone())
        .await
        .is_none());
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("sink"),
            json!({"type": "write_stream", "path": "a", "bytes": null, "truncate": true}),
        )
        .with_binary_key("bytes", vec![7, 8, 9])])
        .await
        .remove(0);
    assert_eq!(out.result["written"], 3, "{}", out.result);
    assert_eq!(out.result["header_had_bytes"], false);
    assert_eq!(*seen.lock().unwrap(), vec![7, 8, 9]);
    let pid = out.pid.expect("an execution").0;
    let row = row_payload(&kernel, pid);
    let keys: Vec<&String> = row.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["type", "path", "bytes", "truncate"]);
    assert_eq!(row["bytes"], json!({"len": 3}));
    let got = rx.try_recv().expect("mirrored");
    let wkeys: Vec<&String> = got.payload.as_object().unwrap().keys().collect();
    assert_eq!(wkeys, ["type", "path", "bytes", "truncate"]);
    assert_eq!(
        got.body,
        Some(fantastic_kernel::Blob::key("bytes", vec![7, 8, 9]))
    );
}
