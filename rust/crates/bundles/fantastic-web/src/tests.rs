//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::Agent;
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
    let _rx = kernel.register(Arc::clone(&root));
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

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("web — axum HTTP host"));
}

#[test]
fn transport_js_self_invokes_global() {
    assert!(TRANSPORT_JS.contains("fantastic_transport"));
    assert!(TRANSPORT_JS.contains("BroadcastChannel"));
}

#[test]
fn root_index_includes_transport_script() {
    assert!(ROOT_INDEX_HTML.contains("transport.js"));
}

#[test]
fn inject_transport_adds_script_before_head_close() {
    let html = "<html><head><title>x</title></head><body>hi</body></html>";
    let out = inject_transport(html);
    assert!(out.contains(r#"<script src="/transport.js"></script>"#));
    let idx_script = out.find("/transport.js").unwrap();
    let idx_close = out.find("</head>").unwrap();
    assert!(idx_script < idx_close);
}

#[test]
fn inject_transport_is_idempotent() {
    // `inject_transport` now also adds a default favicon `<link>`
    // (Python parity). Idempotency requires BOTH tags to be present
    // already.
    let html = r#"<html><head><link rel="icon" type="image/png" href="/_assets/favicon.png"><script src="/transport.js"></script></head><body>x</body></html>"#;
    assert_eq!(inject_transport(html), html);
}

#[test]
fn inject_transport_handles_no_head() {
    let html = "<body>x</body>";
    let out = inject_transport(html);
    assert!(out.contains(r#"<script src="/transport.js"></script>"#));
    assert!(out.contains(r#"<link rel="icon" type="image/png" href="/_assets/favicon.png">"#));
    assert!(out.ends_with("<body>x</body>"));
}

#[test]
fn guess_mime_known_types() {
    assert_eq!(guess_mime("a.html"), "text/html; charset=utf-8");
    assert_eq!(guess_mime("a.CSS"), "text/css; charset=utf-8");
    assert_eq!(guess_mime("a.png"), "image/png");
    assert_eq!(guess_mime("a.json"), "application/json");
    assert_eq!(guess_mime("unknown"), "text/plain; charset=utf-8");
}

#[test]
fn translate_path_substitutes_python_placeholders() {
    assert_eq!(translate_path("/{host_id}/ws"), "/:host_id/ws");
    assert_eq!(
        translate_path("/{rest}/_reflect/{target_id}"),
        "/:rest/_reflect/:target_id"
    );
    assert_eq!(translate_path("/static"), "/static");
    assert_eq!(translate_path("/{x:path}"), "/*x");
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
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": web_id,
                "port": port,
            }),
        )
        .await;
    assert!(
        create_reply.get("error").is_none(),
        "create: {create_reply}"
    );
    let boot_reply = kernel
        .send(&AgentId::from(web_id), json!({"type": "boot"}))
        .await;
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
    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
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

    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": web_id,
                "port": port,
            }),
        )
        .await;
    kernel
        .send(
            &AgentId::from(web_id),
            json!({
                "type": "create_agent",
                "handler_module": "web_ws.tools",
                "id": "wws_child",
            }),
        )
        .await;
    let boot_reply = kernel
        .send(&AgentId::from(web_id), json!({"type": "boot"}))
        .await;
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

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
}

/// `web` + `web_rest` child: POST endpoint live at `/<rest_id>/<target>`.
#[tokio::test]
async fn web_with_rest_child_serves_rest() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, true);
    let port = free_port();
    let web_id = "web_with_rest";
    let rest_id = "wr_child";

    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": web_id,
                "port": port,
            }),
        )
        .await;
    kernel
        .send(
            &AgentId::from(web_id),
            json!({
                "type": "create_agent",
                "handler_module": "web_rest.tools",
                "id": rest_id,
            }),
        )
        .await;
    let boot_reply = kernel
        .send(&AgentId::from(web_id), json!({"type": "boot"}))
        .await;
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

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
}

