//! A ws leg to a NON-LOOPBACK host is SEALED for secrets (owner, 2026-10-03,
//! audit S8) — the twin of python `ws_bridge/tests/test_ws_seal.py`.
//!
//! `ws://` carries every frame in clear, and a forward's secret values ride its
//! frame verbatim (the far kernel needs them). So a leg dialed to a host other
//! than this machine (`localhost` / `127.0.0.1` / `[::1]` — an ssh tunnel's local
//! end) refuses any call carrying secret marks, in both directions, with the relay
//! seal's shape: outbound, a marked `forward` is refused before a frame leaves;
//! inbound, a `call` frame naming `secret` is answered with the same refusal and
//! never dispatched.
//!
//! The peer is a local websocket server; a sealed transport (python's
//! monkeypatched dial to a non-loopback host) is dialed to it and handed to the leg
//! through the injection seam.

use super::*;
use fantastic_kernel::Agent;
use futures_util::{SinkExt, StreamExt};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

const KEY: &str = "sk-ws-seal-0123456789abcdef";

/// python `_SEALED`.
fn sealed() -> Value {
    json!({
        "error": transport::ws::SECRET_SEALED,
        "reason": "unauthorized",
        "hint": transport::ws::SECRET_SEALED_HINT,
    })
}

/// A websocket peer on loopback: records every text frame it receives, answers
/// each `call` with `{ok: true}`, and sends whatever a test pushes.
struct Peer {
    port: u16,
    seen: Arc<Mutex<Vec<Value>>>,
    push: mpsc::UnboundedSender<Value>,
    task: JoinHandle<()>,
}

async fn peer() -> Peer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let s = Arc::clone(&seen);
    let (push, mut rx) = mpsc::unbounded_channel::<Value>();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut sink, mut source) = ws.split();
        loop {
            tokio::select! {
                msg = source.next() => match msg {
                    Some(Ok(Message::Text(t))) => {
                        let v: Value = serde_json::from_str(&t).unwrap();
                        s.lock().unwrap().push(v.clone());
                        if v["type"] == "call" {
                            let reply = json!({"type": "reply", "id": v["id"], "data": {"ok": true}});
                            sink.send(Message::Text(reply.to_string())).await.unwrap();
                        }
                    }
                    Some(Ok(_)) => continue,
                    _ => break,
                },
                Some(v) = rx.recv() => {
                    sink.send(Message::Text(v.to_string())).await.unwrap();
                }
            }
        }
    });
    Peer {
        port,
        seen,
        push,
        task,
    }
}

async fn mk_kernel(tmp: &TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(WS_HANDLER_MODULE, WsBridgeBundle);
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
}

/// A unique leg id — `BRIDGES` and the injection seam are process-wide.
fn leg_id(tmp: &TempDir) -> String {
    let suffix = tmp
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().replace('.', "_"))
        .unwrap_or_default();
    format!("ws_seal_{suffix}")
}

/// A leg whose live transport is `t` (the injection seam), booted.
async fn leg_over(k: &Arc<Kernel>, id: &str, t: Arc<dyn BridgeTransport>) {
    injected_map().insert(AgentId::from(id), t);
    let made = k
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel_state",
                   "handler_module": WS_HANDLER_MODULE, "id": id,
                   "transport": "memory", "peer_id": "web_ws"}),
        )])
        .await
        .remove(0);
    assert_eq!(made.result["id"], json!(id), "{:?}", made.result);
    let r = k
        .send(vec![Item::new(AgentId::from(id), json!({"type": "boot"}))])
        .await
        .remove(0)
        .result;
    assert!(
        r["booted"] == json!(true) || r["already"] == json!(true),
        "{r}"
    );
}

async fn forward(k: &Arc<Kernel>, id: &str, secret: Option<Value>) -> fantastic_kernel::Outcome {
    let item = Item::new(
        AgentId::from(id),
        json!({"type": "forward", "target": "vault",
               "payload": {"type": "set_key", "key": KEY}, "timeout": 2}),
    );
    let item = match secret {
        Some(s) => item.with_secret(s),
        None => item,
    };
    k.send(vec![item]).await.remove(0)
}

