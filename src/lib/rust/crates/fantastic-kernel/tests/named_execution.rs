//! A NAMED execution's task-table handle IS the caller's id.
//!
//! Measured 2026-09-02: `send([{target, payload, id: "job-1"}])` registered the
//! dispatch under a freshly minted `<agent>:<hex>` pid, so `ps` showed an id the
//! caller had never seen and `cancel task_id="job-1"` answered
//! `{cancelled:false, killed:[]}` — the caller could neither find nor stop the
//! execution it had just named. Python registers named work under `exec_id`.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Reply, Status};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// Sleeps long enough that the test can act on it mid-flight.
struct Slow;

#[async_trait]
impl Bundle for Slow {
    fn name(&self) -> &str {
        "slow"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        match payload.get("type").and_then(Value::as_str) {
            Some("work") => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(Some(json!({"done": true})))
            }
            _ => Ok(Some(json!({"ok": true}))),
        }
    }
}

fn mk() -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("slow.tools", Slow);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    (tmp, kernel)
}

async fn seed(kernel: &Arc<Kernel>, id: &str) {
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": "core", "id": id,
                   "handler_module": "slow.tools"}),
        )])
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_named_execution_is_registered_under_its_own_id() {
    let (_t, kernel) = mk();
    seed(&kernel, "w").await;
    let k = Arc::clone(&kernel);
    let running = tokio::spawn(async move {
        k.send(vec![
            Item::new(AgentId::from("w"), json!({"type": "work"})).with_id("job-1")
        ])
        .await
        .remove(0)
    });
    // Let it register.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let ps = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ps"}),
        )])
        .await
        .remove(0)
        .result;
    let ids: Vec<String> = ps["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_str().map(str::to_string))
        .collect();
    assert!(
        ids.iter().any(|i| i == "job-1"),
        "the caller's own id must be the ps handle, got {ids:?}"
    );

    // …and CANCEL by that same id reaches it.
    let c = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "cancel", "task_id": "job-1"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(c["cancelled"], true, "{c}");
    assert_eq!(c["killed"][0], "job-1", "{c}");

    let out = running.await.unwrap();
    assert_eq!(out.status, Status::Cancelled, "{:?}", out.result);
    assert_eq!(
        out.pid.as_ref().map(|p| p.0.as_str()),
        Some("job-1"),
        "the outcome's pid is the caller's id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unnamed_execution_still_mints_a_pid() {
    let (_t, kernel) = mk();
    seed(&kernel, "w").await;
    let out = kernel
        .send(vec![Item::new(AgentId::from("w"), json!({"type": "ping"}))])
        .await
        .remove(0);
    let pid = out.pid.expect("an unnamed item still gets a minted pid");
    assert!(
        pid.0.starts_with("w:"),
        "a minted pid is <agent>:<hex>, got {pid:?}"
    );
}
