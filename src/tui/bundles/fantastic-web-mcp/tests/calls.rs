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
    fantastic_kernel::test_support::send_settled(&Arc::clone(k), vec![item]).await.remove(0).result
}

/// A kernel with a leg `mcp` and a `sleeper` it may call.
async fn setup() -> (Arc<Kernel>, Arc<McpCore>) {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, WebMcpBundle);
    reg.register("sleeper.tools", Sleeper);
    let k = Arc::clone(&bootstrap(reg, BootstrapOptions::in_memory()).expect("boots").kernel);
    let root = k.root().expect("root").id.0.clone();

    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "sleeper",
        "handler_module": "sleeper.tools"})).await;
    assert!(r.get("error").is_none(), "create sleeper: {r}");
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "mcp",
        "handler_module": HANDLER_MODULE, "can": {"sleeper": {"call": ["*"]}}})).await;
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

/// Writing one notification on the session.
type Push = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// The settle notifications a tool call pushes on the session, as the carrier gets them.
fn session() -> (impl FnOnce(Value) -> Push + Send + 'static, tokio::sync::mpsc::UnboundedReceiver<Value>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let push = move |note: Value| -> Push {
        Box::pin(async move {
            let _ = tx.send(note);
        })
    };
    (push, rx)
}

/// The ack a tool call answered: `{pid, status}`.
fn ack(reply: &Value) -> Value {
    body(reply)
}

/// EVERYTHING ASYNC (#795): a tool call answers its ACK at once — while the work
/// still runs — and its settle comes later as the session's notification, naming the
/// ack's pid; the item's row carries the result. No wait bound either way.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_answers_its_ack_then_its_settle_as_a_notification() {
    let (_k, core) = setup().await;
    let (push, mut notes) = session();
    let started = std::time::Instant::now();
    let reply = core.handle(&call(1_500), |_| {}, push).await.expect("a reply");
    assert!(started.elapsed() < std::time::Duration::from_millis(1_000), "the ack does not wait for the work");
    let a = ack(&reply);
    assert_eq!(a["status"], "accepted", "{reply}");
    assert_eq!(reply["result"]["isError"], false);
    let pid = a["pid"].as_i64().expect("the ack names the call");
    assert!(notes.try_recv().is_err(), "nothing settled yet");
    let note = notes.recv().await.expect("the settle notification");
    assert_eq!(note["method"], fantastic_web_mcp::SETTLE_METHOD, "{note}");
    assert!(note.get("id").is_none(), "a notification, not a reply: {note}");
    let p = &note["params"];
    assert_eq!(p["type"], "settle");
    assert_eq!(p["re"], pid, "{note}");
    assert_eq!(p["status"], "completed", "{note}");
    let row = &p["data"]["outcomes"][0];
    assert_eq!(row["status"], "completed", "{note}");
    assert_eq!(row["result"]["slept_ms"], 1_500);
    assert!(started.elapsed() >= std::time::Duration::from_millis(1_500));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_kills_a_call_still_running() {
    let (k, core) = setup().await;
    let (push, mut notes) = session();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let reply = core.handle(&call(10_000), move |t| { let _ = tx.send(t); }, push).await.expect("a reply");
    let task = rx.await.expect("a task id");
    assert_eq!(ack(&reply)["pid"], task.get(), "the ack names the task");
    assert!(k.tasks.contains(&task));
    let r = core.cancel(&task).await;
    assert_eq!(r["cancelled"], true, "{r}");
    assert!(!k.tasks.contains(&task), "gone from the task table");
    let note = notes.recv().await.expect("the settle notification");
    assert_eq!(note["params"]["re"], task.get(), "{note}");
    assert_eq!(note["params"]["status"], "cancelled", "a cancelled call settles cancelled: {note}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_the_leg_was_not_granted_settles_its_refusal() {
    let (k, core) = setup().await;
    let root = k.root().expect("root").id.0.clone();
    let msg = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "send",
        "arguments": {"items": [{"target": root, "payload": {"type": "delete_agent", "id": "sleeper"}}]}}});
    let (push, mut notes) = session();
    let reply = core.handle(&msg, |_| {}, push).await.expect("a reply");
    assert_eq!(ack(&reply)["status"], "accepted", "the hand-in is accepted: {reply}");
    let note = notes.recv().await.expect("the settle notification");
    let row = &note["params"]["data"]["outcomes"][0];
    assert_ne!(row["status"], "completed", "the item is refused: {note}");
    assert!(row["result"]["error"].is_string(), "{note}");
    assert!(k.exists("sleeper"), "nothing was deleted");
}