#[test]
fn the_dialed_host_decides_the_seal() {
    for (host, sealed) in [
        ("10.0.0.5", true),
        ("peer.example.test", true),
        ("0.0.0.0", true),
        ("localhost", false),
        ("127.0.0.1", false),
        ("[::1]", false),
    ] {
        assert_eq!(transport::ws::host_is_sealed(host), sealed, "{host}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marked_forward_to_a_non_loopback_host_is_refused_before_a_frame_leaves() {
    for secret in [
        json!(["payload.payload.key"]),
        json!(["result"]),
        json!(["payload.timeout"]),
        json!([["payload", "payload", "key"]]),
    ] {
        let tmp = TempDir::new().unwrap();
        let k = mk_kernel(&tmp).await;
        let p = peer().await;
        let t = transport::ws::WsTransport::connect(
            &format!("ws://127.0.0.1:{}/web_ws/ws", p.port),
            true,
        )
        .await
        .expect("connect");
        let id = leg_id(&tmp);
        leg_over(&k, &id, t).await;
        let out = forward(&k, &id, Some(secret.clone())).await;
        assert_eq!(out.status.as_str(), "errored", "{secret}: {:?}", out.result);
        assert_eq!(out.result, sealed(), "{secret}");
        // the same leg still carries an UNMARKED forward — the seal is about marks
        let plain = forward(&k, &id, None).await;
        assert_eq!(plain.result, json!({"ok": true}), "{secret}");
        let seen = p.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "only the unmarked call crossed: {seen:?}");
        assert_eq!(seen[0]["type"], "call");
        assert!(seen[0].get("secret").is_none(), "{seen:?}");
        p.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marked_forward_over_a_loopback_leg_carries_its_marks() {
    let tmp = TempDir::new().unwrap();
    let k = mk_kernel(&tmp).await;
    let p = peer().await;
    let id = leg_id(&tmp);
    // the real build path: a loopback `host` dials an unsealed leg
    let made = k
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel_state",
                   "handler_module": WS_HANDLER_MODULE, "id": id,
                   "host": "127.0.0.1", "port": p.port, "peer_id": "web_ws"}),
        )])
        .await
        .remove(0);
    assert_eq!(made.result["id"], json!(id), "{:?}", made.result);
    let r = k
        .send(vec![Item::new(
            AgentId::from(id.as_str()),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        r["booted"] == json!(true) || r["already"] == json!(true),
        "{r}"
    );
    let out = forward(&k, &id, Some(json!(["payload.payload.key"]))).await;
    assert_eq!(out.result, json!({"ok": true}), "{:?}", out.result);
    let seen = p.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0]["secret"], json!(["payload.key"]));
    assert_eq!(seen[0]["payload"]["key"], json!(KEY));
    p.task.abort();
}

/// Inbound: on a sealed leg a `call` frame naming `secret` is answered with the
/// seal's refusal and never reaches the engine; the next frame is read on. A
/// loopback leg hands the call to the engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inbound_marked_call_on_a_non_loopback_leg_is_answered_sealed() {
    for is_sealed in [true, false] {
        let p = peer().await;
        let t = transport::ws::WsTransport::connect(
            &format!("ws://127.0.0.1:{}/web_ws/ws", p.port),
            is_sealed,
        )
        .await
        .expect("connect");
        let call = json!({"type": "call", "id": "sealed-1", "target": "vault",
                          "payload": {"type": "set_key", "key": KEY},
                          "caller": "peer-g|kernel", "secret": ["payload.key"]});
        let after = json!({"type": "reply", "id": "x-1", "data": {"ok": true}});
        p.push.send(call.clone()).unwrap();
        p.push.send(after.clone()).unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), t.recv_frame())
            .await
            .expect("a frame in time")
            .expect("a frame")
            .into_value();
        if is_sealed {
            assert_eq!(got, after);
            // the seal's reply reaches the peer
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while p.seen.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                p.seen.lock().unwrap().clone(),
                vec![json!({"type": "reply", "id": "sealed-1", "data": sealed()})]
            );
        } else {
            assert_eq!(got, call);
            assert!(p.seen.lock().unwrap().is_empty());
        }
        t.close().await;
        p.task.abort();
    }
}

#[test]
fn the_forward_doc_names_the_seal() {
    let doc = Family::Ws.verbs()["forward"].as_str().unwrap().to_string();
    assert!(
        doc.ends_with(
            " SEALED for secrets to a non-loopback host: a call carrying any secret mark \
             is refused — ws:// carries every frame in clear."
        ),
        "{doc}"
    );
}
