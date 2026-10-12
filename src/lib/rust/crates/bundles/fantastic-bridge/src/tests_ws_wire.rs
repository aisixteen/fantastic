//! The ws transport ON THE WIRE, against a peer that speaks as python's
//! `websockets.serve` does: the client's offer, the server's answer
//! (`permessage-deflate; server_max_window_bits=12; client_max_window_bits=12`
//! — its default), deflated frames both ways, and the closing handshake's text.

use super::transport::{ws::WsTransport, BridgeTransport, TransportError};
use fantastic_io_bridge::ws_deflate::{Params, PerMessageDeflate};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What `websockets.serve` negotiates by default: a 4096-byte window each way.
const PYTHON_SERVE_ANSWER: &str =
    "permessage-deflate; server_max_window_bits=12; client_max_window_bits=12";

/// The server's side of that negotiation: it decodes the client (remote, 12)
/// and encodes with its own window (local, 12).
fn server_pmd() -> PerMessageDeflate {
    PerMessageDeflate::new(Params {
        remote_no_context_takeover: false,
        local_no_context_takeover: false,
        remote_max_window_bits: 12,
        local_max_window_bits: 12,
    })
    .unwrap()
}

/// Read the request head; answer the upgrade with `extensions` (none = no
/// header). Returns the request head.
async fn handshake(sock: &mut TcpStream, extensions: Option<&str>) -> String {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        sock.read_exact(&mut b).await.unwrap();
        head.push(b[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let key = head
        .lines()
        .find_map(|l| l.strip_prefix("sec-websocket-key: "))
        .or_else(|| {
            head.lines()
                .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
        })
        .expect("a key")
        .trim()
        .to_string();
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    let mut resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n"
    );
    if let Some(e) = extensions {
        resp.push_str(&format!("Sec-WebSocket-Extensions: {e}\r\n"));
    }
    resp.push_str("\r\n");
    sock.write_all(resp.as_bytes()).await.unwrap();
    head
}

/// One whole frame off the socket: `(first byte, unmasked payload)`.
async fn read_frame(sock: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut h = [0u8; 2];
    sock.read_exact(&mut h).await.unwrap();
    let mut len = usize::from(h[1] & 0x7f);
    if len == 126 {
        let mut b = [0u8; 2];
        sock.read_exact(&mut b).await.unwrap();
        len = usize::from(u16::from_be_bytes(b));
    } else if len == 127 {
        let mut b = [0u8; 8];
        sock.read_exact(&mut b).await.unwrap();
        len = usize::try_from(u64::from_be_bytes(b)).unwrap();
    }
    let mut mask = [0u8; 4];
    let masked = h[1] & 0x80 != 0;
    if masked {
        sock.read_exact(&mut mask).await.unwrap();
    }
    let mut p = vec![0u8; len];
    sock.read_exact(&mut p).await.unwrap();
    if masked {
        for (i, b) in p.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
    }
    (h[0], p)
}

/// An unmasked server frame.
async fn write_frame(sock: &mut TcpStream, b0: u8, payload: &[u8]) {
    let mut f = vec![b0];
    assert!(payload.len() < 126);
    f.push(payload.len() as u8);
    f.extend_from_slice(payload);
    sock.write_all(&f).await.unwrap();
}

/// The client offers what `websockets.connect` offers, takes python's answer,
/// and from then on every message both ways is deflated: its frames leave with
/// RSV1 and inflate under the negotiated window; the server's deflated frames
/// arrive whole.
#[tokio::test]
async fn the_client_offers_deflate_and_speaks_it_as_python_s_client_does() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let head = handshake(&mut sock, Some(PYTHON_SERVE_ANSWER)).await;
        let mut pmd = server_pmd();
        // the client's first message: compressed, RSV1 on its frame
        let (b0, payload) = read_frame(&mut sock).await;
        assert_eq!(b0, 0xc1, "FIN + RSV1 + text");
        let got = pmd.decode(0x1, true, true, &payload).unwrap().unwrap();
        // the server's answer, deflated as python's server sends it
        let (wire, _) = pmd
            .encode(
                0x1,
                true,
                br#"{"type": "reply", "id": "c1", "data": {"ok": true}}"#,
            )
            .unwrap()
            .unwrap();
        write_frame(&mut sock, 0xc1, &wire).await;
        (head, got)
    });
    let t = WsTransport::connect(&format!("ws://127.0.0.1:{port}/peer/ws"))
        .await
        .expect("connect");
    t.send_frame(
        json!({"type": "call", "id": "c1", "target": "x", "payload": {"type": "reflect"}}),
    )
    .await
    .unwrap();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), t.recv_frame())
        .await
        .expect("a reply in time")
        .expect("the reply decodes");
    assert_eq!(
        frame.into_value(),
        json!({"type": "reply", "id": "c1", "data": {"ok": true}})
    );
    let (head, got) = server.await.unwrap();
    assert!(
        head.lines().any(|l| l.eq_ignore_ascii_case(
            "sec-websocket-extensions: permessage-deflate; client_max_window_bits"
        )),
        "python's offer: {head}"
    );
    assert_eq!(
        got,
        br#"{"type": "call", "id": "c1", "target": "x", "payload": {"type": "reflect"}}"#
    );
}

