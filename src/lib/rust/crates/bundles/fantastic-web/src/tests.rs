//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use serde_json::Map;
use std::time::Duration;
use tempfile::TempDir;

/// Acquire a free local port — bind to 127.0.0.1:0 and read back the
/// assigned port. We close the listener before the test boots web; a
/// rebind race is theoretically possible but vanishingly rare in
/// practice and the kernel-binary CI runs serial.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 0");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Build a kernel with web (+ optionally child surface bundles)
/// registered, with a root agent set as `core`.
fn mk_kernel(tmp: &TempDir, register_ws: bool, register_rest: bool) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    if register_ws {
        kernel
            .bundles
            .register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    }
    if register_rest {
        kernel
            .bundles
            .register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    }
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
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

/// Wait briefly for the bound listener to start accepting. Reduces
/// flake on slow CI machines without padding fast runs.
async fn wait_until_port_accepts(port: u16) {
    use tokio::net::TcpStream;
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `web` alone (no web_ws child) must NOT expose `/<id>/ws`. The
/// dynamic-mount step is a no-op when there are no children to query,
/// so the surface-less router 404s on the WS path.
#[tokio::test]
async fn web_alone_has_no_ws_endpoint() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, false);
    let port = free_port();
    let web_id = "web_alone";

    // create + boot web
    let create_reply = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE,
                "id": web_id,
                "port": port,
            }),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        create_reply.get("error").is_none(),
        "create: {create_reply}"
    );
    let boot_reply = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(boot_reply["running"], true, "boot: {boot_reply}");
    wait_until_port_accepts(port).await;

    // HTTP GET on /<id>/ws should NOT 101-upgrade; should 404.
    let url = format!("http://127.0.0.1:{port}/{web_id}/ws");
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .expect("http get");
    assert_eq!(
        resp.status().as_u16(),
        404,
        "expected 404 on bare web's /ws"
    );

    // cleanup
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

/// `web` + `web_ws` child: WS endpoint live at `/<web_id>/ws` after boot.
#[tokio::test]
async fn web_with_ws_child_serves_ws() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as TMessage;

    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, true, false);
    let port = free_port();
    let web_id = "web_with_ws";

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE,
                "id": web_id,
                "port": port,
            }),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": web_id,
                "handler_module": "web_ws.tools",
                "id": "wws_child",
            }),
        )])
        .await
        .remove(0)
        .result;
    let boot_reply = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(boot_reply["running"], true, "boot: {boot_reply}");
    wait_until_port_accepts(port).await;

    // Open a WS to /<web_id>/ws and round-trip a `call` frame.
    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    let frame =
        json!({"type": "call", "target": "kernel", "payload": {"type": "list_agents"}, "id": "x1"});
    ws.send(TMessage::Text(frame.to_string()))
        .await
        .expect("ws send");
    let reply = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("reply within 2s")
        .expect("ws stream open")
        .expect("ws msg ok");
    let text = match reply {
        TMessage::Text(t) => t,
        other => panic!("expected text reply, got {other:?}"),
    };
    let v: Value = serde_json::from_str(&text).expect("reply json");
    assert_eq!(v["type"], "reply");
    assert_eq!(v["id"], "x1");
    assert!(v["data"]["agents"].is_array());

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

/// `web` + `web_rest` child: POST endpoint live at `/<rest_id>/<target>`.
#[tokio::test]
async fn web_with_rest_child_serves_rest() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, true);
    let port = free_port();
    let web_id = "web_with_rest";
    let rest_id = "wr_child";

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE,
                "id": web_id,
                "port": port,
            }),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent",
                "parent_id": web_id,
                "handler_module": "web_rest.tools",
                "id": rest_id,
                // Deny-all: an inbound relay dispatches browser calls into the
                // kernel AS ITSELF — ingress_rule says WHO may call in, can_call
                // says WHERE into the kernel it lands. This test POSTs to `core`.
                // The leg SEALS without an explicit rule (403 before dispatch),
                // so open it here as a browser-facing dev leg would be.
                "ingress_rule": "allow_all_ingress",
                "can_call": { "core": "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
    // Two-sided deny-all: the relay dispatches into root AS ITSELF, but it is
    // root's GRANDCHILD (child of `web`), so $children:* does not cover it — open
    // root's ingress for the relay (merged with the structural baseline).
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "update_agent", "id": "core", "callable_by": {
                "$self": "*", "$parent": "*", "$children": "*", rest_id: "*",
            }}),
        )])
        .await
        .remove(0)
        .result;
    let boot_reply = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(boot_reply["running"], true, "boot: {boot_reply}");
    wait_until_port_accepts(port).await;

    let url = format!("http://127.0.0.1:{port}/{rest_id}/core");
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Content-Type", "application/json")
        .body(r#"{"type": "list_agents"}"#)
        .send()
        .await
        .expect("http post");
    assert!(resp.status().is_success(), "status: {}", resp.status());
    let body: Value = resp.json().await.expect("body json");
    assert!(body["agents"].is_array(), "body: {body}");

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

// ── Binary frame chunking ───────────────────────────────────────────
//
// These tests drive the chunked-upload protocol over a real WS
// connection. Wire shape per chunk:
//
//     [4B BE hdr_len][header JSON {target, type, upload_id, chunk_index,
//                                  total_chunks, final, id?}][raw blob]
//
// Single-frame uploads (no `upload_id` in the header) take the existing
// fast path and stay byte-compatible with Python's wire.

/// Stage a web + web_ws on `port`. Returns the booted kernel + the
/// web agent id. Caller is responsible for stop.
async fn stage_web_with_ws(tmp: &TempDir, port: u16, web_id: &str) -> Arc<Kernel> {
    let kernel = mk_kernel(tmp, true, false);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE,
                "id": web_id,
                "port": port,
            }),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent",
                "parent_id": web_id,
                "handler_module": "web_ws.tools",
                "id": format!("{web_id}_ws"),
            }),
        )])
        .await
        .remove(0)
        .result;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["running"], true, "boot: {r}");
    wait_until_port_accepts(port).await;
    kernel
}

/// Build a binary WS frame: 4B big-endian header_len + JSON header + blob.
fn build_binary_frame(header: &Value, blob: &[u8]) -> Vec<u8> {
    let hdr_bytes = serde_json::to_vec(header).expect("serialize header");
    let mut out = Vec::with_capacity(4 + hdr_bytes.len() + blob.len());
    out.extend_from_slice(&(hdr_bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&hdr_bytes);
    out.extend_from_slice(blob);
    out
}

/// Send a chunk + collect every text frame the server emits during a
/// short window. Returns parsed JSON of each frame.
async fn send_binary_and_drain(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    frame: Vec<u8>,
) -> Vec<Value> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as TMessage;
    ws.send(TMessage::Binary(frame)).await.expect("send binary");
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
            Ok(Some(Ok(TMessage::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    out.push(v);
                }
            }
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => break,
        }
    }
    out
}

