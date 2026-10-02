//! The web ws leg as the TRANSPORT of cross-kernel addressing: a page says
//! `hello {guid}`, the host kernel then reaches the page's agents as
//! `<guid>|<agent>`, each call rides this socket as an outbound `call` frame
//! answered by an inbound `reply`/`error` with the same id, and the page stops
//! being an address the moment its socket closes (owner, 2026-09-30).
//!
//! The twin of python `web_ws/tests/test_peer_calls.py`, over a real socket: the
//! test is the page, and sends INTO it from the kernel it is connected to.

use super::*;
use fantastic_kernel::Outcome;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as TMessage;

const HOST: &str = "h0";
const PAGE: &str = "g1";

/// Stage web + its ws leg (open or sealed), give the root its guid, and open a
/// socket on the WEB agent — the leg's own parent, which its baseline lets it
/// watch, so the connect-time auto-watch adds no error frame to the stream.
async fn open(tmp: &TempDir, web_id: &str, open_leg: bool) -> (Arc<Kernel>, TestWs, String) {
    let port = free_port();
    let kernel = stage_web_with_ws_leg(tmp, port, web_id, open_leg).await;
    kernel
        .root()
        .expect("root")
        .meta
        .write()
        .unwrap()
        .insert("guid".to_string(), json!(HOST));
    let ws = connect(port, web_id).await;
    (kernel, ws, format!("{web_id}_ws"))
}

async fn connect(port: u16, web_id: &str) -> TestWs {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web_id}/ws"))
        .await
        .expect("ws connect");
    ws
}

async fn push(ws: &mut TestWs, frame: Value) {
    ws.send(TMessage::Text(frame.to_string()))
        .await
        .expect("send frame");
}

/// The next TEXT frame of type `ftype`, within 5 s.
async fn next_of(ws: &mut TestWs, ftype: &str) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame in time")
            .expect("stream open")
            .expect("frame ok");
        if let TMessage::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).expect("json frame");
            if v["type"] == ftype {
                return v;
            }
        }
    }
}

