//! A ws MESSAGE is bounded by io_bridge's one frame cap, not only a frame (audit
//! W-3): tungstenite's default message cap is 64 MiB, so a call fragmented into
//! frames under the 16 MiB frame cap was buffered, parsed and dispatched at four
//! times what python's server (uvicorn's `ws_max_size`) allows. A fragmented call
//! under the cap is still answered.

use super::*;
use fantastic_io_bridge::codec::MAX_FRAME;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::Message as TMessage;

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A `call` frame (reflect `kernel`) of EXACTLY `len` bytes, padded in the payload.
fn call_of_len(id: &str, len: usize) -> Vec<u8> {
    let frame = |pad: &str| {
        json!({"type": "call", "id": id, "target": "kernel",
               "payload": {"type": "reflect", "tree": "none", "pad": pad}})
        .to_string()
    };
    let base = frame("").len();
    frame(&"x".repeat(len - base)).into_bytes()
}

/// `msg` as two fragments — a text frame, then its final continuation — each
/// under the frame cap.
async fn send_in_two(ws: &mut Client, msg: &[u8]) {
    let half = msg.len() / 2;
    let first = Frame::message(msg[..half].to_vec(), OpCode::Data(Data::Text), false);
    let last = Frame::message(msg[half..].to_vec(), OpCode::Data(Data::Continue), true);
    ws.send(TMessage::Frame(first)).await.unwrap();
    ws.send(TMessage::Frame(last)).await.unwrap();
}

/// The answer to `id` (its reply or error frame), or `None` when the socket
/// ended (close, error, EOF) without one.
async fn reply_to(ws: &mut Client, id: &str) -> Option<Value> {
    loop {
        let next = tokio::time::timeout(Duration::from_secs(20), ws.next())
            .await
            .expect("an answer or the end of the socket within 20 s");
        match next {
            Some(Ok(TMessage::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["id"] == id {
                    return Some(v);
                }
            }
            Some(Ok(TMessage::Close(_))) | Some(Err(_)) | None => return None,
            Some(Ok(_)) => {}
        }
    }
}

#[tokio::test]
async fn a_fragmented_message_is_bounded_by_the_one_frame_cap() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_msg_cap";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");

    // under the cap, in two fragments: reassembled and answered
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send_in_two(&mut ws, &call_of_len("small", 4096)).await;
    let reply = reply_to(&mut ws, "small").await;
    assert!(
        reply.is_some(),
        "a fragmented call under the cap is answered"
    );

    // one byte over the cap, every fragment well under the frame cap: the
    // socket ends, the call is never dispatched
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send_in_two(&mut ws, &call_of_len("big", MAX_FRAME + 1)).await;
    let reply = reply_to(&mut ws, "big").await;
    assert!(
        reply.is_none(),
        "a message over the one frame cap was dispatched"
    );

    stop(&kernel, web_id).await;
}
