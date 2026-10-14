//! web_rest judges a request BEFORE it reads the body, and bounds the body — the
//! twin of python `web_rest/tests/test_web_rest_ingress_order.py`.
//!
//! The rule judges the request on what it says up front (its target and its
//! `X-Fantastic-Auth` credential), and only an admitted request's body is read —
//! at most `MAX_BODY` bytes (16 MiB, io_bridge's `MAX_FRAME`: the one frame cap
//! every wire leg holds); past it the request is refused, loud, 413. Buffering
//! and parsing the body first would let a sealed or password-gated leg make the
//! kernel hold any amount of data from a client it is about to refuse.
//!
//! A non-ASCII credential is a plain 403 at this leg.
//!
//! Over a RAW socket: what is asserted is that the answer comes while the client
//! still holds the rest of its body back — a leg that read first would wait for it.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SEALED_ERROR: &str = "edge sealed by policy";
const TOKEN_ENV: &str = "FANTASTIC_GROUP_TOKEN_WEB_REST_ORDER";

/// A web host on `port` with ONE rest leg `leg` carrying `ingress` (`None` =
/// sealed), granted the root.
async fn host(
    tmp: &TempDir,
    port: u16,
    web_id: &str,
    leg: &str,
    ingress: Option<Value>,
) -> Arc<Kernel> {
    let kernel = mk_kernel(tmp, false, true);
    let mut rest = json!({"type": "create_agent", "parent_id": web_id,
                          "handler_module": "web_rest.tools", "callable_by": {"$parent": ["get_routes"]}, "id": leg,
                          "can_call": {"kernel_state": "*"}});
    if let Some(rule) = ingress {
        rest["ingress_rule"] = rule;
    }
    for item in [
        json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": web_id, "port": port}),
        rest,
        json!({"type": "update_agent", "id": "kernel_state", "callable_by": {
               "$self": "*", "$parent": "*", "$subtree": "*", leg: "*"}}),
    ] {
        let r = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), item)])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "stage: {r}");
    }
    let boot = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(boot["running"], true, "boot: {boot}");
    wait_until_port_accepts(port).await;
    kernel
}

/// The request head: `POST <path>` with `headers` (raw bytes — a header value may
/// be latin-1). `host` names the server's own authority.
fn head(port: u16, path: &str, headers: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = format!("POST {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n").into_bytes();
    for (k, v) in headers {
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// Read ONE response off `sock`: `(status, json body)`. `None` when no complete
/// answer came within `secs` — a leg still waiting for the body.
async fn answer(sock: &mut tokio::net::TcpStream, secs: f64) -> Option<(u16, Value)> {
    let read = async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let text = String::from_utf8_lossy(&buf[..end]).to_string();
                let status: u16 = text.split(' ').nth(1)?.parse().ok()?;
                let len: usize = text
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    let body = &buf[end + 4..end + 4 + len];
                    let v = if body.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(body).ok()?
                    };
                    return Some((status, v));
                }
            }
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    };
    tokio::time::timeout(Duration::from_secs_f64(secs), read)
        .await
        .ok()
        .flatten()
}

/// POST `path` declaring a `declared`-byte JSON body but sending only `part` of
/// it: the answer that comes back while the rest is held.
async fn post_holding_back(
    port: u16,
    path: &str,
    headers: &[(&str, &[u8])],
    declared: usize,
    part: &[u8],
) -> Option<(u16, Value)> {
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let len = declared.to_string();
    let mut all: Vec<(&str, &[u8])> = vec![
        ("content-type", b"application/json"),
        ("content-length", len.as_bytes()),
    ];
    all.extend_from_slice(headers);
    sock.write_all(&head(port, path, &all)).await.unwrap();
    sock.write_all(part).await.unwrap();
    answer(&mut sock, 3.0).await
}

/// A whole JSON POST: `(status, body)`.
async fn post_whole(port: u16, path: &str, headers: &[(&str, &[u8])], body: &[u8]) -> (u16, Value) {
    post_holding_back(port, path, headers, body.len(), body)
        .await
        .expect("an answer to a whole request")
}

fn too_large(n: usize) -> Value {
    json!({
        "error": format!(
            "web_rest: the request body exceeds {n} bytes — the one frame cap every wire \
             leg holds (io_bridge MAX_FRAME); send larger data in chunks (write_stream)"
        ),
        "reason": "transport_error",
        "limit": n,
    })
}

#[tokio::test]
async fn a_sealed_leg_refuses_before_reading_the_body() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = host(&tmp, port, "web_order_sealed", "rs", None).await;
    let got = post_holding_back(
        port,
        "/rs/kernel_state",
        &[],
        1_000_000,
        &b"{not json at all ".repeat(64),
    )
    .await;
    let (status, body) = got.expect("the sealed leg answered without the body");
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"], SEALED_ERROR, "{body}");
    stop(&kernel, "web_order_sealed").await;
}

