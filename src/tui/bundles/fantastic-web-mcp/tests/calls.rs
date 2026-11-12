//! A CALL AND ITS CANCEL, against a real kernel.
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
    bundle::{BundleError, Handler},
    AgentId, BundleRegistry, Item, Kernel, Reply,
};
use fantastic_web_mcp::{McpCore, WebMcpBundle, HANDLER_MODULE};
use serde_json::{json, Value};

fantastic_kernel::stand_in_verbs!(sleeper_verbs { Wait "wait" { ms: Option<u64> } });

/// Sleeps for `ms`, then answers. The only fake in this file.
struct Sleeper;

#[async_trait]
impl Handler for Sleeper {
    type Call = sleeper_verbs::Call;
    fn name(&self) -> &str {
        "sleeper"
    }
    fn man(&self) -> &'static str {
        "A test stand-in that sleeps."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        sleeper_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        sleeper_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }
    async fn handle(&self, id: &AgentId, call: Self::Call, _k: &Arc<Kernel>) -> Result<Reply, BundleError> {
        match call {
            sleeper_verbs::Call::Wait(w) => {
                let ms = w.ms.unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(Some(json!({"slept_ms": ms})))
            }
            sleeper_verbs::Call::Reflect(_) => Ok(Some(json!({"id": id.as_str(), "sentence": "a sleeper"}))),
        }
    }
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    // a send outside every dispatch is the operator's
    let item = Item::new(AgentId::from(target), payload);
    Arc::clone(k).send(vec![item]).await.remove(0).result
}

/// A kernel with a leg `mcp` and a `sleeper` it may call.
async fn setup() -> (Arc<Kernel>, Arc<McpCore>) {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, WebMcpBundle);
    reg.register("sleeper.tools", Sleeper);
    let k = Arc::clone(&bootstrap(reg, BootstrapOptions::in_memory()).expect("boots").kernel);
    let root = k.root().expect("root").id.0.clone();

    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "sleeper",
        "handler_module": "sleeper.tools", "callable_by": {"mcp": "*"}})).await;
    assert!(r.get("error").is_none(), "create sleeper: {r}");
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "mcp",
        "handler_module": HANDLER_MODULE, "can_call": {"sleeper": "*"}})).await;
    assert!(r.get("error").is_none(), "create mcp: {r}");

    let core = Arc::new(McpCore::new(Arc::clone(&k), AgentId::from("mcp"), "test"));
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
async fn a_call_answers_its_outcome_when_it_ends_with_no_wait_bound() {
    let (_k, core) = setup().await;
    let reply = core.handle(&call(1_500), |_| {}).await.expect("a reply");
    let b = body(&reply);
    assert_eq!(b[0]["status"], "completed", "{reply}");
    assert_eq!(b[0]["result"]["slept_ms"], 1_500);
    assert_eq!(reply["result"]["isError"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_kills_a_call_still_running() {
    let (k, core) = setup().await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let c = Arc::clone(&core);
    let pending = tokio::spawn(async move { c.handle(&call(10_000), move |t| { let _ = tx.send(t); }).await });
    let task = rx.await.expect("a task id");
    assert!(k.tasks.contains(&task));
    let r = core.cancel(&task).await;
    assert_eq!(r["cancelled"], true, "{r}");
    assert!(!k.tasks.contains(&task), "gone from the task table");
    let reply = pending.await.expect("joined").expect("a reply");
    assert_eq!(reply["result"]["isError"], true, "a cancelled call answers as an error: {reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_the_leg_was_not_granted_is_refused_as_is_error() {
    let (k, core) = setup().await;
    let root = k.root().expect("root").id.0.clone();
    let msg = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "send",
        "arguments": {"items": [{"target": root, "payload": {"type": "delete_agent", "id": "sleeper"}}]}}});
    let reply = core.handle(&msg, |_| {}).await.expect("a reply");
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert!(k.agents.contains_key(&AgentId::from("sleeper")), "nothing was deleted");
}
