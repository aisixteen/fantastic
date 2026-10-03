//! A WS MESSAGE is bounded by io_bridge's one frame cap on every dialled socket
//! (audit W-3): tungstenite's default message cap is 64 MiB, so a peer's message
//! fragmented into frames under the 16 MiB frame cap reached the leg at four
//! times python's `max_size`. Both dialled transports — ws_bridge's and
//! relay_connector's — now end the socket on it; the same delivery under the cap,
//! fragmented, still arrives whole.

use super::transport::relay::{PasswordReader, RelaySpawn, RelayTransport, SUBPROTOCOL};
use super::transport::{ws::WsTransport, BridgeTransport, Frame, TransportError};
use fantastic_io_bridge::codec::MAX_FRAME;
use fantastic_kernel::{AgentId, Kernel};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
use tokio_tungstenite::tungstenite::protocol::frame::Frame as WsFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// `envelope(pad)` padded to EXACTLY `len` bytes.
fn of_len(envelope: impl Fn(&str) -> Value, len: usize) -> Vec<u8> {
    let base = envelope("").to_string().len();
    envelope(&"x".repeat(len - base)).to_string().into_bytes()
}

/// A reply to the bridge's call `A:1`, padded.
fn reply(pad: &str) -> Value {
    json!({"type": "reply", "id": "A:1", "data": {"pad": pad}})
}

/// The same reply as the relay delivers it from the partner `B`.
fn relayed_reply(pad: &str) -> Value {
    json!({"type": "event", "source": "B", "payload": reply(pad)})
}

/// A peer that, once dialled, sends `msg` as two fragments — a text frame, then
/// its final continuation — each under the frame cap, and holds the socket open.
/// `subprotocol` answers the relay's pairing header.
async fn peer_sending(msg: Vec<u8>, subprotocol: bool) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        #[allow(clippy::result_large_err)]
        let cb = move |_req: &Request, mut resp: Response| {
            if subprotocol {
                resp.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    HeaderValue::from_static(SUBPROTOCOL),
                );
            }
            Ok(resp)
        };
        let mut ws: WebSocketStream<TcpStream> = tokio_tungstenite::accept_hdr_async(stream, cb)
            .await
            .unwrap();
        let half = msg.len() / 2;
        let first = WsFrame::message(msg[..half].to_vec(), OpCode::Data(Data::Text), false);
        let last = WsFrame::message(msg[half..].to_vec(), OpCode::Data(Data::Continue), true);
        ws.send(Message::Frame(first)).await.unwrap();
        ws.send(Message::Frame(last)).await.unwrap();
        while let Some(Ok(_)) = ws.next().await {}
    });
    (port, server)
}

async fn next_frame(t: &dyn BridgeTransport) -> Result<Frame, TransportError> {
    tokio::time::timeout(Duration::from_secs(20), t.recv_frame())
        .await
        .expect("a frame or the end of the socket within 20 s")
}

#[tokio::test]
async fn the_ws_transport_bounds_a_fragmented_message_by_the_one_frame_cap() {
    // under the cap: reassembled, delivered whole
    let (port, server) = peer_sending(of_len(reply, 4096), false).await;
    let t = WsTransport::connect(&format!("ws://127.0.0.1:{port}/peer/ws"), false)
        .await
        .expect("connect");
    let f = next_frame(t.as_ref())
        .await
        .expect("a fragmented message arrives");
    assert_eq!(f.into_value()["id"], json!("A:1"));
    t.close().await;
    server.abort();

    // one byte over: the reader ends on the cap, nothing is delivered
    let (port, server) = peer_sending(of_len(reply, MAX_FRAME + 1), false).await;
    let t = WsTransport::connect(&format!("ws://127.0.0.1:{port}/peer/ws"), false)
        .await
        .expect("connect");
    match next_frame(t.as_ref()).await {
        Err(TransportError::ConnectionClosed(m)) => {
            assert!(m.contains("Message too long"), "{m}");
        }
        other => panic!("a message over the one frame cap was delivered: {other:?}"),
    }
    server.abort();
}

/// A one-shot relay leg (reconnect 0) dialling `port` as `A`, paired with `B`:
/// a drop is terminal, so the end of its socket is observable.
async fn one_shot_leg(kernel: &Arc<Kernel>, port: u16) -> Arc<RelayTransport> {
    let no_password: PasswordReader = Arc::new(|| Box::pin(async { Ok(String::new()) }));
    RelayTransport::connect(
        &format!("ws://127.0.0.1:{port}"),
        "A",
        no_password,
        "B",
        0.0,
        0.0,
        json!({}),
        RelaySpawn::new(kernel, &AgentId::from("relay")),
    )
    .await
    .expect("dial")
}

#[tokio::test]
async fn the_relay_transport_bounds_a_fragmented_message_by_the_one_frame_cap() {
    let kernel = Arc::new(Kernel::new());

    // under the cap: reassembled, the partner's reply delivered
    let (port, server) = peer_sending(of_len(relayed_reply, 4096), true).await;
    let t = one_shot_leg(&kernel, port).await;
    let f = next_frame(t.as_ref())
        .await
        .expect("a fragmented delivery arrives");
    assert_eq!(f.into_value()["id"], json!("A:1"));
    t.close().await;
    server.abort();

    // one byte over: the socket ends on the cap, nothing is delivered
    let (port, server) = peer_sending(of_len(relayed_reply, MAX_FRAME + 1), true).await;
    let t = one_shot_leg(&kernel, port).await;
    match next_frame(t.as_ref()).await {
        Err(TransportError::ConnectionClosed(m)) => assert_eq!(m, "relay closed"),
        other => panic!("a message over the one frame cap was delivered: {other:?}"),
    }
    server.abort();
}