/// A server that answers no extension gets plain frames.
#[tokio::test]
async fn a_server_that_declines_gets_plain_frames() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        handshake(&mut sock, None).await;
        read_frame(&mut sock).await
    });
    let t = WsTransport::connect(&format!("ws://127.0.0.1:{port}/peer/ws"))
        .await
        .expect("connect");
    t.send_frame(json!({"a": 1})).await.unwrap();
    let (b0, payload) = server.await.unwrap();
    assert_eq!(b0, 0x81, "FIN + text, no RSV1");
    assert_eq!(payload, br#"{"a": 1}"#);
}

/// A send after the PEER closed fails with python's `ConnectionClosed` text:
/// what was received, then the echo this side sent (A7.17/A7.30).
#[tokio::test]
async fn a_send_after_the_peer_closed_reports_the_closing_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        handshake(&mut sock, None).await;
        let mut close = 1001u16.to_be_bytes().to_vec();
        close.extend_from_slice(b"peer going away");
        write_frame(&mut sock, 0x88, &close).await;
        // the echo: the same code and reason
        read_frame(&mut sock).await
    });
    let t = WsTransport::connect(&format!("ws://127.0.0.1:{port}/peer/ws"))
        .await
        .expect("connect");
    match t.recv_frame().await {
        Err(TransportError::ConnectionClosed(_)) => {}
        other => panic!("the peer's close ends the reader: {other:?}"),
    }
    let (b0, echo) = server.await.unwrap();
    assert_eq!(b0, 0x88);
    assert_eq!(&echo[..2], &1001u16.to_be_bytes());
    assert_eq!(&echo[2..], b"peer going away");
    match t.send_frame(json!({"type": "call"})).await {
        Err(TransportError::ConnectionClosed(m)) => assert_eq!(
            m,
            "received 1001 (going away) peer going away; then sent 1001 (going away) peer going away"
        ),
        other => panic!("a send on a closed socket must fail: {other:?}"),
    }
}

/// A send after THIS side closed: python's `close()` sends 1000, waits for the
/// peer's echo, and the next send reports both.
#[tokio::test]
async fn a_send_after_closing_reports_what_was_sent_then_received() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        handshake(&mut sock, None).await;
        let (b0, payload) = read_frame(&mut sock).await;
        // echo the client's close
        write_frame(&mut sock, b0, &payload).await;
        payload
    });
    let t = WsTransport::connect(&format!("ws://127.0.0.1:{port}/peer/ws"))
        .await
        .expect("connect");
    t.close().await;
    assert_eq!(
        server.await.unwrap(),
        1000u16.to_be_bytes(),
        "1000, no reason"
    );
    match t.send_frame(json!({"type": "call"})).await {
        Err(TransportError::ConnectionClosed(m)) => {
            assert_eq!(m, "sent 1000 (OK); then received 1000 (OK)");
        }
        other => panic!("a send on a closed socket must fail: {other:?}"),
    }
}