#[tokio::test]
async fn single_frame_upload_dispatches_immediately() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_single_bin";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;

    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    // No upload_id → single-frame fast path. Target a non-existent agent;
    // assertion is on the wire shape, not on dispatch success.
    let header = json!({"target": "nonexistent_xyz", "type": "noop", "id": "s1"});
    let frame = build_binary_frame(&header, &[1, 2, 3, 4]);
    let frames = send_binary_and_drain(&mut ws, frame).await;
    let reply = frames
        .iter()
        .find(|f| f["id"] == "s1")
        .expect("expected a reply frame for id=s1");
    // Wire shape: WS replies are always `type:"reply"` with verb-level
    // errors inside `data.error` (Python parity — see web/lib.rs).
    // `type:"error"` is reserved for out-of-band frame-decode failures.
    assert_eq!(reply["type"], "reply", "single-frame got: {reply}");
    let err = reply["data"]["error"].as_str().unwrap_or("");
    assert!(
        err.contains("no agent"),
        "expected `no agent` in data.error, got: {reply}"
    );

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn chunked_upload_reassembles_in_order() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_chunked_inorder";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;

    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    let upload_id = "u_ord1";
    // 3 chunks, 4 bytes each. Final dispatch is to a non-existent agent
    // → reply carries the kernel's `no agent` error, but the chunking
    // path is what we're asserting reached dispatch.
    for (idx, blob) in [
        (0u32, &[1u8, 1, 1, 1] as &[u8]),
        (1, &[2, 2, 2, 2]),
        (2, &[3, 3, 3, 3]),
    ] {
        let is_final = idx == 2;
        let header = json!({
            "target": "nonexistent",
            "type": "noop",
            "id": "c1",
            "upload_id": upload_id,
            "chunk_index": idx,
            "total_chunks": 3,
            "final": is_final,
        });
        let frame = build_binary_frame(&header, blob);
        let frames = send_binary_and_drain(&mut ws, frame).await;
        if is_final {
            let reply = frames
                .iter()
                .find(|f| f["id"] == "c1")
                .expect("final chunk should yield a reply");
            // Python wire shape: reply frame, verb-level error in data.
            assert_eq!(reply["type"], "reply");
            let err = reply["data"]["error"].as_str().unwrap_or("");
            assert!(
                err.contains("no agent"),
                "expected `no agent`, got: {reply}"
            );
        } else {
            let ack = frames
                .iter()
                .find(|f| f["type"] == "chunk_ack")
                .expect("non-final chunk must produce a chunk_ack");
            assert_eq!(ack["upload_id"], upload_id);
            assert_eq!(ack["chunk_index"], idx);
        }
    }

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn chunked_upload_handles_out_of_order_chunks() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_chunked_ooo";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;

    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    let upload_id = "u_ooo1";
    // Out-of-order with the `final`-flagged chunk arriving FIRST.
    // Regression (fixed): the server must HOLD the early `final` and
    // assemble once all chunks are present — it must NOT error+drop.
    // Assembly triggers on count == total_chunks, so order is irrelevant.
    let chunks = [
        (2u32, &[3u8; 4] as &[u8], true), // final flag, arrives first
        (0, &[1u8; 4], false),
        (1, &[2u8; 4], false),
    ];
    let mut saw_error = false;
    let mut reply: Option<Value> = None;
    for (idx, blob, is_final) in chunks {
        let header = json!({
            "target": "nonexistent",
            "type": "noop",
            "id": "ooo1",
            "upload_id": upload_id,
            "chunk_index": idx,
            "total_chunks": 3,
            "final": is_final,
        });
        let frame = build_binary_frame(&header, blob);
        for f in send_binary_and_drain(&mut ws, frame).await {
            if f["id"] == "ooo1" {
                match f["type"].as_str() {
                    Some("error") => saw_error = true,
                    Some("reply") => reply = Some(f.clone()),
                    _ => {}
                }
            }
        }
    }
    assert!(
        !saw_error,
        "early `final` must be held, not error+drop (out-of-order regression)"
    );
    let reply = reply.expect("dispatch reply once all 3 chunks assembled");
    // Reassembled (12-byte) blob dispatched to a nonexistent agent →
    // reply.data carries the kernel's `no agent` error.
    assert!(
        reply["data"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("no agent"),
        "expected 'no agent' in dispatch reply, got {reply}"
    );

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn oversized_chunk_rejected() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_oversize";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;

    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    // Past MAX_CHUNK_SIZE, which is 4 KB under test (see the consts in `lib.rs`).
    let big_blob = vec![0u8; 2 * crate::ws::MAX_CHUNK_SIZE];
    let header = json!({"target": "nonexistent", "type": "noop", "id": "big1"});
    let frame = build_binary_frame(&header, &big_blob);
    let frames = send_binary_and_drain(&mut ws, frame).await;
    let err = frames
        .iter()
        .find(|f| f["id"] == "big1")
        .expect("expected error reply for oversized chunk");
    assert_eq!(err["type"], "error");
    assert!(err["error"]
        .as_str()
        .unwrap_or("")
        .contains("exceeds chunk cap"));

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn oversized_total_rejected() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_oversize_total";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;

    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    // Chunks AT the per-chunk cap, enough of them that the cumulative sum trips
    // the total cap one chunk before the last. Both caps are tiny under test, so
    // this is a few KB rather than the 101 MB it used to push across the socket.
    let upload_id = "u_huge";
    let chunk_size = crate::ws::MAX_CHUNK_SIZE;
    let total_chunks: u32 = (crate::ws::MAX_UPLOAD_SIZE / chunk_size) as u32 + 1;
    let one_mb = vec![0u8; chunk_size];

    let mut saw_total_error = false;
    for idx in 0..total_chunks {
        let header = json!({
            "target": "nonexistent",
            "type": "noop",
            "id": "huge1",
            "upload_id": upload_id,
            "chunk_index": idx,
            "total_chunks": total_chunks,
            "final": false,
        });
        let frame = build_binary_frame(&header, &one_mb);
        let frames = send_binary_and_drain(&mut ws, frame).await;
        if frames.iter().any(|f| {
            f["type"] == "error" && f["error"].as_str().unwrap_or("").contains("total cap")
        }) {
            saw_total_error = true;
            break;
        }
    }
    assert!(
        saw_total_error,
        "expected total-size cap to fire before chunk 101"
    );

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn chunk_ack_emitted_after_each_non_final_chunk() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_chunk_ack";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;

    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    let upload_id = "u_ack";
    // Send 2 non-final chunks — each must produce a chunk_ack.
    for idx in [0u32, 1] {
        let header = json!({
            "target": "nonexistent",
            "type": "noop",
            "id": "ack1",
            "upload_id": upload_id,
            "chunk_index": idx,
            "total_chunks": 3,
            "final": false,
        });
        let frame = build_binary_frame(&header, &[idx as u8; 4]);
        let frames = send_binary_and_drain(&mut ws, frame).await;
        let ack = frames
            .iter()
            .find(|f| f["type"] == "chunk_ack")
            .unwrap_or_else(|| panic!("no chunk_ack after chunk {idx}: {frames:?}"));
        assert_eq!(ack["upload_id"], upload_id);
        assert_eq!(ack["chunk_index"], idx);
    }

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

#[tokio::test]
async fn pending_uploads_drop_on_ws_disconnect() {
    // Per-WS state means a dropped connection cleans up automatically —
    // no GC task needed. This test verifies that semantics by opening
    // a WS, sending a partial chunk, dropping the connection, then
    // opening a fresh WS and reusing the same upload_id from scratch
    // (which should work because the prior buffer is gone).
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_disconn";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;

    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");

    {
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("ws1");
        let header = json!({
            "target": "nonexistent",
            "type": "noop",
            "upload_id": "u_disco",
            "chunk_index": 0,
            "total_chunks": 2,
            "final": false,
        });
        let frame = build_binary_frame(&header, &[42u8; 4]);
        let frames = send_binary_and_drain(&mut ws, frame).await;
        assert!(frames.iter().any(|f| f["type"] == "chunk_ack"));
        // Drop the WS without sending the second chunk.
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Fresh WS — same upload_id should be a clean slate (per-WS state
    // means the prior WS's pending map dropped). Sending chunk 0 again
    // should succeed without a "total_chunks mismatch" or similar.
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("ws2");
    let header = json!({
        "target": "nonexistent",
        "type": "noop",
        "upload_id": "u_disco",
        "chunk_index": 0,
        "total_chunks": 2,
        "final": false,
    });
    let frame = build_binary_frame(&header, &[42u8; 4]);
    let frames = send_binary_and_drain(&mut ws, frame).await;
    assert!(
        frames.iter().any(|f| f["type"] == "chunk_ack"),
        "fresh WS should treat upload_id as new — got {frames:?}",
    );

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

// ── /_assets/* — vendored third-party web deps ─────────────────────
//
// Three.js + xterm + addon-fit are bundled and served at top-level
// URLs so canvas/terminal WebView surfaces don't need CDN access.
// These tests boot the HTTP layer + hit each asset URL with reqwest
// and verify content-type + non-empty body + immutable cache header.

// ── ingest_chunk unit tests (pure assembly, no WS) ──────────────────

#[test]
fn ingest_chunk_assembles_out_of_order() {
    // Regression: a `final`/last-index chunk arriving BEFORE its peers
    // must be held + assembled once all are present — not dropped with
    // "final received but N missing". Chunks concatenate in index order
    // (BTreeMap) regardless of arrival order.
    let mut map = std::collections::HashMap::new();
    let hdr = json!({"target": "x", "type": "upload"});
    // Arrive last-index first, then 0, then 1.
    assert!(
        crate::ws::ingest_chunk(&mut map, "u", 2, 3, b"C".to_vec(), &hdr)
            .expect("early last chunk acks, not errors")
            .is_none()
    );
    assert!(
        crate::ws::ingest_chunk(&mut map, "u", 0, 3, b"A".to_vec(), &hdr)
            .unwrap()
            .is_none()
    );
    let (header, blob) = crate::ws::ingest_chunk(&mut map, "u", 1, 3, b"B".to_vec(), &hdr)
        .unwrap()
        .expect("assembled once all 3 present");
    assert_eq!(blob, b"ABC", "index-ordered concat regardless of arrival");
    assert_eq!(header, json!({"target": "x", "type": "upload"}));
    assert!(map.is_empty(), "entry removed on completion");
}

#[test]
fn ingest_chunk_total_mismatch_errors_and_drops() {
    let mut map = std::collections::HashMap::new();
    let hdr = json!({"type": "upload"});
    assert!(
        crate::ws::ingest_chunk(&mut map, "u", 0, 2, b"A".to_vec(), &hdr)
            .unwrap()
            .is_none()
    );
    let err = crate::ws::ingest_chunk(&mut map, "u", 1, 3, b"B".to_vec(), &hdr)
        .expect_err("total_chunks mismatch must error");
    assert!(err.contains("total_chunks mismatch"), "{err}");
    assert!(map.is_empty(), "mismatched entry dropped");
}

#[test]
fn ingest_chunk_single_chunk_completes_immediately() {
    let mut map = std::collections::HashMap::new();
    let hdr = json!({"type": "upload"});
    let (_h, blob) = crate::ws::ingest_chunk(&mut map, "u", 0, 1, b"solo".to_vec(), &hdr)
        .unwrap()
        .expect("a 1-chunk upload assembles on the first frame");
    assert_eq!(blob, b"solo");
}

/// The BINARY leg of the REST surface (octet-stream body) — mirror of the
/// Python `web_rest` binary leg. An octet POST of a `text_file.patch` carries
/// the patch in the raw body and the envelope on `X-Fantastic-Payload`; the
/// reply is JSON `{patched:true}`. A `read_stream` returns an OCTET reply whose
/// body is the raw file bytes and whose `X-Fantastic-Payload` header carries the
/// rest of the envelope. Never base64.
#[tokio::test]
async fn web_rest_octet_leg_patch_and_read_stream() {
    // Kernel with web + web_rest + file_bridge registered.
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    kernel
        .bundles
        .register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    kernel.bundles.register(
        fantastic_text_file::HANDLER_MODULE,
        fantastic_text_file::TextFileBundle,
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    // The file_bridge clamps its root under the kernel's running dir, which the
    // bundle derives from the root agent's `.fantastic` parent = `tmp`. Seed a
    // text file at `tmp/served/greeting.txt`.
    let served = tmp.path().join("served");
    std::fs::create_dir_all(&served).unwrap();
    std::fs::write(served.join("greeting.txt"), "hello world\n").unwrap();

    let port = free_port();
    let web_id = "web_octet";
    let rest_id = "wr_octet";
    let fb_id = "fb_octet";

    // web host.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE, "id": web_id, "port": port,
            }),
        )])
        .await
        .remove(0)
        .result;
    // file_bridge — OPEN (its own io-bridge gate) + accept the REST surface as a
    // caller (two-sided call gate: ingress side).
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent", "parent_id": "core",
                "handler_module": "file_bridge.tools", "id": fb_id,
                "root": "served",
                "ingress_rule": "allow_all_ingress",
                "callable_by": { "$self": "*", "$parent": "*", "$children": "*", rest_id: "*", "ed_octet": "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
    // The EDITOR: patch moved off the bridge — text_file owns SEARCH/REPLACE
    // and reaches disk only through gated sends to the file leg.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({
                "type": "create_agent", "parent_id": "core",
                "handler_module": "text_file.tools", "id": "ed_octet",
                "target_id": fb_id,
                "can_call": { fb_id: "*" },
                "callable_by": { "$self": "*", "$parent": "*", "$children": "*", rest_id: "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
    // web_rest child — may reach the file_bridge AND the editor (egress side).
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": web_id,
                "handler_module": "web_rest.tools", "id": rest_id,
                "ingress_rule": "allow_all_ingress",
                "can_call": { fb_id: "*", "ed_octet": "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
    let boot_reply = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(boot_reply["running"], true, "boot: {boot_reply}");
    wait_until_port_accepts(port).await;

    let url = format!("http://127.0.0.1:{port}/{rest_id}/{fb_id}");
    let ed_url = format!("http://127.0.0.1:{port}/{rest_id}/ed_octet");
    let client = reqwest::Client::new();

    // ── text_file.patch: patch rides the raw octet body, to the EDITOR ───
    let patch = "<<<<<<< SEARCH\nhello world\n=======\ngoodbye world\n>>>>>>> REPLACE\n";
    let resp = client
        .post(&ed_url)
        .header("Content-Type", "application/octet-stream")
        .header(
            "X-Fantastic-Payload",
            r#"{"type":"patch","edits":[{"path":"greeting.txt","hunks":1}]}"#,
        )
        .body(patch)
        .send()
        .await
        .expect("octet patch post");
    assert!(
        resp.status().is_success(),
        "patch status: {}",
        resp.status()
    );
    // A verb with no bytes in its reply comes back as JSON.
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.starts_with("application/json"), "patch reply ct: {ct}");
    let pbody: Value = resp.json().await.expect("patch reply json");
    assert_eq!(pbody["patched"], true, "patch reply: {pbody}");
    assert_eq!(pbody["edits"][0]["hunks"], 1, "patch reply: {pbody}");

    // ── read_stream: reply is an OCTET body = the raw (patched) file bytes ─
    let resp = client
        .post(&url)
        .header("Content-Type", "application/octet-stream")
        .header(
            "X-Fantastic-Payload",
            r#"{"type":"read_stream","path":"greeting.txt"}"#,
        )
        .body(Vec::<u8>::new())
        .send()
        .await
        .expect("octet read_stream post");
    assert!(resp.status().is_success(), "read status: {}", resp.status());
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("application/octet-stream"),
        "read reply ct: {ct}"
    );
    // Envelope rides the header; the ONE bytes field is named by the header.
    let env_raw = resp
        .headers()
        .get("x-fantastic-payload")
        .and_then(|v| v.to_str().ok())
        .expect("X-Fantastic-Payload on octet reply")
        .to_string();
    let field = resp
        .headers()
        .get("x-fantastic-bytes-field")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(field, "bytes", "bytes-field header");
    let env: Value = serde_json::from_str(&env_raw).expect("payload header is JSON");
    assert_eq!(env["path"], "greeting.txt", "reply envelope: {env}");
    // The body is the raw file bytes — reflects the applied patch, never base64.
    let raw = resp.bytes().await.expect("octet reply body");
    assert_eq!(&raw[..], b"goodbye world\n", "read_stream body");

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

/// A SEALED REST leg answers 403 with the repair — it does not serve, and it does
/// not blame the wrong side.
///
/// The rust POST/GET handlers dispatched straight into `with_sender(owner)` with
/// no ingress check, so a leg carrying NO `ingress_rule` — sealed by default
/// everywhere else in the io family — served unauthenticated requests. Where a
/// call did fail it failed on EGRESS and named a false cause ("wr_sealed may not
/// call \"reflect\" on …") while the true one, this leg is sealed, was never said.
#[tokio::test]
async fn sealed_rest_leg_answers_403_and_teaches_the_repair() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, true);
    let port = free_port();
    let web_id = "web_sealed_rest";
    let rest_id = "wr_sealed";

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": HANDLER_MODULE, "id": web_id, "port": port}),
        )])
        .await
        .remove(0)
        .result;
    // NO ingress_rule — the sealed-by-default leg. `can_call` is wide open, so a
    // 200 here could only mean the ingress gate never ran.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": web_id,
                   "handler_module": "web_rest.tools", "id": rest_id,
                   "can_call": {"core": "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "update_agent", "id": "core", "callable_by": {
                "$self": "*", "$parent": "*", "$children": "*", rest_id: "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let boot_reply = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(boot_reply["running"], true, "boot: {boot_reply}");
    wait_until_port_accepts(port).await;

    let client = reqwest::Client::new();
    // POST — the dispatch route.
    let resp = client
        .post(format!("http://127.0.0.1:{port}/{rest_id}/core"))
        .header("Content-Type", "application/json")
        .body(r#"{"type": "list_agents"}"#)
        .send()
        .await
        .expect("http post");
    assert_eq!(resp.status().as_u16(), 403, "a sealed leg must not serve");
    let body: Value = resp.json().await.expect("body json");
    assert_eq!(body["reason"], "unauthorized", "body: {body}");
    assert!(
        body["hint"]
            .as_str()
            .unwrap_or("")
            .contains("ingress_rule=allow_all_ingress"),
        "the 403 must carry the repair: {body}"
    );
    assert!(
        body["agents"].is_null(),
        "the tree leaked past the gate: {body}"
    );

    // GET /_reflect and GET /_reflect/<target> — the same seal.
    for path in [
        format!("http://127.0.0.1:{port}/{rest_id}/_reflect"),
        format!("http://127.0.0.1:{port}/{rest_id}/_reflect/core"),
    ] {
        let r = client.get(&path).send().await.expect("http get");
        assert_eq!(r.status().as_u16(), 403, "{path} served past the seal");
        let b: Value = r.json().await.expect("body json");
        assert_eq!(b["reason"], "unauthorized", "{path}: {b}");
    }

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

/// The other side of the same gate: an OPEN leg dispatches, reflect routes
/// included — so the seal above is the rule refusing, not the route breaking.
#[tokio::test]
async fn open_rest_leg_dispatches_including_the_reflect_routes() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, true);
    let port = free_port();
    let web_id = "web_open_rest";
    let rest_id = "wr_open";

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": HANDLER_MODULE, "id": web_id, "port": port}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": web_id,
                   "handler_module": "web_rest.tools", "id": rest_id,
                   "ingress_rule": "allow_all_ingress",
                   "can_call": {"core": "*", "kernel": "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "update_agent", "id": "core", "callable_by": {
                "$self": "*", "$parent": "*", "$children": "*", rest_id: "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let boot_reply = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(boot_reply["running"], true, "boot: {boot_reply}");
    wait_until_port_accepts(port).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{port}/{rest_id}/_reflect"))
        .send()
        .await
        .expect("http get");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "an open leg must serve reflect"
    );
    let body: Value = resp.json().await.expect("body json");
    assert_eq!(body["id"], "core", "reflect body: {body}");

    let resp = client
        .get(format!("http://127.0.0.1:{port}/{rest_id}/_reflect/core"))
        .send()
        .await
        .expect("http get");
    assert_eq!(resp.status().as_u16(), 200);

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

/// Finished call handles are pruned, so a long-lived socket cannot grow the
/// pending vector without bound (#135). The handles still running are kept —
/// they are exactly what `teardown` has to abort on disconnect.
#[tokio::test]
async fn finished_call_handles_are_pruned_before_the_next_push() {
    // THE TABLE IS THE LIVENESS TRUTH now: a frame handler is a registered task,
    // so pruning asks the kernel what is still running rather than polling a
    // JoinHandle. The bound is the same one this test always pinned — a
    // dashboard polling `status` once a second must not accumulate a handle per
    // call for the life of the socket.
    let kernel = std::sync::Arc::new(fantastic_kernel::Kernel::new());
    let agent = AgentId::from("web_track");
    let pending: std::sync::Mutex<Vec<fantastic_kernel::TaskId>> =
        std::sync::Mutex::new(Vec::new());

    for _ in 0..50 {
        let t = kernel.spawn_task(&agent, "ws_call", async {});
        // Let it finish, then push: the NEXT push must prune it.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        crate::ws::track_call(&kernel, &pending, t);
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let live = kernel.spawn_task(&agent, "ws_call", std::future::pending::<()>());
    crate::ws::track_call(&kernel, &pending, live);
    let held = pending.lock().unwrap().len();
    assert!(held <= 2, "pending grew unbounded: {held} ids held");

    // The one still running is KEPT — pruning must not drop what a disconnect
    // has to cancel.
    assert_eq!(held, 1, "the in-flight id was pruned away");
    assert!(kernel.tasks.contains(&live));
    kernel.cancel_task(&live).await;
}

// ═══ batch 6: python-parity twins ═══════════════════════════════════

/// Stage one `web` host with an OPEN `web_rest` child and an OPEN
/// `file_bridge` rooted at `<tmp>/served`, booted and accepting. The shape
/// every HTTP-envelope twin below needs.
async fn stage_host(
    tmp: &TempDir,
    port: u16,
    web_id: &str,
    rest_id: &str,
    fb_id: &str,
) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    kernel
        .bundles
        .register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    kernel
        .bundles
        .register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let served = tmp.path().join("served");
    std::fs::create_dir_all(&served).unwrap();
    std::fs::write(served.join("hello.txt"), "hello world\n").unwrap();
    std::fs::write(served.join("empty.txt"), "").unwrap();
    std::fs::write(served.join("doc.pdf"), b"%PDF-1.4\n").unwrap();
    std::fs::write(served.join("na\u{ef}ve.txt"), "naive!\n").unwrap();

    for item in [
        json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": HANDLER_MODULE, "id": web_id, "port": port}),
        json!({"type": "create_agent", "parent_id": "core",
               "handler_module": "file_bridge.tools", "id": fb_id, "root": "served",
               "ingress_rule": "allow_all_ingress",
               "callable_by": {"$self": "*", "$parent": "*", "$children": "*",
                               rest_id: "*", web_id: "*"}}),
        json!({"type": "create_agent", "parent_id": web_id,
               "handler_module": "web_rest.tools", "id": rest_id,
               "ingress_rule": "allow_all_ingress",
               "can_call": {"core": "*", "kernel": "*", fb_id: "*"}}),
        json!({"type": "update_agent", "id": "core", "callable_by": {
               "$self": "*", "$parent": "*", "$children": "*", rest_id: "*"}}),
        json!({"type": "update_agent", "id": web_id, "can_call": {
               "$self": "*", "$parent": "*", "$children": "*", fb_id: "*"}}),
    ] {
        let reply = kernel
            .send(vec![Item::new(AgentId::from("core"), item)])
            .await
            .remove(0)
            .result;
        assert!(reply.get("error").is_none(), "stage: {reply}");
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

/// A11.2 — the readme is python's file, byte for byte.
#[test]
fn readme_is_python_verbatim() {
    assert_eq!(
        README,
        include_str!("../../../../../python/bundled_agents/web/host/src/web/readme.md")
    );
}

/// A11.3 — the `/` scaffold is python's template, byte for byte (no `.visit`
/// rule, no legend, no `transport.js` tag).
#[test]
fn index_html_is_python_verbatim() {
    assert_eq!(
        ROOT_INDEX_HTML,
        include_str!("../../../../../python/bundled_agents/web/host/src/web/templates/index.html")
    );
}

/// A11.10 — the file proxy types through CPython's `mimetypes` table, with
/// starlette's `; charset=utf-8` on `text/*` and octet-stream for a miss.
#[test]
fn file_proxy_content_type_matches_python_mimetypes() {
    assert_eq!(mimetypes::content_type("doc.pdf"), "application/pdf");
    assert_eq!(
        mimetypes::content_type("a/b/note.txt"),
        "text/plain; charset=utf-8"
    );
    assert_eq!(
        mimetypes::content_type("page.HTML"),
        "text/html; charset=utf-8"
    );
    assert_eq!(
        mimetypes::content_type("style.css"),
        "text/css; charset=utf-8"
    );
    assert_eq!(mimetypes::content_type("data.json"), "application/json");
    assert_eq!(mimetypes::content_type("logo.png"), "image/png");
    assert_eq!(mimetypes::content_type("logo.svg"), "image/svg+xml");
    // an ENCODING suffix names the compression, not the type
    assert_eq!(mimetypes::content_type("dump.tar.gz"), "application/x-tar");
    // a compound suffix rewrites first
    assert_eq!(mimetypes::content_type("pack.tgz"), "application/x-tar");
    // unknown and extension-less both fall back
    assert_eq!(
        mimetypes::content_type("blob.zzz"),
        "application/octet-stream"
    );
    assert_eq!(
        mimetypes::content_type("README"),
        "application/octet-stream"
    );
}

/// A13.14 — every server→client text frame is python's
/// `json.dumps(envelope, default=str)`: `, `/`: ` separators and ensure_ascii,
/// so `·` and `↑` leave escaped, not as raw UTF-8.
#[test]
fn event_frame_bytes_match_python_json_dumps() {
    let frame =
        json!({"type": "event", "payload": {"text": "[reply to 128 · files reflect · ↑126]"}});
    assert_eq!(
        fantastic_io_bridge::codec::encode_frame(&frame),
        "{\"type\": \"event\", \"payload\": {\"text\": \"[reply to 128 \\u00b7 files reflect \\u00b7 \\u2191126]\"}}"
    );
}

/// A13.18 — uvicorn's keepalive contract, pinned at the source (a live twin
/// would have to idle 40 s). The loop pings every `PING_INTERVAL` and closes
/// `1011 keepalive ping timeout` when no pong lands inside `PING_TIMEOUT`.
#[test]
fn server_pings_and_closes_1011_without_pong() {
    assert_eq!(crate::ws::PING_INTERVAL, std::time::Duration::from_secs(20));
    assert_eq!(crate::ws::PING_TIMEOUT, std::time::Duration::from_secs(20));
    assert_eq!(crate::ws::CLOSE_INTERNAL_ERROR, 1011);
    assert_eq!(
        crate::ws::KEEPALIVE_TIMEOUT_REASON,
        "keepalive ping timeout"
    );
}

/// A11.3 (D11) — the route table is python's FOUR baked routes. The rust-only
/// extras are gone: no `/<id>/` server-side render, no `/transport.js`, no
/// `/_assets/*` vendored JS, no `/favicon.ico`.
#[tokio::test]
async fn render_route_is_absent() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_d11", "wr_d11", "fb_d11").await;
    let client = reqwest::Client::new();
    for gone in [
        "/web_d11/",
        "/transport.js",
        "/favicon.ico",
        "/_assets/three.module.js",
        "/_assets/xterm.min.js",
    ] {
        let resp = client
            .get(format!("http://127.0.0.1:{port}{gone}"))
            .send()
            .await
            .expect("get");
        assert_eq!(resp.status().as_u16(), 404, "{gone} must not be a route");
    }
    // The four that remain DO serve.
    for kept in ["/", "/favicon.png", "/_assets/favicon.png"] {
        let resp = client
            .get(format!("http://127.0.0.1:{port}{kept}"))
            .send()
            .await
            .expect("get");
        assert_eq!(resp.status().as_u16(), 200, "{kept} must serve");
    }
    // `/` is python's `_index_page` markup: id, code, hm — no `↗` visit link,
    // because there is no server-side render route to link to.
    let resp = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect("get /");
    assert_eq!(resp.headers()["content-type"], "text/html; charset=utf-8");
    let page = resp.text().await.unwrap();
    assert!(!page.contains("class=\"visit\""), "no ↗ link: {page}");
    assert!(!page.contains("transport.js"), "no transport tag");
    assert!(
        page.contains(
            r#"<li><span class="id">core</span> <code>core</code> <span class="hm">(root)</span>"#
        ),
        "python's node markup: {page}"
    );
    let resp = client
        .get(format!("http://127.0.0.1:{port}/fb_d11/file/hello.txt"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()["content-type"],
        "text/plain; charset=utf-8",
        "A11.10: python's mimetypes table types the proxy"
    );
    assert_eq!(resp.text().await.unwrap(), "hello world\n");
    stop(&kernel, "web_d11").await;
}

/// A13.6 + A13.9 — an unmatched path is FastAPI's `{"detail":"Not Found"}`, a
/// path that matches under another method is `405` with that route's `Allow`
/// and FastAPI's `{"detail":"Method Not Allowed"}`, and a trailing slash that
/// would match after toggling is a `307` carrying the `Host`.
#[tokio::test]
async fn unmatched_paths_and_methods_answer_fastapi_json() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_404", "rest", "fb_404").await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://127.0.0.1:{port}/nope"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let body = resp.text().await.unwrap();
    assert_eq!(body, r#"{"detail":"Not Found"}"#);
    assert_eq!(body.len(), 22, "python measured 22 bytes");

    // `/rest/files` is mounted POST-only: every other method is 405 allow: POST.
    for method in [
        reqwest::Method::GET,
        reqwest::Method::PUT,
        reqwest::Method::HEAD,
        reqwest::Method::OPTIONS,
    ] {
        let resp = client
            .request(method.clone(), format!("http://127.0.0.1:{port}/rest/core"))
            .send()
            .await
            .expect("req");
        assert_eq!(resp.status().as_u16(), 405, "{method} on a POST route");
        assert_eq!(resp.headers()["allow"], "POST");
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert_eq!(resp.headers()["content-length"], "31");
        if method != reqwest::Method::HEAD {
            assert_eq!(
                resp.text().await.unwrap(),
                r#"{"detail":"Method Not Allowed"}"#
            );
        }
    }
    // HEAD on a GET route is 405 too — FastAPI's APIRoute adds no implicit HEAD.
    let resp = client
        .request(
            reqwest::Method::HEAD,
            format!("http://127.0.0.1:{port}/rest/_reflect/core"),
        )
        .send()
        .await
        .expect("head");
    assert_eq!(resp.status().as_u16(), 405);
    assert_eq!(resp.headers()["allow"], "GET");

    // `/rest/` toggles to `/rest`, which matches nothing → 404 (measured).
    let resp = client
        .get(format!("http://127.0.0.1:{port}/rest/"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);
    assert_eq!(resp.text().await.unwrap(), r#"{"detail":"Not Found"}"#);
    stop(&kernel, "web_404").await;
}

/// A13.9 — starlette's ordered walk: `POST /rest/_reflect` hits the POST
/// route's `{target_id}` (the literal `_reflect` GET route is a different
/// method, so it cannot shadow it), and a trailing slash redirects `307`.
#[tokio::test]
async fn post_to_reflect_literal_dispatches_to_that_target() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_ord", "rest", "fb_ord").await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://127.0.0.1:{port}/rest/_reflect"))
        .header("content-type", "application/json")
        .body(r#"{"type":"reflect"}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "python dispatches to target `_reflect`"
    );
    let body: Value = resp.json().await.expect("json");
    assert_eq!(
        body["reason"], "unknown_agent",
        "the target named `_reflect` does not exist: {body}"
    );
    stop(&kernel, "web_ord").await;
}

/// A13.9 — `GET /rest/_reflect/core/` toggles to a matching route, so python
/// answers `307` with an absolute `location` built from the `Host` header and
/// the query string carried through. `content-length: 0`.
#[tokio::test]
async fn trailing_slash_answers_307_with_location() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_307", "rest", "fb_307").await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/rest/_reflect/core/?tree=none"
        ))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 307);
    assert_eq!(
        resp.headers()["location"],
        format!("http://127.0.0.1:{port}/rest/_reflect/core?tree=none")
    );
    assert_eq!(resp.headers()["content-length"], "0");
    stop(&kernel, "web_307").await;
}