#[tokio::test]
async fn a_wrong_credential_is_refused_before_reading_the_body() {
    std::env::set_var(TOKEN_ENV, "grp");
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = host(
        &tmp,
        port,
        "web_order_pw",
        "pw",
        Some(json!({"type": "password_ingress", "env": TOKEN_ENV})),
    )
    .await;
    let reflect = br#"{"type": "reflect"}"#;
    // `été` as latin-1 bytes, and `päss` as utf-8
    for token in [&b"nope"[..], &[0xe9, b't', 0xe9][..], "päss".as_bytes()] {
        let got = post_holding_back(
            port,
            "/pw/kernel_state",
            &[("x-fantastic-auth", token)],
            reflect.len() + 1_000_000,
            reflect,
        )
        .await;
        let (status, out) = got.expect("refused without the body");
        assert_eq!(status, 403, "{token:?} {out}");
        assert_eq!(out["reason"], "unauthorized", "{token:?} {out}");
    }
    let (status, out) = post_whole(
        port,
        "/pw/kernel_state",
        &[("x-fantastic-auth", b"grp")],
        reflect,
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["id"], "kernel_state", "{out}");
    stop(&kernel, "web_order_pw").await;
}

#[test]
fn the_body_bound_is_the_one_frame_cap() {
    assert_eq!(
        crate::handlers::MAX_BODY,
        fantastic_io_bridge::codec::MAX_FRAME
    );
    assert_eq!(fantastic_io_bridge::codec::MAX_FRAME, 16 * 1024 * 1024);
}

#[tokio::test]
async fn a_declared_oversize_body_is_refused_unread() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = host(
        &tmp,
        port,
        "web_order_big",
        "ro",
        Some(json!("allow_all_ingress")),
    )
    .await;
    let max = crate::handlers::MAX_BODY;
    let got = post_holding_back(port, "/ro/kernel_state", &[], max + 1, b"").await;
    assert_eq!(got, Some((413, too_large(max))));
    stop(&kernel, "web_order_big").await;
}

/// No Content-Length (a chunked body): read until the bound is crossed, then
/// refused — the rest is never asked for (the terminating chunk is never sent).
#[tokio::test]
async fn an_undeclared_oversize_body_stops_at_the_bound() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = host(
        &tmp,
        port,
        "web_order_chunked",
        "ro",
        Some(json!("allow_all_ingress")),
    )
    .await;
    let max = crate::handlers::MAX_BODY;
    let sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (rd, mut wr) = sock.into_split();
    wr.write_all(&head(
        port,
        "/ro/kernel_state",
        &[
            ("content-type", b"application/json"),
            ("transfer-encoding", b"chunked"),
        ],
    ))
    .await
    .unwrap();
    // 1 MiB chunks until just past the bound — never the terminating `0` chunk.
    let chunk = vec![b'y'; 1 << 20];
    let writer = tokio::spawn(async move {
        for _ in 0..=(max >> 20) {
            let mut c = format!("{:x}\r\n", chunk.len()).into_bytes();
            c.extend_from_slice(&chunk);
            c.extend_from_slice(b"\r\n");
            if wr.write_all(&c).await.is_err() {
                return wr; // the leg answered and closed: nothing more is read
            }
        }
        wr
    });
    let mut joined = rd.reunite(writer.await.unwrap()).unwrap();
    let got = answer(&mut joined, 5.0).await;
    assert_eq!(got, Some((413, too_large(max))));
    let ok = br#"{"type": "reflect", "tree": "none"}"#;
    let (status, out) = post_whole(port, "/ro/kernel_state", &[], ok).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["id"], "kernel_state", "{out}");
    stop(&kernel, "web_order_chunked").await;
}

/// The bound, on the body stream itself: chunks are pulled only until it is
/// crossed (python reads `request.stream()` and stops there).
#[tokio::test]
async fn the_capped_read_stops_at_the_bound() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let pulled = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&pulled);
    let chunks = futures_util::stream::iter((0..10).map(move |_| {
        seen.fetch_add(40, Ordering::SeqCst);
        Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b'y'; 40]))
    }));
    let got = crate::handlers::read_capped(axum::body::Body::from_stream(chunks), None, 64).await;
    assert!(got.is_none(), "over the bound");
    assert_eq!(
        pulled.load(Ordering::SeqCst),
        80,
        "the bound was crossed at 80"
    );
    let whole = futures_util::stream::iter(
        (0..2).map(|_| Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b'z'; 30]))),
    );
    let got = crate::handlers::read_capped(axum::body::Body::from_stream(whole), None, 64).await;
    assert_eq!(got.as_deref(), Some(&[b'z'; 60][..]));
    let declared =
        crate::handlers::read_capped(axum::body::Body::from(vec![b'x'; 10]), Some(65), 64).await;
    assert!(
        declared.is_none(),
        "a declared length over the bound is refused unread"
    );
}
