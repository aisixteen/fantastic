//! THE BOUND AND THE CANCEL, against a real kernel.
//!
//! E15 (e2e) drives the binary end to end, but no shipped agent has a verb that
//! blocks, so these two paths are pinned here with a test-only bundle that
//! sleeps. A real in-memory kernel, the real gate, the real task table: the only
//! fake is the sleeper.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    bundle::BundleError,
    send::with_operator,
    AgentId, Bundle, BundleRegistry, Item, Kernel, Reply,
};
use fantastic_web_mcp::{McpCore, WebMcpBundle, BOUND_FIELD, HANDLER_MODULE};
use serde_json::{json, Value};

/// Sleeps for `ms`, then answers. The only fake in this file.
struct Sleeper;

#[async_trait]
impl Bundle for Sleeper {
    fn name(&self) -> &str {
        "sleeper"
    }
    async fn handle(&self, _id: &AgentId, p: &Value, _k: &Arc<Kernel>) -> Result<Reply, BundleError> {
        match p.get("type").and_then(Value::as_str) {
            Some("wait") => {
                let ms = p.get("ms").and_then(Value::as_u64).unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(Some(json!({"slept_ms": ms})))
            }
            _ => Ok(Some(json!({"verbs": {"wait": "args: ms:int"}}))),
        }
    }
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    let k2 = Arc::clone(k);
    let item = Item::new(AgentId::from(target), payload);
    with_operator(async move { k2.send(vec![item]).await }).await.remove(0).result
}

/// A kernel with a leg `mcp` (bound `bound_ms`) and a `sleeper` it may call.
async fn setup(bound_ms: u64) -> (Arc<Kernel>, McpCore) {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, WebMcpBundle);
    reg.register("sleeper.tools", Sleeper);
    let k = Arc::clone(&bootstrap(reg, BootstrapOptions::in_memory()).expect("boots").kernel);
    let root = k.root().expect("root").id.0.clone();

    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "sleeper",
        "handler_module": "sleeper.tools", "callable_by": {"mcp": "*"}})).await;
    assert!(r.get("error").is_none(), "create sleeper: {r}");
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "mcp",
        "handler_module": HANDLER_MODULE, "can_call": {"sleeper": "*"}, BOUND_FIELD: bound_ms})).await;
    assert!(r.get("error").is_none(), "create mcp: {r}");

    let core = McpCore::new(Arc::clone(&k), AgentId::from("mcp"), Duration::from_millis(bound_ms), "test");
    (k, core)
}

fn call(ms: u64) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "send",
        "arguments": {"items": [{"target": "sleeper", "payload": {"type": "wait", "ms": ms}}]}}})
}

fn body(reply: &Value) -> Value {
    let text = reply["result"]["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_inside_the_bound_answers_its_outcome() {
    let (_k, core) = setup(2_000).await;
    let reply = core.handle(&call(20), |_| {}).await.expect("a reply");
    let b = body(&reply);
    assert_eq!(b[0]["status"], "completed", "{reply}");
    assert_eq!(b[0]["result"]["slept_ms"], 20);
    assert_eq!(reply["result"]["isError"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_past_the_bound_answers_its_pid_and_keeps_running() {
    let (k, core) = setup(100).await;
    let mut seen = None;
    let reply = core.handle(&call(1_500), |t| seen = Some(t)).await.expect("a reply");
    let b = body(&reply);
    assert_eq!(b["status"], "running", "{reply}");
    let pid = b["pid"].as_i64().expect("a pid");
    let task = seen.expect("the carrier was told the task id");
    assert_eq!(task.get(), pid, "the pid IS the task the carrier can cancel");
    assert!(k.tasks.contains(&task), "still running after the reply");
    // ...and it finishes on its own.
    tokio::time::sleep(Duration::from_millis(1_700)).await;
    assert!(!k.tasks.contains(&task), "the task ended after the sleep");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_kills_a_call_still_running() {
    let (k, core) = setup(100).await;
    let mut seen = None;
    let _ = core.handle(&call(10_000), |t| seen = Some(t)).await.expect("a reply");
    let task = seen.expect("a task id");
    assert!(k.tasks.contains(&task));
    let r = core.cancel(&task).await;
    assert_eq!(r["cancelled"], true, "{r}");
    assert!(!k.tasks.contains(&task), "gone from the task table");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_the_leg_was_not_granted_is_refused_as_is_error() {
    let (k, core) = setup(2_000).await;
    let root = k.root().expect("root").id.0.clone();
    let msg = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "send",
        "arguments": {"items": [{"target": root, "payload": {"type": "delete_agent", "id": "sleeper"}}]}}});
    let reply = core.handle(&msg, |_| {}).await.expect("a reply");
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert!(k.agents.contains_key(&AgentId::from("sleeper")), "nothing was deleted");
}