/// A13.13 — no request-body limit: axum's 2 MiB `DefaultBodyLimit` refused a
/// 3 MB write where python answers `written: 3000000`.
#[tokio::test]
async fn rest_post_accepts_bodies_over_2mb() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_big", "rest", "fb_big").await;
    let blob = vec![b'x'; 3_000_000];
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/rest/fb_big"))
        .header("content-type", "application/octet-stream")
        .header(
            "x-fantastic-payload",
            r#"{"type":"write_stream","path":"big.bin","truncate":true}"#,
        )
        .body(blob)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["written"], 3_000_000, "body: {body}");
    stop(&kernel, "web_big").await;
}

/// Shared teardown.
async fn stop(kernel: &Arc<Kernel>, web_id: &str) {
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
}

/// A13.7 — the response header BLOCK, in python's order: uvicorn writes `date`
/// then `server`, starlette then the handler's own headers, then
/// `content-length`, then `content-type`; a `204` carries neither of the last
/// two. Read off the raw socket, because that is where order lives.
#[tokio::test]
async fn rest_response_header_order_matches_python() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_hdr", "rest", "fb_hdr").await;

    async fn header_names(port: u16, request: &str) -> (String, Vec<String>) {
        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        sock.write_all(request.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        loop {
            let mut chunk = [0u8; 1024];
            let n = sock.read(&mut chunk).await.expect("read");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let head = text.split("\r\n\r\n").next().unwrap_or("").to_string();
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap_or("").to_string();
        let names = lines
            .filter(|l| !l.is_empty())
            .map(|l| l.split(':').next().unwrap_or("").to_ascii_lowercase())
            .collect();
        (status, names)
    }

    // JSON reply: date, server, content-length, content-type.
    let (status, names) = header_names(
        port,
        &format!(
            "POST /rest/core HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\ncontent-type: application/json\r\ncontent-length: 22\r\n\r\n{{\"type\":\"list_agents\"}}"
        ),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert_eq!(names, ["date", "server", "content-length", "content-type"]);

    // 405: date, server, allow, content-length, content-type.
    let (status, names) = header_names(
        port,
        &format!("GET /rest/core HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 405"), "{status}");
    assert_eq!(
        names,
        ["date", "server", "allow", "content-length", "content-type"]
    );

    // 204: date, server only.
    let (status, names) = header_names(
        port,
        &format!(
            "POST /rest/fb_hdr HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\ncontent-type: application/json\r\ncontent-length: 15\r\n\r\n{{\"type\":\"boot\"}}"
        ),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 204"), "{status}");
    assert_eq!(names, ["date", "server"]);

    // octet: date, server, x-fantastic-payload, x-fantastic-bytes-field,
    // content-length, content-type.
    let (status, names) = header_names(
        port,
        &format!(
            "POST /rest/fb_hdr HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\ncontent-type: application/octet-stream\r\nx-fantastic-payload: {{\"type\":\"read_stream\",\"path\":\"hello.txt\"}}\r\ncontent-length: 0\r\n\r\n"
        ),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert_eq!(
        names,
        [
            "date",
            "server",
            "x-fantastic-payload",
            "x-fantastic-bytes-field",
            "content-length",
            "content-type"
        ]
    );
    stop(&kernel, "web_hdr").await;
}

/// A13.2 + A13.4 + A13.12 + A11.20 — the octet leg's header contract.
#[tokio::test]
async fn octet_leg_header_contract_matches_python() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_oct", "rest", "fb_oct").await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/rest/fb_oct");

    // A13.2 — the reply header is `json.dumps(rest)`: `, `/`: ` separators and
    // ensure_ascii, so `naïve` leaves as `naïve`, never raw UTF-8.
    let resp = client
        .post(&url)
        .header("content-type", "application/octet-stream")
        .header(
            "x-fantastic-payload",
            r#"{"type":"read_stream","path":"na\u00efve.txt"}"#,
        )
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()["x-fantastic-payload"],
        r#"{"path": "na\u00efve.txt", "offset": 0, "next_offset": 7, "eof": true, "size": 7}"#
    );
    assert_eq!(resp.headers()["x-fantastic-bytes-field"], "bytes");
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"naive!\n");

    // A13.4 — an EMPTY payload header parses as `{}` (`headers.get(...) or "{}"`),
    // so the dispatch is a typeless send, not a 400.
    let resp = client
        .post(&url)
        .header("content-type", "application/octet-stream")
        .header("x-fantastic-payload", "")
        .send()
        .await
        .expect("post");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "an empty header is an empty envelope"
    );
    let body: Value = resp.json().await.expect("json");
    // The envelope reached the target as `{}` — a 400 would mean the leg had
    // treated the empty value as a parse failure. (python's refusal TEXT here
    // is `send: payload.type required`, which arrives with A13.3's move of the
    // octet leg onto the batch.)
    assert!(body.get("error").is_some(), "body: {body}");

    // A13.12 — request header values decode byte-for-byte as latin-1, so a raw
    // UTF-8 path arrives mojibaked exactly as starlette hands it to python.
    let mut raw = reqwest::header::HeaderValue::from_bytes(
        "{\"type\":\"read_stream\",\"path\":\"na\u{ef}ve.txt\"}".as_bytes(),
    )
    .expect("header");
    raw.set_sensitive(false);
    let resp = client
        .post(&url)
        .header("content-type", "application/octet-stream")
        .header("x-fantastic-payload", raw)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(
        body["reason"], "not_found",
        "latin-1 mangles the path: {body}"
    );
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("na\u{c3}\u{af}ve.txt"),
        "the path is the latin-1 reading of the raw bytes: {body}"
    );

    // A11.20 — `x-fantastic-bytes-field` names the field the body lands under.
    // The default name binds the body as the chunk...
    let resp = client
        .post(&url)
        .header("content-type", "application/octet-stream")
        .header(
            "x-fantastic-payload",
            r#"{"type":"write_stream","path":"bf1.bin","truncate":true}"#,
        )
        .header("x-fantastic-bytes-field", "bytes")
        .body("abcdef")
        .send()
        .await
        .expect("post");
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["written"], 6, "body: {body}");
    // ...any OTHER name leaves the target with no chunk at all (`written: 0`)...
    let resp = client
        .post(&url)
        .header("content-type", "application/octet-stream")
        .header(
            "x-fantastic-payload",
            r#"{"type":"write_stream","path":"bf2.bin","truncate":true}"#,
        )
        .header("x-fantastic-bytes-field", "data")
        .body("abcdef")
        .send()
        .await
        .expect("post");
    let body: Value = resp.json().await.expect("json");
    assert_eq!(
        body["written"], 0,
        "a foreign field name writes nothing: {body}"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("served/bf2.bin")).unwrap(),
        b""
    );
    // ...and a header `bytes` key is a JSON value where raw bytes belong.
    let resp = client
        .post(&url)
        .header("content-type", "application/octet-stream")
        .header(
            "x-fantastic-payload",
            r#"{"type":"write_stream","path":"bf3.bin","bytes":"abc"}"#,
        )
        .header("x-fantastic-bytes-field", "data")
        .body("abcdef")
        .send()
        .await
        .expect("post");
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["reason"], "missing_arg", "body: {body}");
    assert_eq!(
        body["error"],
        "write_stream: bytes (one raw chunk) required"
    );
    assert!(!tmp.path().join("served/bf3.bin").exists());
    stop(&kernel, "web_oct").await;
}