/// `closed` FOLLOWS THE SOCKET (audit W-7): false while open, true once the peer's
/// close frame arrived — python's `state in (CLOSING, CLOSED)`.
#[tokio::test]
async fn closed_follows_the_peer_s_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (go, wait) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        handshake(&mut sock, None).await;
        wait.await.unwrap();
        write_frame(&mut sock, 0x88, &1000u16.to_be_bytes()).await;
        read_frame(&mut sock).await
    });
    let t = WsTransport::connect(&format!("ws://127.0.0.1:{port}/peer/ws"))
        .await
        .expect("connect");
    assert!(!t.closed(), "an open socket is not closed");
    go.send(()).unwrap();
    assert!(matches!(
        t.recv_frame().await,
        Err(TransportError::ConnectionClosed(_))
    ));
    assert!(t.closed(), "the peer closed the socket");
    server.await.unwrap();
}

/// An ABRUPT drop (no close frame, the TCP connection just ends) closes it too —
/// python's `CLOSED`.
#[tokio::test]
async fn closed_follows_an_abrupt_drop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (go, wait) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        handshake(&mut sock, None).await;
        wait.await.unwrap();
        drop(sock);
    });
    let t = WsTransport::connect(&format!("ws://127.0.0.1:{port}/peer/ws"))
        .await
        .expect("connect");
    assert!(!t.closed());
    go.send(()).unwrap();
    assert!(matches!(
        t.recv_frame().await,
        Err(TransportError::ConnectionClosed(_))
    ));
    assert!(t.closed(), "the socket ended");
    server.await.unwrap();
}

/// A ws LEG whose remote dropped reflects `connected:false`, refuses a forward as
/// not connected, and `boot` DIALS AGAIN instead of answering `{already:true}`
/// (audit W-7 — only `reconnect` used to recover it).
#[tokio::test]
async fn a_dropped_leg_reflects_down_and_boot_dials_again() {
    use fantastic_kernel::{Agent, AgentId, Item, Kernel};
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // The remote: every connection stays open until the test drops it.
    let (drop_tx, mut drop_rx) = tokio::sync::mpsc::channel::<()>(4);
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = Arc::clone(&accepted);
    let server = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            tokio::select! {
                conn = listener.accept() => {
                    let (mut sock, _) = conn.unwrap();
                    handshake(&mut sock, None).await;
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    held.push(sock);
                }
                Some(()) = drop_rx.recv() => {
                    let mut sock = held.remove(0);
                    write_frame(&mut sock, 0x88, &1000u16.to_be_bytes()).await;
                }
            }
        }
    });

    let tmp = tempfile::TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel
        .bundles
        .register(super::WS_HANDLER_MODULE, super::WsBridgeBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        serde_json::Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let leg = format!(
        "w7_leg_{}",
        tmp.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace('.', "_")
    );
    let ask = |payload: serde_json::Value| {
        let kernel = Arc::clone(&kernel);
        let leg = leg.clone();
        async move {
            kernel
                .send(vec![Item::new(AgentId::from(leg.as_str()), payload)])
                .await
                .remove(0)
                .result
        }
    };
    let made = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": super::WS_HANDLER_MODULE, "id": leg,
                "transport": "ws", "host": "127.0.0.1", "port": port, "peer_id": "peer",
            }),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(made["id"], json!(leg), "{made}");

    // create_agent booted the leg; a live socket makes `boot` a no-op.
    assert_eq!(
        ask(json!({"type": "boot"})).await,
        json!({"already": true, "transport": "ws"})
    );
    assert_eq!(
        ask(json!({"type": "reflect"})).await["connected"],
        json!(true)
    );

    drop_tx.send(()).await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while ask(json!({"type": "reflect"})).await["connected"] != json!(false) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "reflect kept saying connected after the remote closed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let fwd = ask(json!({"type": "forward", "target": "x", "payload": {"type": "reflect"}})).await;
    assert_eq!(fwd["reason"], json!("not_connected"), "{fwd}");

    let dials = accepted.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        ask(json!({"type": "boot"})).await,
        json!({"booted": true, "transport": "ws"})
    );
    assert_eq!(
        accepted.load(std::sync::atomic::Ordering::SeqCst),
        dials + 1
    );
    assert_eq!(
        ask(json!({"type": "reflect"})).await["connected"],
        json!(true)
    );

    // the remote goes away first, so the leg's close finds the socket gone
    server.abort();
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": leg}),
        )])
        .await;
}
