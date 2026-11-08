//! The leg tells its watchers when a socket opens and when it closes, so the side
//! that was dialed (a box a master reaches over a tunnel) knows its link without a
//! call: `socket_open {client_id}`, then `socket_closed {client_id, guid}`.

use super::*;
use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::Message as TMessage;

/// The next event of `ty` on `rx`, within 5 s.
async fn next_of(rx: &mut fantastic_kernel::inbox::InboxReceiver, ty: &str) -> Value {
    let end = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        while let Some(item) = rx.try_recv() {
            if item.payload["type"] == json!(ty) {
                return item.payload;
            }
        }
        assert!(tokio::time::Instant::now() < end, "no `{ty}` in time");
        let _ = tokio::time::timeout(Duration::from_millis(100), rx.ready()).await;
    }
}

#[tokio::test]
async fn the_leg_emits_socket_open_and_socket_closed() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_socket_events";
    let leg = AgentId::from(format!("{web_id}_ws").as_str());
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    let watcher = AgentId::from("leg_watcher");
    let mut rx = kernel.ensure_inbox(&watcher);
    kernel.watch(&leg, watcher.clone()).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web_id}/ws"))
        .await
        .expect("ws connect");
    let open = next_of(&mut rx, "socket_open").await;
    let client = open["client_id"].as_str().expect("a client id").to_string();
    assert!(!client.is_empty(), "{open}");

    ws.send(TMessage::Text(json!({"type": "hello", "guid": "g-peer"}).to_string()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    ws.close(None).await.unwrap();
    let closed = next_of(&mut rx, "socket_closed").await;
    assert_eq!(closed["client_id"], json!(client), "{closed}");
    assert_eq!(closed["guid"], json!("g-peer"), "{closed}");
}