/// A11.17 — the GET `_reflect` shortcuts read their flags exactly as python's
/// `_reflect_payload` does (`readme=1|true|yes` ⇒ `True`, any other non-falsy
/// value is a DOC KEY, `tree`/`bundles` only from the three tiers), and a null
/// reply is a bare 404.
#[tokio::test]
async fn get_reflect_query_flags_match_python() {
    assert_eq!(routes::reflect_payload(""), json!({"type": "reflect"}));
    assert_eq!(
        routes::reflect_payload("readme=true"),
        json!({"type": "reflect", "readme": true})
    );
    assert_eq!(
        routes::reflect_payload("readme=1"),
        json!({"type": "reflect", "readme": true})
    );
    assert_eq!(
        routes::reflect_payload("readme=yes"),
        json!({"type": "reflect", "readme": true})
    );
    assert_eq!(
        routes::reflect_payload("readme=0"),
        json!({"type": "reflect"})
    );
    assert_eq!(
        routes::reflect_payload("readme=no"),
        json!({"type": "reflect"})
    );
    assert_eq!(
        routes::reflect_payload("readme="),
        json!({"type": "reflect"})
    );
    assert_eq!(
        routes::reflect_payload("readme=self-extension"),
        json!({"type": "reflect", "readme": "self-extension"})
    );
    assert_eq!(
        routes::reflect_payload("tree=ids&bundles=none"),
        json!({"type": "reflect", "tree": "ids", "bundles": "none"})
    );
    assert_eq!(
        routes::reflect_payload("tree=deep"),
        json!({"type": "reflect"})
    );

    // ...and over the wire: `?readme=true` is 200 (rust used to answer 400
    // "invalid digit found in string"), and a null reply is a 404.
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_q", "rest", "fb_q").await;
    let client = reqwest::Client::new();
    for q in ["?readme=true", "?readme=1", "?tree=ids", ""] {
        let resp = client
            .get(format!("http://127.0.0.1:{port}/rest/_reflect/fb_q{q}"))
            .send()
            .await
            .expect("get");
        assert_eq!(resp.status().as_u16(), 200, "query {q:?}");
    }
    stop(&kernel, "web_q").await;
}