async fn until(what: &str, pred: impl Fn() -> bool) {
    let end = tokio::time::Instant::now() + Duration::from_secs(3);
    while !pred() {
        assert!(tokio::time::Instant::now() < end, "{what}: not reached");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn send_to_page(kernel: &Arc<Kernel>, payload: Value) -> tokio::task::JoinHandle<Outcome> {
    send_item(
        kernel,
        Item::new(AgentId::from(format!("{PAGE}|canvas")), payload),
    )
}

fn send_item(kernel: &Arc<Kernel>, item: Item) -> tokio::task::JoinHandle<Outcome> {
    let k = Arc::clone(kernel);
    tokio::spawn(async move { k.send(vec![item]).await.remove(0) })
}

async fn settled(h: tokio::task::JoinHandle<Outcome>) -> Outcome {
    tokio::time::timeout(Duration::from_secs(3), h)
        .await
        .expect("the send settles")
        .expect("send task")
}

#[tokio::test]
async fn hello_makes_the_page_an_address_and_a_call_round_trips() {
    let tmp = TempDir::new().unwrap();
    let (kernel, mut ws, leg) = open(&tmp, "web_peer_rt", true).await;
    push(&mut ws, json!({"type": "hello", "guid": PAGE})).await;
    until("peer registered", || kernel.peer(PAGE).is_some()).await;
    assert_eq!(kernel.peer_rows(), json!([{"guid": PAGE, "via": leg}]));
    let send = send_to_page(&kernel, json!({"type": "around", "radius": 2}));
    let call = next_of(&mut ws, "call").await;
    assert_eq!(
        (&call["target"], &call["payload"], &call["caller"]),
        (
            &json!("canvas"),
            &json!({"type": "around", "radius": 2}),
            &json!(format!("{HOST}|kernel"))
        ),
        "{call}"
    );
    push(
        &mut ws,
        json!({"type": "reply", "id": call["id"], "data": {"cells": ["a"]}}),
    )
    .await;
    let out = settled(send).await;
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(out.result, json!({"cells": ["a"]}));
    stop(&kernel, "web_peer_rt").await;
}

#[tokio::test]
async fn a_page_side_refusal_is_the_calls_outcome() {
    let tmp = TempDir::new().unwrap();
    let (kernel, mut ws, _) = open(&tmp, "web_peer_refusal", true).await;
    push(&mut ws, json!({"type": "hello", "guid": PAGE})).await;
    until("peer registered", || kernel.peer(PAGE).is_some()).await;
    let send = send_to_page(&kernel, json!({"type": "around"}));
    let call = next_of(&mut ws, "call").await;
    let refusal = json!({
        "error": "canvas does not accept 'around' from h0|kernel",
        "reason": "unauthorized",
    });
    push(
        &mut ws,
        json!({"type": "error", "id": call["id"], "data": refusal}),
    )
    .await;
    assert_eq!(settled(send).await.result, refusal);
    stop(&kernel, "web_peer_refusal").await;
}

#[tokio::test]
async fn a_disconnect_settles_waiting_calls_and_drops_the_address() {
    let tmp = TempDir::new().unwrap();
    let (kernel, mut ws, _) = open(&tmp, "web_peer_gone", true).await;
    push(&mut ws, json!({"type": "hello", "guid": PAGE})).await;
    until("peer registered", || kernel.peer(PAGE).is_some()).await;
    let send = send_to_page(&kernel, json!({"type": "around"}));
    next_of(&mut ws, "call").await;
    ws.close(None).await.expect("close");
    let out = settled(send).await;
    assert_eq!(out.result["reason"], "transport_error", "{:?}", out.result);
    assert!(
        out.result["error"]
            .as_str()
            .unwrap()
            .contains("disconnected before answering"),
        "{:?}",
        out.result
    );
    assert!(kernel.peer(PAGE).is_none());
    let gone = settled(send_to_page(&kernel, json!({"type": "around"}))).await;
    assert_eq!(gone.result["reason"], "unknown_agent", "{:?}", gone.result);
    stop(&kernel, "web_peer_gone").await;
}

#[tokio::test]
async fn one_guid_per_connection_and_per_kernel() {
    let tmp = TempDir::new().unwrap();
    let (kernel, mut ws, leg) = open(&tmp, "web_peer_pin", true).await;
    let port = kernel
        .agents
        .get(&AgentId::from("web_peer_pin"))
        .and_then(|a| a.meta.read().unwrap().get("port").and_then(Value::as_u64))
        .expect("port") as u16;
    let mut ws2 = connect(port, "web_peer_pin").await;
    push(&mut ws, json!({"type": "hello", "guid": PAGE})).await;
    until("peer registered", || kernel.peer(PAGE).is_some()).await;
    // the same connection cannot become someone else
    push(&mut ws, json!({"type": "hello", "guid": "other"})).await;
    let err = next_of(&mut ws, "error").await;
    assert_eq!(err["arg"], "guid", "{err}");
    assert!(err["error"].as_str().unwrap().contains(PAGE), "{err}");
    // another connection cannot take a guid that is live
    push(&mut ws2, json!({"type": "hello", "guid": PAGE})).await;
    let err2 = next_of(&mut ws2, "error").await;
    assert!(
        err2["error"]
            .as_str()
            .unwrap()
            .contains(&format!("already connected via {leg}")),
        "{err2}"
    );
    // and a malformed guid is refused
    push(&mut ws2, json!({"type": "hello", "guid": "a|b"})).await;
    let err3 = next_of(&mut ws2, "error").await;
    assert_eq!(err3["arg"], "guid", "{err3}");
    stop(&kernel, "web_peer_pin").await;
}

#[tokio::test]
async fn a_sealed_leg_carries_no_peer() {
    let tmp = TempDir::new().unwrap();
    let (kernel, mut ws, _) = open(&tmp, "web_peer_sealed", false).await;
    push(&mut ws, json!({"type": "hello", "guid": PAGE})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(kernel.peer(PAGE).is_none());
    stop(&kernel, "web_peer_sealed").await;
}

/// A raw body rides the call both ways on the binary channel: out at
/// `["payload", "bytes"]` (python's payload dict holds it there), back at
/// `["data", "bytes"]`.
#[tokio::test]
async fn a_raw_body_rides_the_call_both_ways() {
    let tmp = TempDir::new().unwrap();
    let (kernel, mut ws, _) = open(&tmp, "web_peer_bytes", true).await;
    push(&mut ws, json!({"type": "hello", "guid": PAGE})).await;
    until("peer registered", || kernel.peer(PAGE).is_some()).await;
    let send = send_item(
        &kernel,
        Item::new(
            AgentId::from(format!("{PAGE}|files")),
            json!({"type": "write_stream", "path": "a.bin"}),
        )
        .with_binary_key("bytes", b"ping".to_vec()),
    );
    let wire = loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame in time")
            .expect("stream open")
            .expect("frame ok");
        if let TMessage::Binary(b) = msg {
            break b;
        }
    };
    let head = wire_head(&wire);
    assert_eq!(head["type"], "call", "{head}");
    assert_eq!(head["_binary_path"], json!(["payload", "bytes"]), "{head}");
    assert_eq!(head["payload"]["bytes"], Value::Null, "{head}");
    let body = fantastic_io_bridge::codec::decode_binary_frame(&wire)
        .expect("binary frame")
        .body;
    assert_eq!(body, b"ping");
    let reply = json!({"type": "reply", "id": head["id"], "data": {"ok": true, "bytes": null},
                       "_binary_path": ["data", "bytes"]});
    ws.send(TMessage::Binary(build_binary_frame(&reply, b"pong")))
        .await
        .expect("send reply");
    let out = settled(send).await;
    assert_eq!(out.result["ok"], true, "{:?}", out.result);
    assert_eq!(
        out.body,
        Some(fantastic_kernel::Blob::key("bytes", b"pong".to_vec()))
    );
    stop(&kernel, "web_peer_bytes").await;
}
