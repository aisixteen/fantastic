//! Unit tests for [`crate::send`].
use crate::outcome::Item;

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
fn summarize_never_truncates() {
    // It used to cut at 160 chars + "...". A summary that silently drops the end of
    // a payload lies about what was sent, and the reader cannot tell a short payload
    // from a cut one — while the dropped part is exactly what you opened telemetry
    // to find. Eliding is a RENDER decision, where whoever is looking can see it
    // happen. Mirrors py `test_summary_shrinks_bytes_but_never_truncates`.
    let big = "x".repeat(500);
    let v = json!({ "text": big });
    let s = summarize_payload(&v);
    assert!(s.contains(&big), "the payload must survive whole");
    assert!(!s.ends_with("..."));
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
        .send(vec![Item::new(
            AgentId::from("nope"),
            json!({"type": "ping"}),
        )])
        .await
        .remove(0)
        .result;
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
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
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
        .send(vec![Item::new(
            AgentId::from("a_1"),
            json!({"type": "list_agents"}),
        )])
        .await
        .remove(0)
        .result;
    let agents = v["agents"].as_array().expect("list_agents.agents array");
    let ids: Vec<&str> = agents
        .iter()
        .filter_map(|x| x.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(ids, vec!["a_1", "b_1"]);
}

// ─── #2 — current_origin: the CALLER, not the callee ────────────────

#[tokio::test]
async fn a_handler_reads_its_caller_not_itself() {
    // `current_sender()` inside a handler is that handler's OWN agent — the
    // callee. A bundle whose payload NAMES a resource needs the CALLER, which
    // is what `current_origin()` carries. Without it an agent could write
    // another agent's namespace through a blob verb and nothing could tell.
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    use crate::send::{current_origin, with_sender};
    use std::sync::Mutex;

    static SEEN: Mutex<Option<(Option<String>, Option<String>)>> = Mutex::new(None);

    struct Spy;
    #[async_trait::async_trait]
    impl Bundle for Spy {
        fn name(&self) -> &str {
            "origin_spy"
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            _payload: &Value,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            *SEEN.lock().unwrap() = Some((
                crate::send::current_sender().map(|a| a.0),
                current_origin().map(|a| a.0),
            ));
            Ok(Some(json!({"ok": true})))
        }
    }

    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("origin_spy.tools", Spy);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);

    // Two-sided gate: the edge exists only if BOTH ends grant it, so the agent
    // half of this test needs real wiring — a bare `with_sender` of an
    // unregistered id is (correctly) deny-all.
    let mut spy_meta = Map::new();
    spy_meta.insert("callable_by".into(), json!({"caller": "*"}));
    let spy = Agent::new(
        AgentId::from("spy"),
        Some("origin_spy.tools".to_string()),
        None,
        spy_meta,
        Path::new("/tmp/nowhere").join("spy"),
        true,
    );
    let _rx = kernel.register(Arc::clone(&spy));

    let mut caller_meta = Map::new();
    caller_meta.insert("can_call".into(), json!({"spy": "*"}));
    let caller = Agent::new(
        AgentId::from("caller"),
        None,
        None,
        caller_meta,
        Path::new("/tmp/nowhere").join("caller"),
        true,
    );
    let _rx2 = kernel.register(Arc::clone(&caller));

    // OPERATOR send: there is no caller, so there is no origin.
    let r = kernel
        .send(vec![Item::new(spy.id.clone(), json!({"type": "poke"}))])
        .await
        .remove(0)
        .result;
    let (sender, origin) = SEEN
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| panic!("handler never ran; reply={r}"));
    assert_eq!(sender.as_deref(), Some("spy"), "sender is the CALLEE");
    assert_eq!(origin, None, "an operator send has no origin");

    // AGENT send: origin names the CALLER; sender still names the callee.
    let _ = with_sender(AgentId::from("caller"), async {
        kernel
            .send(vec![Item::new(spy.id.clone(), json!({"type": "poke"}))])
            .await
            .remove(0)
            .result
    })
    .await;
    let (sender, origin) = SEEN
        .lock()
        .unwrap()
        .take()
        .expect("the granted edge must reach the handler");
    assert_eq!(sender.as_deref(), Some("spy"));
    assert_eq!(
        origin.as_deref(),
        Some("caller"),
        "origin must be the CALLER — the whole reason the third local exists"
    );
}

// ─── the READ side of the gate: watch + the state stream ────────────

#[tokio::test]
async fn an_ungranted_agent_cannot_watch_another() {
    // A watcher receives its target's raw inbound payloads VERBATIM — the model
    // prompt, the document body, whatever was sent. So an ungated `watch` is a
    // read side-channel around `callable_by`: an agent that may not CALL an
    // agent could still read everything sent TO it.
    //
    // Found 2026-08-27 in a dataroom audit: a browser leg granted exactly
    // `assistant:[send]` could mirror ANY agent's inbox in its session and read
    // the packed confidential context out of the model's prompt.
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("core");
    let _r = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let secret = make_agent("secret");
    let _s = kernel.register(Arc::clone(&secret));
    let nosy = make_agent("nosy");
    let _n = kernel.register(Arc::clone(&nosy));

    let denial = crate::send::with_sender(AgentId::from("nosy"), async {
        kernel
            .watch(&AgentId::from("secret"), AgentId::from("nosy"))
            .await
    })
    .await;
    assert!(denial.is_some(), "an ungranted watch was allowed");
    assert_eq!(denial.unwrap()["reason"], "unauthorized");
    // ...and nothing was registered, so no payload can ever reach it.
    assert!(!secret
        .watcher_ids
        .read()
        .await
        .contains(&AgentId::from("nosy")));
}

#[tokio::test]
async fn an_operator_watch_stays_ungated() {
    // `sender = None` is the origin root of trust — the bootstrap wiring a
    // renderer, a test driver. Same rule as send.
    let kernel = Arc::new(Kernel::new());
    let src = make_agent("src");
    let _s = kernel.register(Arc::clone(&src));
    assert!(kernel
        .watch(&AgentId::from("src"), AgentId::from("cli"))
        .await
        .is_none());
    assert!(src.watcher_ids.read().await.contains(&AgentId::from("cli")));
}

#[tokio::test]
async fn the_state_stream_is_not_covered_by_a_wildcard() {
    // The broadest read the kernel offers. The structural baseline grants
    // `$parent: "*"`, so WITHOUT `state_subscribe` being privileged every direct
    // child of the root could tap the whole stream while holding no grant that
    // names it.
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("core");
    let _r = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let mut meta = Map::new();
    // A wildcard on the root — deliberately generous, and still not enough.
    meta.insert("can_call".into(), json!({"core": "*"}));
    let leaf = Agent::new(
        AgentId::from("leaf"),
        None,
        Some(root.id.clone()),
        meta,
        Path::new("/tmp/nowhere").join("leaf"),
        true,
    );
    let _l = kernel.register(Arc::clone(&leaf));

    let denial = crate::send::with_sender(AgentId::from("leaf"), async {
        kernel.authorize_state_subscribe(&AgentId::from("leaf"))
    })
    .await;
    assert!(
        denial.is_some(),
        "a wildcard covered the whole state stream"
    );
    assert_eq!(denial.unwrap()["reason"], "unauthorized");

    // An OPERATOR tap is ungated.
    assert!(kernel
        .authorize_state_subscribe(&AgentId::from("cli"))
        .is_none());
}