/// A11.11 + A11.12 — the reflect frame is python's, key for key.
#[tokio::test]
async fn reflect_sentence_and_verbs_match_python() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_r", "rest", "fb_r").await;
    let reply = kernel
        .send(vec![Item::new(
            AgentId::from("web_r"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    let obj = reply.as_object().expect("object");
    // The bundle's own frame, in python's key order; the kernel then appends
    // its reflect decoration (can_call / callable_by / kernel_verbs / tree).
    assert_eq!(
        obj.keys().map(String::as_str).take(9).collect::<Vec<_>>(),
        vec![
            "id",
            "sentence",
            "port",
            "running",
            "served_agent_count",
            "surfaces",
            "routes",
            "browser_reachable",
            "verbs",
        ]
    );
    assert_eq!(
        obj["sentence"],
        json!("HTTP host — rendering routes baked in; call surfaces mounted from sub-agents.")
    );
    assert_eq!(obj["port"], json!(port));
    assert_eq!(obj["running"], json!(true));
    assert_eq!(obj["served_agent_count"], json!(kernel.agents.len()));
    assert_eq!(
        obj["surfaces"],
        json!({"rest": ["/rest/{target_id}", "/rest/_reflect", "/rest/_reflect/{target_id}"]})
    );
    // The io leg is open AND both call gates name the host, so the file bridge
    // is the one agent a browser could actually fetch.
    assert_eq!(obj["browser_reachable"], json!(["fb_r"]));
    assert!(obj["routes"]["GET /<agent_id>/file/<path>"]
        .as_str()
        .expect("route contract")
        .starts_with("Streams a file by read_stream-ing through the named agent."));
    assert_eq!(
        obj["verbs"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["reflect", "boot", "stop", "mount", "unmount"]
    );
    assert_eq!(
        obj["verbs"]["reflect"],
        json!("Identity + uvicorn port + running flag + currently mounted surfaces (process-local; reflect the live serve for truth).")
    );
    stop(&kernel, "web_r").await;
}

/// A11.14 — `boot` answers `{running, surfaces}`, a missing port RAISES, and
/// `stop` answers `{stopped:bool}`.
#[tokio::test]
async fn boot_and_stop_replies_match_python() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_b", "rest", "fb_b").await;
    let boot = kernel
        .send(vec![Item::new(
            AgentId::from("web_b"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(boot, json!({"running": true, "surfaces": ["rest"]}));
    let stopped = kernel
        .send(vec![Item::new(
            AgentId::from("web_b"),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(stopped, json!({"stopped": true}));
    let again = kernel
        .send(vec![Item::new(
            AgentId::from("web_b"),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(again, json!({"stopped": false}));

    // A record with no `port` RAISES — no default, no fallback.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": HANDLER_MODULE, "id": "web_noport"}),
        )])
        .await
        .remove(0)
        .result;
    let boot = kernel
        .send(vec![Item::new(
            AgentId::from("web_noport"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(boot["raised"], json!(true), "boot: {boot}");
    assert_eq!(
        boot["error"],
        json!("RuntimeError: webapp web_noport: rec.port is required (no default). Set port on create_agent or via update_agent.")
    );
}

/// A11.13 — `mount` / `unmount` are verbs with python's replies, and an
/// unknown type is `web: unknown type 'x'`.
#[tokio::test]
async fn mount_unmount_and_unknown_verb_match_python() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_m", "rest", "fb_m").await;
    let call = |payload: Value| {
        let kernel = Arc::clone(&kernel);
        async move {
            kernel
                .send(vec![Item::new(AgentId::from("web_m"), payload)])
                .await
                .remove(0)
                .result
        }
    };
    assert_eq!(
        call(json!({"type": "mount"})).await,
        json!({"error": "web.mount: child_id required"})
    );
    assert_eq!(
        call(json!({"type": "unmount"})).await,
        json!({"error": "web.unmount: child_id required"})
    );
    assert_eq!(
        call(json!({"type": "mount", "child_id": "rest"})).await,
        json!({"mounted": ["/rest/{target_id}", "/rest/_reflect", "/rest/_reflect/{target_id}"]})
    );
    assert_eq!(
        call(json!({"type": "unmount", "child_id": "rest"})).await,
        json!({"unmounted": "rest"})
    );
    assert_eq!(
        call(json!({"type": "shutdown"})).await,
        json!({"error": "web: unknown type 'shutdown'"})
    );
    assert_eq!(
        call(json!({"type": "render_html"})).await,
        json!({"error": "web: unknown type 'render_html'"})
    );
    stop(&kernel, "web_m").await;
    assert_eq!(
        call(json!({"type": "mount", "child_id": "rest"})).await,
        json!({"error": "web.mount: not running"})
    );
    assert_eq!(
        call(json!({"type": "unmount", "child_id": "rest"})).await,
        json!({"error": "web.unmount: not running"})
    );
}

/// A11.18 — routes change at boot and through `mount`/`unmount`, never through
/// a state subscriber watching for `created`/`routes_changed`.
#[tokio::test]
async fn a_child_created_after_boot_mounts_only_through_mount() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = mk_kernel(&tmp, true, false);
    let web_id = "web_hot";
    for item in [json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": HANDLER_MODULE, "id": web_id, "port": port})]
    {
        let _ = kernel
            .send(vec![Item::new(AgentId::from("core"), item)])
            .await
            .remove(0)
            .result;
    }
    // Unmount before boot is refused, not silently ignored.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "stop"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        kernel
            .send(vec![Item::new(
                AgentId::from(web_id),
                json!({"type": "unmount", "child_id": "ws_hot"}),
            )])
            .await
            .remove(0)
            .result,
        json!({"error": "web.unmount: not running"})
    );
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    wait_until_port_accepts(port).await;

    // A child created AFTER boot is not routed until it is mounted.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "create_agent", "parent_id": web_id,
                   "handler_module": "web_ws.tools", "id": "ws_hot",
                   "ingress_rule": "allow_all_ingress"}),
        )])
        .await
        .remove(0)
        .result;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let ws_url = format!("http://127.0.0.1:{port}/{web_id}/ws");
    let resp = reqwest::Client::new()
        .get(&ws_url)
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404, "no auto-remount");

    assert_eq!(
        kernel
            .send(vec![Item::new(
                AgentId::from(web_id),
                json!({"type": "mount", "child_id": "ws_hot"}),
            )])
            .await
            .remove(0)
            .result,
        json!({"mounted": ["/{host_id}/ws"]})
    );
    let (sock, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web_id}/ws"))
        .await
        .expect("ws connect after mount");
    drop(sock);

    assert_eq!(
        kernel
            .send(vec![Item::new(
                AgentId::from(web_id),
                json!({"type": "unmount", "child_id": "ws_hot"}),
            )])
            .await
            .remove(0)
            .result,
        json!({"unmounted": "ws_hot"})
    );
    let resp = reqwest::Client::new()
        .get(&ws_url)
        .send()
        .await
        .expect("get");
    assert_eq!(
        resp.status().as_u16(),
        404,
        "unmount takes effect immediately"
    );
    stop(&kernel, web_id).await;
}

/// A11.1 — boot ANNOUNCES the URL by sending `say` to `cli`, and falls back to
/// the stderr line only when that send answered something (no cli ⇒ refusal).
#[tokio::test]
async fn boot_announces_say_to_cli() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    kernel.bundles.register("cli_probe.tools", SayProbe);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let port = free_port();
    for item in [
        json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": "cli_probe.tools", "id": "cli",
               "callable_by": {"$self": "*", "$parent": "*", "$children": "*", "web_say": "*"}}),
        json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": HANDLER_MODULE, "id": "web_say", "port": port,
               "can_call": {"$self": "*", "$parent": "*", "$children": "*", "cli": "*"}}),
    ] {
        let reply = kernel
            .send(vec![Item::new(AgentId::from("core"), item)])
            .await
            .remove(0)
            .result;
        assert!(reply.get("error").is_none(), "stage: {reply}");
    }
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("web_say"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    let said = SAID.lock().expect("said").clone();
    assert!(
        said.iter().any(|s| s
            == &format!(
            "listening on http://127.0.0.1:{port}/ · rest/ws are children (reflect kernel for ids)"
        )),
        "cli never heard the announcement: {said:?}"
    );
    stop(&kernel, "web_say").await;
}

/// Every `say` text the boot announcement sent, for `boot_announces_say_to_cli`.
static SAID: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A stand-in `cli` that records `say` and answers `None`, as python's does.
struct SayProbe;

#[async_trait::async_trait]
impl fantastic_kernel::bundle::Bundle for SayProbe {
    fn name(&self) -> &str {
        "cli_probe"
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
        if payload.get("type").and_then(Value::as_str) == Some("say") {
            SAID.lock().expect("said").push(
                payload
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
        Ok(None)
    }
}

/// A11.21 (D6) — a REST-originated dispatch runs inside the boot dispatch's
/// task scope, so python's uvicorn-task parentage is reproduced at the source.
///
/// RESIDUAL: the observable `parent` still differs. python reports the boot pid
/// even after that task is gone (§8 "Dangling REST parent pid", measured 7);
/// rust's `TaskTable::register_marked` DROPS a parent that has left the table,
/// so a request arriving after boot returned reports `parent: null`. Carrying a
/// dead pid is a kernel-table change, not a web one.
#[tokio::test]
async fn ingress_dispatch_parent_matches_python() {
    let kernel = Arc::new(Kernel::new());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let boot = kernel.spawn_task(&AgentId::from("web_p"), "boot", async move {
        let _ = rx.await;
    });
    let state = AppState {
        kernel: Arc::clone(&kernel),
        web_agent_id: AgentId::from("web_p"),
        boot_task: Some(boot),
    };
    let seen = state
        .scoped(async { fantastic_kernel::current_task() })
        .await;
    assert_eq!(
        seen,
        Some(boot),
        "a request handler runs under the boot task"
    );
    // While the boot task lives, work forked from a request names it as parent.
    let forked = state
        .scoped(async { kernel.spawn_task(&AgentId::from("web_p"), "ws_call", async {}) })
        .await;
    let rows = kernel.tasks.rows();
    let row = rows
        .iter()
        .find(|r| r["id"] == json!(forked.0))
        .expect("the forked task is in ps");
    assert_eq!(row["parent"], json!(boot.0), "rows: {rows:?}");
    let _ = tx.send(());
}

/// A13.16 — a client Close is ECHOED with its payload. Rust used to break the
/// loop and abort the send task before the queued reply flushed, so the client
/// saw `1006` (abnormal) instead of the code it sent.
#[tokio::test]
async fn client_close_is_echoed_with_its_code() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::Message as TMessage;

    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = mk_kernel(&tmp, true, false);
    let web_id = "web_close";
    for item in [
        json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": HANDLER_MODULE, "id": web_id, "port": port}),
        json!({"type": "create_agent", "parent_id": web_id,
               "handler_module": "web_ws.tools", "id": "ws_close",
               "ingress_rule": "allow_all_ingress"}),
    ] {
        let _ = kernel
            .send(vec![Item::new(AgentId::from("core"), item)])
            .await
            .remove(0)
            .result;
    }
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    wait_until_port_accepts(port).await;

    let (mut sock, resp) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web_id}/ws"))
            .await
            .expect("ws connect");
    // A13.17 (partial) — uvicorn's 101 block order, plus `server: uvicorn`.
    // RESIDUAL: python also sends `Sec-WebSocket-Extensions: permessage-deflate`
    // and capitalised header names; hyper lowercases every name and tungstenite
    // has no deflate codec, so neither is reproducible from here.
    let names: Vec<String> = resp
        .headers()
        .keys()
        .map(|k| k.as_str().to_string())
        .collect();
    assert_eq!(
        names,
        vec![
            "upgrade",
            "connection",
            "sec-websocket-accept",
            "date",
            "server"
        ]
    );
    assert_eq!(resp.headers()["server"], "uvicorn");

    sock.send(TMessage::Close(Some(
        tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: CloseCode::Normal,
            reason: "bye".into(),
        },
    )))
    .await
    .expect("send close");
    let echoed = tokio::time::timeout(Duration::from_secs(5), sock.next())
        .await
        .expect("close echo in time");
    match echoed {
        Some(Ok(TMessage::Close(Some(frame)))) => {
            assert_eq!(
                frame.code,
                CloseCode::Normal,
                "the client's code comes back"
            );
            assert_eq!(frame.reason, "bye");
        }
        other => panic!("expected the close echo, got {other:?}"),
    }
    stop(&kernel, web_id).await;
}

// ─── C86 / A13.14 binary half — a watcher gets the BODY ──────────────

#[tokio::test]
async fn ws_watcher_gets_a_binary_frame_for_a_bytes_payload() {
    // A browser watching an agent that receives a `write_stream` must get the
    // BYTES, as python does: `[4B len | head with _binary_path | body]`. The
    // port fanned out the header without the blob, so no watcher could ever
    // receive the binary frame python sends.
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message as TMessage;

    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_watch_bin";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;

    // The connection AUTO-WATCHES the host agent it connected to.
    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    // Let the auto-watch land before the traffic it must mirror.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let blob: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x01];
    let _ = kernel
        .send_with_binary(
            &AgentId::from(web_id),
            json!({"type": "write_stream", "path": "x.bin"}),
            blob.clone(),
        )
        .await;

    let mut binary: Option<Vec<u8>> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while tokio::time::Instant::now() < deadline && binary.is_none() {
        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
            Ok(Some(Ok(TMessage::Binary(b)))) => binary = Some(b),
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => break,
        }
    }
    let wire = binary.expect("the watcher must receive a BINARY event frame");
    let (header, body) =
        fantastic_io_bridge::codec::decode_binary_frame(&wire).expect("a well-formed frame");
    assert_eq!(header["type"], "event");
    assert_eq!(header["payload"]["type"], "write_stream");
    assert_eq!(header["payload"]["path"], "x.bin");
    assert_eq!(header["payload"]["bytes"], Value::Null);
    assert_eq!(header["_binary_path"], "payload.bytes");
    assert_eq!(body, blob, "the raw body rides beside the head, not base64");

    stop(&kernel, web_id).await;
}