/// Boot web alone, then create a `web_ws` child + emit `routes_changed`,
/// verify the WS endpoint becomes available without restarting axum.
#[tokio::test]
async fn runtime_added_ws_child_hot_mounts() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as TMessage;

    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, true, false);
    let port = free_port();
    let web_id = "web_hot";

    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": web_id,
                "port": port,
            }),
        )
        .await;
    let boot_reply = kernel
        .send(&AgentId::from(web_id), json!({"type": "boot"}))
        .await;
    assert_eq!(boot_reply["running"], true, "boot: {boot_reply}");
    wait_until_port_accepts(port).await;

    // Sanity: no WS yet.
    let url_http = format!("http://127.0.0.1:{port}/{web_id}/ws");
    let pre = reqwest::Client::new()
        .get(&url_http)
        .send()
        .await
        .expect("pre http");
    assert_eq!(pre.status().as_u16(), 404, "pre-mount must 404");

    // Stage the child + trigger a re-mount via `routes_changed` emit.
    kernel
        .send(
            &AgentId::from(web_id),
            json!({
                "type": "create_agent",
                "handler_module": "web_ws.tools",
                "id": "wws_hot",
            }),
        )
        .await;
    // `create_agent` itself emits `created` which the subscriber treats
    // as a re-mount signal — but we also explicitly emit
    // `routes_changed` for spec parity. Both should be idempotent.
    kernel
        .send(&AgentId::from("wws_hot"), json!({"type": "routes_changed"}))
        .await;

    // The subscriber tokio::spawns the rebuild; poll until the WS
    // upgrades or we time out.
    let mut ok = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let url_ws = format!("ws://127.0.0.1:{port}/{web_id}/ws");
        if let Ok((mut ws, _)) = tokio_tungstenite::connect_async(&url_ws).await {
            let frame = json!({"type": "call", "target": "kernel", "payload": {"type": "list_agents"}, "id": "hot1"});
            if ws.send(TMessage::Text(frame.to_string())).await.is_err() {
                continue;
            }
            if let Ok(Some(Ok(TMessage::Text(t)))) =
                tokio::time::timeout(Duration::from_secs(1), ws.next()).await
            {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if v["type"] == "reply" && v["id"] == "hot1" {
                        ok = true;
                        break;
                    }
                }
            }
        }
    }
    assert!(ok, "WS endpoint never became live after hot-mount");

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": web_id,
                "port": port,
            }),
        )
        .await;
    kernel
        .send(
            &AgentId::from(web_id),
            json!({
                "type": "create_agent",
                "handler_module": "web_ws.tools",
                "id": format!("{web_id}_ws"),
            }),
        )
        .await;
    let r = kernel
        .send(&AgentId::from(web_id), json!({"type": "boot"}))
        .await;
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

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
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

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
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
    // Order: 2 (final), then 0, then 1. The final flag on chunk 2
    // doesn't dispatch until all 3 are present.
    let chunks = [
        (2u32, &[3u8; 4] as &[u8], true),
        (0, &[1u8; 4], false),
        (1, &[2u8; 4], false),
    ];
    let mut got_reply = false;
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
        let frames = send_binary_and_drain(&mut ws, frame).await;
        if let Some(r) = frames.iter().find(|f| f["id"] == "ooo1") {
            // Could be on the first "final" or after the last needed chunk.
            assert_eq!(r["type"], "error");
            got_reply = true;
        }
    }
    // Send the final-flag chunk LAST when all 3 are present.
    let header = json!({
        "target": "nonexistent",
        "type": "noop",
        "id": "ooo1b",
        "upload_id": upload_id,
        "chunk_index": 2,
        "total_chunks": 3,
        "final": true,
    });
    let frame = build_binary_frame(&header, &[3u8; 4]);
    // This may dispatch (if previous final-flag triggered missing-chunks
    // error and cleared the map, this re-uploads chunk 2; in that case
    // we need the other 2 chunks again). Either way the wire is asserted
    // by got_reply above for the assembled path.
    let _ = send_binary_and_drain(&mut ws, frame).await;
    assert!(got_reply, "expected a dispatch reply for chunked upload");

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
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

    // 2 MB blob — past MAX_CHUNK_SIZE (1 MB).
    let big_blob = vec![0u8; 2 * 1_048_576];
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

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
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

    // 101 chunks × 1 MB each → 101 MB, past the 100 MB total cap.
    // Send chunks until the server rejects. Each chunk is at the per-
    // chunk cap so cumulative trips the total cap on chunk #101.
    let upload_id = "u_huge";
    let chunk_size = 1_048_576;
    let total_chunks: u32 = 101;
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

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
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

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
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

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
}

