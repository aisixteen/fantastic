//! Unit tests for [`crate::send`].

use super::*;
use crate::agent::Agent;
use serde_json::Map;
use std::path::Path;

fn make_agent(id: &str) -> Arc<Agent> {
    Agent::new(
        id.into(),
        None,
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

#[test]
fn summarize_truncates_long_payloads() {
    let v = json!({"text": "x".repeat(500)});
    let s = summarize_payload(&v);
    assert!(s.len() <= 160);
    assert!(s.ends_with("..."));
}

#[test]
fn summarize_short_payload_unchanged() {
    let v = json!({"type": "ping"});
    let s = summarize_payload(&v);
    assert_eq!(s, r#"{"type":"ping"}"#);
}

#[test]
fn current_sender_outside_scope_is_none() {
    assert!(current_sender().is_none());
}

#[tokio::test]
async fn with_sender_propagates_to_nested_async() {
    let r = with_sender(AgentId::from("alice"), async {
        current_sender().map(|s| s.0)
    })
    .await;
    assert_eq!(r.as_deref(), Some("alice"));
}

#[tokio::test]
async fn send_to_missing_target_returns_error() {
    let kernel = Arc::new(Kernel::new());
    let v = kernel
        .send(&AgentId::from("nope"), json!({"type": "ping"}))
        .await;
    assert!(v["error"].as_str().unwrap_or("").contains("no agent"));
}

#[tokio::test]
async fn send_kernel_alias_routes_to_root() {
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("core");
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // Root has no handler_module → reflect is answered natively.
    let v = kernel
        .send(&AgentId::from("kernel"), json!({"type": "reflect"}))
        .await;
    // Reflect returns at least an id + a tree (full shape exercised
    // in reflect.rs tests + integration tests).
    assert!(v.is_object(), "reflect must return an object: {v:?}");
}

#[tokio::test]
async fn watch_then_emit_fans_payload_to_watcher() {
    let kernel = Arc::new(Kernel::new());
    let src = make_agent("src");
    let _rx_src = kernel.register(Arc::clone(&src));
    let watcher = make_agent("watcher");
    let mut rx_watcher = kernel.register(Arc::clone(&watcher));
    kernel
        .watch(&AgentId::from("src"), AgentId::from("watcher"))
        .await;
    kernel
        .emit(&AgentId::from("src"), json!({"type": "hello", "k": 1}))
        .await;
    // Watcher receives the RAW emit payload (Python parity — see
    // Kernel::fanout_to_watchers). State subscribers above got the
    // metadata envelope ({type:"emit", sender, target, verb, summary});
    // watchers see what was actually emitted.
    let got = tokio::time::timeout(std::time::Duration::from_millis(100), rx_watcher.recv())
        .await
        .expect("watcher receive within 100ms")
        .expect("channel still open");
    assert_eq!(got["type"], "hello");
    assert_eq!(got["k"], 1);
}

#[tokio::test]
async fn unwatch_stops_fanout() {
    let kernel = Arc::new(Kernel::new());
    let src = make_agent("src");
    let _rx = kernel.register(Arc::clone(&src));
    let watcher = make_agent("watcher");
    let mut rx_w = kernel.register(Arc::clone(&watcher));
    kernel
        .watch(&AgentId::from("src"), AgentId::from("watcher"))
        .await;
    kernel
        .unwatch(&AgentId::from("src"), &AgentId::from("watcher"))
        .await;
    kernel
        .emit(&AgentId::from("src"), json!({"type": "after_unwatch"}))
        .await;
    // try_recv: nothing queued.
    match rx_w.try_recv() {
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
        other => panic!("expected Empty, got {other:?}"),
    }
}

#[tokio::test]
async fn send_with_binary_missing_target_returns_error() {
    let kernel = Arc::new(Kernel::new());
    let (v, body) = kernel
        .send_with_binary(
            &AgentId::from("nope"),
            json!({"type": "frob"}),
            vec![1, 2, 3],
        )
        .await;
    assert!(v["error"].as_str().unwrap_or("").contains("no agent"));
    assert!(body.is_empty());
}

#[tokio::test]
async fn send_with_binary_routes_through_bundle_handle_binary() {
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    struct EchoBytes;
    #[async_trait::async_trait]
    impl Bundle for EchoBytes {
        fn name(&self) -> &str {
            "echo_bytes"
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            _payload: &Value,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            Ok(Some(json!({"error": "text path not exercised here"})))
        }
        async fn handle_binary(
            &self,
            _agent_id: &AgentId,
            _header: Value,
            blob: Vec<u8>,
            _kernel: &Arc<Kernel>,
        ) -> Result<(Reply, Vec<u8>), BundleError> {
            // Echo the bytes back as the reply BODY (symmetric channel).
            let len = blob.len();
            Ok((Some(json!({"ok": true, "len": len})), blob))
        }
    }
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("echo_bytes.tools", EchoBytes);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let agent = Agent::new(
        AgentId::from("eb"),
        Some("echo_bytes.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("eb"),
        true,
    );
    let _rx = kernel.register(Arc::clone(&agent));
    let (reply, body) = kernel
        .send_with_binary(
            &AgentId::from("eb"),
            json!({"type": "ingest"}),
            vec![0u8; 64],
        )
        .await;
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["len"], 64);
    // Symmetric channel: bytes flow back as the reply body too.
    assert_eq!(body.len(), 64);
}

#[tokio::test]
async fn list_agents_returns_records_for_every_registered() {
    let kernel = Arc::new(Kernel::new());
    let a = make_agent("a_1");
    let b = make_agent("b_1");
    let _rx = kernel.register(Arc::clone(&a));
    let _rx2 = kernel.register(Arc::clone(&b));
    kernel.set_root(Arc::clone(&a));
    let v = kernel
        .send(&AgentId::from("a_1"), json!({"type": "list_agents"}))
        .await;
    let agents = v["agents"].as_array().expect("list_agents.agents array");
    let ids: Vec<&str> = agents
        .iter()
        .filter_map(|x| x.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(ids, vec!["a_1", "b_1"]);
}
