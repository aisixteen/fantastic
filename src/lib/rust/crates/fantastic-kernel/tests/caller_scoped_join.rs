//! JOINING a caller-scoped kernel verb's execution (owner-approved 2026-10-03,
//! audit KV-1) — the twin of python `tests/kernel/test_caller_scoped_join.py`.
//!
//! `ledger` and `ack_deadletter` answer from WHO sends them — the executions that
//! caller originated. A join never re-runs the handler, so before the fix a
//! joiner was handed the original caller's view (`rs_probe`: "B JOIN of A's named
//! ledger leaks A's view: true"). Now a join of such an execution by any caller
//! other than the one that sent it is refused; the original caller still dedups,
//! and cross-caller joins of every other verb are unchanged.

use async_trait::async_trait;
use fantastic_kernel::{Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Outcome, Reply};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tempfile::TempDir;

const MOD: &str = "scoped_join_probe.tools";

struct Probe;

#[async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "scoped_join_probe"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match verb {
            "boot" | "shutdown" | "reflect" => Ok(Some(
                json!({"id": id.as_str(), "sentence": "scoped join probe", "verbs": {}}),
            )),
            "work" => Ok(Some(json!({"body": "result-of-a"}))),
            "boom" => Err("boom".into()),
            other => Ok(Some(json!({"error": format!("unknown verb {other:?}")}))),
        }
    }
}

fn mk() -> (TempDir, Arc<Kernel>) {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register(MOD, Probe);
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
    (tmp, kernel)
}

async fn create(k: &Arc<Kernel>, payload: Value) {
    let r = k
        .send(vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
        .result;
    assert!(r.get("id").is_some(), "{r}");
}

/// `A` and `B`, children of the root granted its caller-scoped verbs (and `ps`)
/// by name — a wildcard never covers a kernel verb — and a `desk` both may call.
async fn seed(k: &Arc<Kernel>) {
    create(
        k,
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "desk",
               "handler_module": MOD, "callable_by": {"A": "*", "B": "*"}}),
    )
    .await;
    for aid in ["A", "B"] {
        create(
            k,
            json!({"type": "create_agent", "parent_id": "kernel_state", "id": aid,
                   "handler_module": MOD,
                   "can_call": {"desk": "*",
                                "kernel_state": ["ledger", "ack_deadletter", "ps"]}}),
        )
        .await;
    }
}

async fn send(k: &Arc<Kernel>, item: Item, sender: Option<&str>) -> Outcome {
    let run = async { k.send(vec![item]).await.remove(0) };
    match sender {
        None => run.await,
        Some(s) => fantastic_kernel::send::with_sender(AgentId::from(s), run).await,
    }
}

/// A NAMED call: the ledger keeps a named execution's result body.
fn work() -> Item {
    Item::new(AgentId::from("desk"), json!({"type": "work"})).with_id("work-a")
}

fn kernel_item(payload: Value) -> Item {
    Item::new(AgentId::from("kernel"), payload)
}

fn refused(exec_id: &str, verb: &str) -> Value {
    json!({
        "error": format!(
            "send: id {exec_id} names another caller's {verb} execution — {verb} \
             answers only the caller that sent it, so it cannot be joined; make your \
             own {verb} call under a new id"
        ),
        "reason": "bad_arg",
        "arg": "id",
    })
}

fn pid(out: &Outcome) -> i64 {
    out.pid.expect("a pid").get()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_join_of_a_named_ledger_read_is_refused() {
    let (_t, k) = mk();
    seed(&k).await;
    send(&k, work(), Some("A")).await;
    let own = send(
        &k,
        kernel_item(json!({"type": "ledger"})).with_id("led-A"),
        Some("A"),
    )
    .await;
    assert_eq!(own.status.as_str(), "completed", "{:?}", own.result);
    assert!(
        own.result.to_string().contains("result-of-a"),
        "{}",
        own.result
    );

    let by_name = send(
        &k,
        kernel_item(json!({"type": "ledger"})).with_id("led-A"),
        Some("B"),
    )
    .await;
    assert_eq!(by_name.status.as_str(), "failed");
    assert_eq!(by_name.result, refused("'led-A'", "ledger"));
    let by_pid = send(
        &k,
        kernel_item(json!({"type": "ledger"})).with_raw_id(json!(pid(&own))),
        Some("B"),
    )
    .await;
    assert_eq!(by_pid.result, refused(&pid(&own).to_string(), "ledger"));

    // the original caller still dedups: the recorded view
    let again = send(
        &k,
        kernel_item(json!({"type": "ledger"})).with_id("led-A"),
        Some("A"),
    )
    .await;
    assert_eq!(again.result, own.result);
    assert_eq!(pid(&again), pid(&own));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_joining_the_operators_ledger_read_is_refused() {
    let (_t, k) = mk();
    seed(&k).await;
    send(&k, work(), Some("A")).await;
    let op = send(&k, kernel_item(json!({"type": "ledger"})), None).await;
    assert!(
        op.result.to_string().contains("result-of-a"),
        "the operator's read is unscoped"
    );
    let out = send(
        &k,
        kernel_item(json!({"type": "ledger"})).with_raw_id(json!(pid(&op))),
        Some("B"),
    )
    .await;
    assert_eq!(out.result, refused(&pid(&op).to_string(), "ledger"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_joining_an_agents_ledger_read_is_refused() {
    let (_t, k) = mk();
    seed(&k).await;
    send(
        &k,
        kernel_item(json!({"type": "ledger"})).with_id("led-A"),
        Some("A"),
    )
    .await;
    let out = send(
        &k,
        kernel_item(json!({"type": "ledger"})).with_id("led-A"),
        None,
    )
    .await;
    assert_eq!(out.result, refused("'led-A'", "ledger"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_join_of_an_ack_deadletter_is_refused() {
    let (_t, k) = mk();
    seed(&k).await;
    let dead = send(
        &k,
        Item::new(AgentId::from("desk"), json!({"type": "boom"})),
        Some("A"),
    )
    .await;
    assert_eq!(dead.status.as_str(), "failed", "{:?}", dead.result);
    let ack = || kernel_item(json!({"type": "ack_deadletter", "id": pid(&dead)})).with_id("ack-A");
    let own = send(&k, ack(), Some("A")).await;
    assert_eq!(own.result["acked"], json!(true), "{:?}", own.result);
    let out = send(&k, ack(), Some("B")).await;
    assert_eq!(out.result, refused("'ack-A'", "ack_deadletter"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_caller_joins_of_other_verbs_stay_open() {
    // `ps` answers the same table to everyone: B joining A's named `ps` is the
    // once-only contract, unchanged.
    let (_t, k) = mk();
    seed(&k).await;
    let own = send(
        &k,
        kernel_item(json!({"type": "ps"})).with_id("ps-A"),
        Some("A"),
    )
    .await;
    let joined = send(
        &k,
        kernel_item(json!({"type": "ps"})).with_id("ps-A"),
        Some("B"),
    )
    .await;
    assert_eq!(own.status.as_str(), "completed", "{:?}", own.result);
    assert_eq!(joined.result, own.result);
    assert_eq!(pid(&joined), pid(&own));
}
