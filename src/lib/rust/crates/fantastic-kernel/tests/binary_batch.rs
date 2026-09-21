//! A binary item rides the ORDINARY batch.
//!
//! `send_with_binary` bypassed `send` entirely, so a `write_stream` the model
//! issued through `send_binary` had no task row (invisible to `ps`, unreachable
//! by `cancel`), no ledger id (no dedup, no join) and no timeout — and the
//! reply tag it produced read `[reply to - · …]` because there was no pid to
//! name (measured 2026-09-02). An `Item` may now carry the blob, and the kernel
//! dispatches it through `handle_binary` under the same row, id and deadline as
//! any other item.

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
        blob: Vec<u8>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Vec<u8>>), Box<dyn std::error::Error + Send + Sync>> {
        let n = blob.len();
        *self.0.lock().unwrap() = blob;
        Ok((
            Some(json!({"written": n, "path": header.get("path").cloned()})),
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
        .with_binary(payload.clone())])
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
        .with_binary(first.clone())])
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
        .with_binary(vec![9, 9, 9, 9, 9, 9])])
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
        .with_binary(payload.clone())])
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
