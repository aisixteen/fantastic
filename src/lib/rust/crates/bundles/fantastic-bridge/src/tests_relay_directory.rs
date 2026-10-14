//! Relay DIRECTORY events reach the connector's inbox through the engine — the
//! twin of python `test_watch_directory_re_emits_directory_events_on_the_connector_inbox`.
//!
//! The relay transport tags a directory event `src:"relay"` (the engine re-emits
//! only a subscribed `src`), `watch_directory` subscribes the leg to it (the
//! engine's `watching`), and `unwatch_directory` withdraws it.

use super::*;
use futures_util::{SinkExt, StreamExt};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

/// A relay that acks a directory `watch` and pushes every payload the test hands
/// it as a `{type:"event", source:"relay", payload}` directory event.
async fn directory_relay() -> (String, tokio::sync::mpsc::Sender<Value>) {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (push, mut pushed) = tokio::sync::mpsc::channel::<Value>(8);
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        #[allow(clippy::result_large_err)]
        let cb = |_req: &Request, mut resp: Response| {
            resp.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_static(transport::relay::SUBPROTOCOL),
            );
            Ok(resp)
        };
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, cb)
            .await
            .unwrap();
        loop {
            tokio::select! {
                msg = ws.next() => {
                    let Some(Ok(Message::Text(t))) = msg else { return };
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v["target"] == "relay" && v["type"] == "watch" {
                        let ack = json!({"type": "reply", "id": v["id"], "data": {"ok": true}});
                        ws.send(Message::Text(ack.to_string())).await.unwrap();
                    }
                }
                Some(payload) = pushed.recv() => {
                    let ev = json!({"type": "event", "source": "relay", "payload": payload});
                    ws.send(Message::Text(ev.to_string())).await.unwrap();
                }
            }
        }
    });
    (format!("ws://127.0.0.1:{port}"), push)
}

#[tokio::test]
async fn watch_directory_re_emits_directory_events_on_the_connector_inbox() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(RELAY_HANDLER_MODULE, RelayConnectorBundle);
    let kernel = Arc::new(kernel);
    let root = fantastic_kernel::Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let leg = format!(
        "w10_leg_{}",
        tmp.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace('.', "_")
    );
    let leg_id = AgentId::from(leg.as_str());
    let ask = |payload: Value| {
        let kernel = Arc::clone(&kernel);
        let leg_id = leg_id.clone();
        async move {
            kernel
                .send(vec![Item::new(leg_id, payload)])
                .await
                .remove(0)
                .result
        }
    };
    let made = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel_state",
                   "handler_module": RELAY_HANDLER_MODULE, "id": leg,
                   "transport": "memory", "partner_guid": "B"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], json!(leg), "{made}");

    let (url, push) = directory_relay().await;
    let no_password: transport::relay::PasswordReader =
        Arc::new(|| Box::pin(async { Ok(String::new()) }));
    let relay = RelayTransport::connect(
        &url,
        "A",
        no_password,
        "B",
        0.0,
        0.0,
        json!({}),
        RelaySpawn::new(&kernel, &leg_id),
    )
    .await
    .expect("the fake relay accepts");
    injected_map().insert(leg_id.clone(), relay);
    let mut rx = kernel.ensure_inbox(&leg_id);
    assert_eq!(
        ask(json!({"type": "boot"})).await,
        json!({"booted": true, "transport": "memory"})
    );

    let directory_events = |rx: &mut fantastic_kernel::inbox::InboxReceiver| {
        let mut seen: Vec<Value> = Vec::new();
        while let Some(v) = rx.try_recv() {
            if v.payload["type"]
                .as_str()
                .is_some_and(|t| t.starts_with("peer_"))
            {
                seen.push(v.payload);
            }
        }
        seen
    };
    let settle = || tokio::time::sleep(std::time::Duration::from_millis(150));

    // before the subscription: dropped
    push.send(json!({"type": "peer_left", "guid": "X"}))
        .await
        .unwrap();
    settle().await;
    assert_eq!(directory_events(&mut rx), Vec::<Value>::new());

    assert_eq!(
        ask(json!({"type": "watch_directory"})).await,
        json!({"ok": true})
    );
    push.send(json!({"type": "peer_joined", "guid": "C"}))
        .await
        .unwrap();
    push.send(json!({"type": "peer_status", "guid": "C", "status": "red"}))
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        directory_events(&mut rx),
        vec![
            json!({"type": "peer_joined", "guid": "C", "src": "relay"}),
            json!({"type": "peer_status", "guid": "C", "status": "red", "src": "relay"}),
        ]
    );

    assert_eq!(
        ask(json!({"type": "unwatch_directory"})).await,
        json!({"ok": true, "unwatched": "relay"})
    );
    push.send(json!({"type": "peer_evicted", "guid": "C"}))
        .await
        .unwrap();
    settle().await;
    assert_eq!(directory_events(&mut rx), Vec::<Value>::new());

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": leg}),
        )])
        .await;
}
