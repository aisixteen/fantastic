//! The ws leg's handshake negotiates permessage-deflate as uvicorn's websockets
//! server does, and then speaks it. Every expected answer was measured against
//! uvicorn 0.46 / websockets 16.0 with the same raw upgrade request.

use super::*;
use fantastic_io_bridge::ws_deflate::{Params, PerMessageDeflate};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A raw upgrade request (python's capture client's shape), with `offer` as
/// its `Sec-WebSocket-Extensions` when given.
async fn upgrade(port: u16, path: &str, offer: Option<&str>) -> (TcpStream, String, Vec<u8>) {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n"
    );
    if let Some(o) = offer {
        req.push_str(&format!("Sec-WebSocket-Extensions: {o}\r\n"));
    }
    req.push_str("\r\n");
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        sock.read_exact(&mut b).await.unwrap();
        head.push(b[0]);
    }
    let head = String::from_utf8(head).unwrap();
    // a refused upgrade carries a body; read what its length says
    let len: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(str::to_string)
        })
        .map_or(0, |v| v.trim().parse().unwrap());
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).await.unwrap();
    (sock, head, body)
}

/// The header block as `(lowercased name, value)`, `date` blanked.
fn header_block(head: &str) -> Vec<(String, String)> {
    head.lines()
        .skip(1)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (n, v) = l.split_once(": ").unwrap();
            let n = n.to_ascii_lowercase();
            let v = if n == "date" {
                "<DATE>".to_string()
            } else {
                v.to_string()
            };
            (n, v)
        })
        .collect()
}

fn hb(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
        .collect()
}

#[tokio::test]
async fn the_upgrade_answers_each_offer_as_uvicorn_does() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_pmd_hs";
    let kernel = stage_web_with_ws_leg(&tmp, port, web_id, true).await;
    let path = format!("/{web_id}/ws");
    let accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
    for (offer, ext) in [
        (None, None),
        (Some("permessage-deflate"), Some("permessage-deflate")),
        (
            Some("permessage-deflate; client_max_window_bits"),
            Some("permessage-deflate"),
        ),
        (
            Some("permessage-deflate; server_max_window_bits=10"),
            Some("permessage-deflate; server_max_window_bits=10"),
        ),
        (
            Some("permessage-deflate; server_no_context_takeover; client_no_context_takeover"),
            Some("permessage-deflate; server_no_context_takeover; client_no_context_takeover"),
        ),
        (Some("x-webkit-deflate-frame"), None),
        (Some("permessage-deflate; bogus"), None),
    ] {
        let (_sock, head, _) = upgrade(port, &path, offer).await;
        assert!(
            head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "{offer:?}: {head}"
        );
        let mut want = vec![
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-accept", accept),
        ];
        if let Some(e) = ext {
            want.push(("sec-websocket-extensions", e));
        }
        want.extend([("date", "<DATE>"), ("server", "uvicorn")]);
        assert_eq!(header_block(&head), hb(&want), "{offer:?}");
    }
    // what python refuses: its status, its headers (no `server`), its body
    for (offer, status, body) in [
        (
            ";;",
            "HTTP/1.1 400 Bad Request",
            "Failed to open a WebSocket connection: invalid Sec-WebSocket-Extensions header: \
             expected token at 0 in ;;.\n",
        ),
        (
            "permessage-deflate; server_max_window_bits=8",
            "HTTP/1.1 500 Internal Server Error",
            "Failed to open a WebSocket connection.\nSee server log for more information.\n",
        ),
    ] {
        let (_sock, head, got) = upgrade(port, &path, Some(offer)).await;
        assert!(
            head.starts_with(&format!("{status}\r\n")),
            "{offer}: {head}"
        );
        let len = body.len().to_string();
        assert_eq!(
            header_block(&head),
            hb(&[
                ("date", "<DATE>"),
                ("content-length", &len),
                ("content-type", "text/plain"),
                ("connection", "close"),
            ]),
            "{offer}"
        );
        assert_eq!(String::from_utf8(got).unwrap(), body);
    }
    stop(&kernel, web_id).await;
}

/// After `permessage-deflate` is negotiated the leg speaks it: a deflated call
/// is understood, and the reply comes back deflated (RSV1, python's sync-flush
/// framing).
#[tokio::test]
async fn a_deflated_call_is_answered_deflated() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_pmd_io";
    let kernel = stage_web_with_ws_leg(&tmp, port, web_id, true).await;
    let (mut sock, head, _) = upgrade(
        port,
        &format!("/{web_id}/ws"),
        Some("permessage-deflate; client_max_window_bits"),
    )
    .await;
    assert!(
        head.contains("sec-websocket-extensions: permessage-deflate\r\n"),
        "{head}"
    );
    // the client's view: it decodes the server (15) and encodes its own (15)
    let mut pmd = PerMessageDeflate::new(Params {
        remote_no_context_takeover: false,
        local_no_context_takeover: false,
        remote_max_window_bits: 15,
        local_max_window_bits: 15,
    })
    .unwrap();
    let call = br#"{"type": "call", "id": "r1", "target": "kernel_state", "payload": {"type": "reflect", "tree": "none"}}"#;
    let (wire, rsv1) = pmd.encode(0x1, true, call).unwrap().unwrap();
    assert!(rsv1);
    // a masked client frame: FIN + RSV1 + text
    let mask = [9u8, 8, 7, 6];
    assert!(wire.len() < 126, "one short frame");
    let mut frame = vec![0xc1, 0x80 | wire.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(wire.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    sock.write_all(&frame).await.unwrap();
    // read server frames until the reply to `r1`
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(tokio::time::Instant::now() < deadline, "no reply to r1");
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
        assert_eq!(h[1] & 0x80, 0, "a server frame is never masked");
        let mut payload = vec![0u8; len];
        sock.read_exact(&mut payload).await.unwrap();
        if h[0] & 0x0f != 0x1 {
            continue;
        }
        assert_eq!(
            h[0] & 0x40,
            0x40,
            "every data frame the leg sends is deflated"
        );
        let text = pmd
            .decode(0x1, h[0] & 0x80 != 0, true, &payload)
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_slice(&text).unwrap();
        if v["type"] == "reply" && v["id"] == "r1" {
            // the deflated call was read and answered (here: the gate's
            // teaching denial, a reply like any other)
            assert!(v["data"].is_object(), "{v}");
            break;
        }
    }
    stop(&kernel, web_id).await;
}