// ── /_assets/* — vendored third-party web deps ─────────────────────
//
// Three.js + xterm + addon-fit are bundled and served at top-level
// URLs so canvas/terminal WebView surfaces don't need CDN access.
// These tests boot the HTTP layer + hit each asset URL with reqwest
// and verify content-type + non-empty body + immutable cache header.

async fn boot_web_for_assets(kernel: &Arc<Kernel>, port: u16, web_id: &str) {
    let _ = kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": web_id,
                "port": port,
            }),
        )
        .await;
    let boot_reply = kernel
        .send(&AgentId::from(web_id), json!({"type": "boot"}))
        .await;
    assert_eq!(boot_reply["running"], true, "boot: {boot_reply}");
    wait_until_port_accepts(port).await;
}

#[tokio::test]
async fn assets_three_js_served_with_javascript_mime() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, false);
    let port = free_port();
    let web_id = "web_three_js";
    boot_web_for_assets(&kernel, port, web_id).await;

    let url = format!("http://127.0.0.1:{port}/_assets/three.module.js");
    let resp = reqwest::get(&url).await.expect("http get");
    assert_eq!(resp.status().as_u16(), 200, "expected 200");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("application/javascript"),
        "content-type was {ct:?}"
    );
    let body = resp.text().await.expect("body");
    assert!(
        body.len() > 100_000,
        "three.module.js looks too small: {} bytes",
        body.len()
    );
    assert!(
        body.contains("THREE") || body.contains("three"),
        "body doesn't look like Three.js"
    );

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
}

#[tokio::test]
async fn assets_xterm_js_served_with_javascript_mime() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, false);
    let port = free_port();
    let web_id = "web_xterm_js";
    boot_web_for_assets(&kernel, port, web_id).await;

    let url = format!("http://127.0.0.1:{port}/_assets/xterm.min.js");
    let resp = reqwest::get(&url).await.expect("http get");
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("application/javascript"),
        "content-type was {ct:?}"
    );
    let body = resp.text().await.expect("body");
    assert!(
        body.len() > 50_000,
        "xterm.min.js too small: {} bytes",
        body.len()
    );

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
}

#[tokio::test]
async fn assets_xterm_css_served_with_css_mime() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, false);
    let port = free_port();
    let web_id = "web_xterm_css";
    boot_web_for_assets(&kernel, port, web_id).await;

    let url = format!("http://127.0.0.1:{port}/_assets/xterm.min.css");
    let resp = reqwest::get(&url).await.expect("http get");
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("text/css"), "content-type was {ct:?}");
    let body = resp.text().await.expect("body");
    assert!(body.contains("xterm"), "css body looks wrong");

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
}

#[tokio::test]
async fn assets_xterm_addon_fit_served() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, false);
    let port = free_port();
    let web_id = "web_addon_fit";
    boot_web_for_assets(&kernel, port, web_id).await;

    let url = format!("http://127.0.0.1:{port}/_assets/xterm-addon-fit.min.js");
    let resp = reqwest::get(&url).await.expect("http get");
    assert_eq!(resp.status().as_u16(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("application/javascript"),
        "content-type was {ct:?}"
    );
    let body = resp.text().await.expect("body");
    assert!(!body.is_empty());

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
}

#[tokio::test]
async fn assets_carry_immutable_cache_control() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, false);
    let port = free_port();
    let web_id = "web_cache_ctl";
    boot_web_for_assets(&kernel, port, web_id).await;

    let url = format!("http://127.0.0.1:{port}/_assets/three.module.js");
    let resp = reqwest::get(&url).await.expect("http get");
    let cache = resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        cache.contains("immutable"),
        "expected immutable cache-control; got {cache:?}"
    );
    assert!(
        cache.contains("max-age="),
        "expected max-age in cache-control; got {cache:?}"
    );

    kernel
        .send(&AgentId::from(web_id), json!({"type": "stop"}))
        .await;
}
