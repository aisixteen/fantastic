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
/// practice.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 0");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Build a kernel with web (+ optionally child surface bundles)
/// registered, with a root agent set as `kernel_state`.
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
    // The file proxy and the stream verbs need a real SOURCE to talk to.
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle::default());
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
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

/// The ROOT accepts every verb from `ids` — the operator widens its
/// `callable_by` (kernel verbs at the root take the two-sided gate too).
async fn accept_on_root(kernel: &Arc<Kernel>, ids: &[&str]) {
    let root = kernel.root().expect("a root");
    let mut grant = root
        .meta
        .read()
        .expect("meta")
        .get("callable_by")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for id in ids {
        grant.insert((*id).to_string(), json!("*"));
    }
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": root.id.as_str(), "callable_by": grant}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "accept_on_root: {r}");
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
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]},
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
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]},
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
                "handler_module": "web_ws.tools", "callable_by": {"$parent": ["get_routes"]},
                "id": "wws_child",
                // A WS frame runs as the LEG, and a leg with no
                // `ingress_rule` is SEALED. An open face says so.
                "ingress_rule": "allow_all_ingress",
                // A grant map is keyed by a LITERAL id or a `$token` — `"*"` is
                // never a key, only a verb wildcard.
                "can_call": {"$parent": ["watch"], "kernel_state": "*"},
            }),
        )])
        .await
        .remove(0)
        .result;
    accept_on_root(&kernel, &["wws_child"]).await;
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
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]},
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
                "handler_module": "web_rest.tools", "callable_by": {"$parent": ["get_routes"]},
                "id": rest_id,
                // Deny-all: an inbound relay dispatches browser calls into the
                // kernel AS ITSELF — ingress_rule says WHO may call in, can_call
                // says WHERE into the kernel it lands. This test POSTs to `kernel_state`.
                // The leg SEALS without an explicit rule (403 before dispatch),
                // so open it here as a browser-facing dev leg would be.
                "ingress_rule": "allow_all_ingress",
                "can_call": { "kernel_state": "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
    // Two-sided deny-all: the relay dispatches into root AS ITSELF, and the root's
    // baseline is `$self` alone — open root's ingress for the relay (merged with
    // that baseline).
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": "kernel_state", "callable_by": {
                "$self": "*", "$parent": "*", "$subtree": "*", rest_id: "*",
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

    let url = format!("http://127.0.0.1:{port}/{rest_id}/kernel_state");
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

// ── The WS frame loop ──────────────────────────────────────────────
//
// One wire shape for both directions:
//
//     text   : the JSON envelope
//     binary : [4B BE hdr_len][header JSON + `_binary_path`][raw blob]
//
// There is NO chunked-upload protocol any more: python has none, and
// the `upload_id`/`chunk_index`/`total_chunks` frames rust invented were a
// second, incompatible dialect nothing else on the wire spoke. A big upload is
// a big binary frame, or a `write_stream` per offset.

/// Stage a web + web_ws on `port`. `open_leg` writes
/// `ingress_rule: allow_all_ingress` onto the web_ws child — WITHOUT it the leg
/// is SEALED and every frame is denied or dropped, which is what a bare
/// leg means everywhere else in this kernel.
async fn stage_web_with_ws_leg(
    tmp: &TempDir,
    port: u16,
    web_id: &str,
    open_leg: bool,
) -> Arc<Kernel> {
    let kernel = mk_kernel(tmp, true, false);
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": "*"}, "can_call": {"$subtree": ["get_routes"]},
                "id": web_id,
                "port": port,
            }),
        )])
        .await
        .remove(0)
        .result;
    let mut ws_rec = json!({
        "type": "create_agent",
        "parent_id": web_id,
        "handler_module": "web_ws.tools", "can_call": {"$parent": ["watch"]}, "callable_by": {"$parent": ["get_routes"]},
        "id": format!("{web_id}_ws"),
    });
    if open_leg {
        ws_rec["ingress_rule"] = json!("allow_all_ingress");
        // The leg is the SENDER of every frame now, so it needs an egress
        // grant as well as an open ingress rule — the two gates are independent
        // and both have to be wired by an operator. A grant map is keyed by a
        // LITERAL id, never `"*"`.
        ws_rec["can_call"] = json!({"kernel_state": "*", "$parent": "*"});
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel"), ws_rec)])
        .await
        .remove(0)
        .result;
    if open_leg {
        accept_on_root(&kernel, &[&format!("{web_id}_ws")]).await;
    }
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

/// The common case: an OPEN leg.
async fn stage_web_with_ws(tmp: &TempDir, port: u16, web_id: &str) -> Arc<Kernel> {
    stage_web_with_ws_leg(tmp, port, web_id, true).await
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

/// A binary frame's header AS IT CROSSED the wire — `_binary_path` still in it,
/// the key order kept.
fn wire_head(wire: &[u8]) -> Value {
    let h = u32::from_be_bytes([wire[0], wire[1], wire[2], wire[3]]) as usize;
    serde_json::from_slice(&wire[4..4 + h]).expect("a JSON header")
}

type TestWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Collect every frame the server emits during a short window, text and binary.
async fn drain_frames(ws: &mut TestWs, window_ms: u64) -> (Vec<Value>, Vec<Vec<u8>>) {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message as TMessage;
    let (mut text, mut binary) = (Vec::new(), Vec::new());
    let deadline = tokio::time::Instant::now() + Duration::from_millis(window_ms);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
            Ok(Some(Ok(TMessage::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    text.push(v);
                }
            }
            Ok(Some(Ok(TMessage::Binary(b)))) => binary.push(b),
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => break,
        }
    }
    (text, binary)
}

/// Send a raw WS message and drain what comes back.
async fn round_trip(
    ws: &mut TestWs,
    msg: tokio_tungstenite::tungstenite::Message,
) -> (Vec<Value>, Vec<Vec<u8>>) {
    use futures_util::SinkExt;
    ws.send(msg).await.expect("send");
    drain_frames(ws, 800).await
}

/// Create a file_bridge child of the root, open for reads and writes, and wire
/// BOTH gate ends between it and the WS leg — the leg is the sender of every
/// frame now, so `callable_by` on the bridge and `can_call` on the leg are
/// what make a browser fetch reach disk at all.
async fn make_file_bridge(kernel: &Arc<Kernel>, id: &str, root: &std::path::Path, leg: &str) {
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent",
                "parent_id": "kernel",
                "handler_module": "file_bridge.tools", "allow": ["."],
                "id": id,
                "root": root.to_string_lossy(),
                "ingress_rule": "allow_all_ingress",
                "callable_by": {leg: "*"},
            }),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "create file_bridge: {r}");
    let g = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": leg, "can_call": {"kernel_state": "*", id: "*"}}),
        )])
        .await
        .remove(0)
        .result;
    assert!(g.get("error").is_none(), "grant the leg: {g}");
}

/// A binary frame is an ordinary envelope plus a body at
/// A TEXT `call` carrying `write_stream` rides the binary channel for the reply
/// shape, but its `bytes` key is the CLIENT'S JSON value — python only
/// overwrites it with `payload[field] = body` on a binary frame. So a string
/// there is `missing_arg` and an absent key is an empty chunk. Rust stripped the
/// key on both, so `{"bytes": "xyz"}` wrote 0 bytes and answered success.
#[tokio::test]
async fn a_text_write_stream_keeps_the_clients_bytes_value() {
    use tokio_tungstenite::tungstenite::Message as TMessage;
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_ws_text_bytes";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    let files = tmp.path().join("files");
    std::fs::create_dir_all(&files).unwrap();
    make_file_bridge(&kernel, "fb_txt", &files, "web_ws_text_bytes_ws").await;

    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    // A JSON STRING where raw bytes belong — refused, nothing written.
    let frame = json!({
        "type": "call", "target": "fb_txt", "id": "s1",
        "payload": {"type": "write_stream", "path": "t.txt", "offset": 0, "bytes": "xyz"},
    });
    let (text, _) = round_trip(&mut ws, TMessage::Text(frame.to_string())).await;
    let reply = text
        .iter()
        .find(|f| f["id"] == "s1")
        .unwrap_or_else(|| panic!("no reply for s1: {text:?}"));
    assert_eq!(
        reply["data"]["error"], "write_stream: bytes (one raw chunk) required",
        "{reply}"
    );
    assert_eq!(reply["data"]["reason"], "missing_arg", "{reply}");
    assert_eq!(reply["data"]["arg"], "bytes (one raw chunk)", "{reply}");
    assert!(!files.join("t.txt").exists(), "the refusal still wrote");

    // NO key at all — python's `payload.get("bytes", b"")`: an empty chunk.
    let frame = json!({
        "type": "call", "target": "fb_txt", "id": "s2",
        "payload": {"type": "write_stream", "path": "t2.txt", "offset": 0},
    });
    let (text, _) = round_trip(&mut ws, TMessage::Text(frame.to_string())).await;
    let reply = text
        .iter()
        .find(|f| f["id"] == "s2")
        .unwrap_or_else(|| panic!("no reply for s2: {text:?}"));
    assert_eq!(reply["data"]["written"], 0, "{reply}");
    assert_eq!(reply["data"]["size"], 0, "{reply}");

    stop(&kernel, web_id).await;
}

/// `_binary_path` — routed by `type` exactly like a text frame, with the frame
/// `id` naming the execution and the blob reaching the target's binary
/// channel.
#[tokio::test]
async fn binary_ws_frame_routes_like_a_text_call() {
    use tokio_tungstenite::tungstenite::Message as TMessage;
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_bin_route";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    let files = tmp.path().join("files");
    std::fs::create_dir_all(&files).unwrap();
    make_file_bridge(&kernel, "fb_bin", &files, "web_bin_route_ws").await;

    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    let blob = vec![7u8; 10_240];
    let header = json!({
        "type": "call", "target": "fb_bin", "id": "w1",
        "payload": {"type": "write_stream", "path": "x.bin", "truncate": true},
        "_binary_path": ["payload", "bytes"],
    });
    let (text, _) = round_trip(
        &mut ws,
        TMessage::Binary(build_binary_frame(&header, &blob)),
    )
    .await;
    let reply = text
        .iter()
        .find(|f| f["id"] == "w1")
        .unwrap_or_else(|| panic!("no reply for w1: {text:?}"));
    assert_eq!(reply["type"], "reply", "{reply}");
    assert_eq!(reply["data"]["written"], 10_240, "{reply}");
    assert_eq!(
        std::fs::read(files.join("x.bin")).expect("the blob landed"),
        blob
    );

    // A TEXT `read_stream` call answers a BINARY frame whose head keeps
    // `bytes: null` in its python position and names it last.
    let frame = json!({
        "type": "call", "target": "fb_bin", "id": "r1",
        "payload": {"type": "read_stream", "path": "x.bin", "offset": 0, "length": 4096},
    });
    let (_, bins) = round_trip(&mut ws, TMessage::Text(frame.to_string())).await;
    let wire = bins.first().expect("a binary reply frame");
    let head = wire_head(wire);
    assert_eq!(head["type"], "reply");
    assert_eq!(head["id"], "r1");
    assert_eq!(head["_binary_path"], json!(["data", "bytes"]));
    assert_eq!(head["data"]["bytes"], Value::Null);
    let body = fantastic_io_bridge::codec::decode_binary_frame(wire)
        .expect("well-formed")
        .body;
    let keys: Vec<&str> = head["data"]
        .as_object()
        .expect("data is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "path",
            "offset",
            "bytes",
            "next_offset",
            "eof",
            "size",
            "origin"
        ],
        "python's key order, `bytes` nulled IN PLACE"
    );
    assert_eq!(body, vec![7u8; 4096]);

    stop(&kernel, web_id).await;
}

/// Every undecodable frame gets the ONE frame error and the socket
/// SURVIVES — a 3-byte binary frame, a header length past the end, JSON that is
/// not an object, a `_binary_path` python's `set_path` cannot place, and the old
/// dotted-string spelling of one (not a list of steps).
#[tokio::test]
async fn malformed_frames_answer_the_frame_error_and_keep_the_socket() {
    use tokio_tungstenite::tungstenite::Message as TMessage;
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_bad_frames";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    let expect_error = |frames: &[Value], what: &str| {
        let f = frames
            .iter()
            .find(|f| f["type"] == "error")
            .unwrap_or_else(|| panic!("{what}: no error frame in {frames:?}"));
        assert_eq!(
            *f,
            json!({
                "type": "error",
                "error": "frame: frame must be a well-formed call/emit/watch envelope",
                "reason": "bad_arg",
                "arg": "frame",
            }),
            "{what}"
        );
    };

    let (t, _) = round_trip(&mut ws, TMessage::Text(String::new())).await;
    expect_error(&t, "empty text");
    let (t, _) = round_trip(&mut ws, TMessage::Text("[1]".into())).await;
    expect_error(&t, "a JSON array");
    let (t, _) = round_trip(&mut ws, TMessage::Text("\"call\"".into())).await;
    expect_error(&t, "a JSON string");
    let (t, _) = round_trip(&mut ws, TMessage::Binary(vec![0, 1, 2])).await;
    expect_error(&t, "a 3-byte binary frame");
    let (t, _) = round_trip(&mut ws, TMessage::Binary(vec![0xFF, 0xFF, 0xFF, 0xFF, 1])).await;
    expect_error(&t, "a header length past the end");
    let bad_path = json!({
        "type": "call", "target": "kernel", "id": "bp",
        "payload": {}, "_binary_path": ["payload", "nope", "deep"],
    });
    let (t, _) = round_trip(
        &mut ws,
        TMessage::Binary(build_binary_frame(&bad_path, b"x")),
    )
    .await;
    expect_error(&t, "an unplaceable _binary_path");
    let dotted = json!({
        "type": "call", "target": "kernel", "id": "bd",
        "payload": {"bytes": null}, "_binary_path": "payload.bytes",
    });
    let (t, _) = round_trip(&mut ws, TMessage::Binary(build_binary_frame(&dotted, b"x"))).await;
    expect_error(&t, "the old dotted _binary_path");

    // THE SOCKET SURVIVED all seven: an ordinary call still answers.
    let good =
        json!({"type": "call", "target": "kernel", "id": "ok", "payload": {"type": "reflect"}});
    let (t, _) = round_trip(&mut ws, TMessage::Text(good.to_string())).await;
    assert!(
        t.iter().any(|f| f["id"] == "ok" && f["type"] == "reply"),
        "the socket died on a bad frame: {t:?}"
    );

    stop(&kernel, web_id).await;
}

/// A SEALED leg (no `ingress_rule`) denies a `call` AND TEACHES, skips the
/// connect-time auto-watch entirely, and drops `emit`/`watch`/`state_subscribe`
/// in silence — no frame runs as the operator, and the sealed socket receives
/// none of the host's term events.
#[tokio::test]
async fn sealed_leg_denies_the_call_and_leaks_no_telemetry() {
    use tokio_tungstenite::tungstenite::Message as TMessage;
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_sealed_ws";
    let kernel = stage_web_with_ws_leg(&tmp, port, web_id, false).await;
    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    let frame =
        json!({"type": "call", "target": "kernel", "id": "c1", "payload": {"type": "reflect"}});
    let (t, _) = round_trip(&mut ws, TMessage::Text(frame.to_string())).await;
    let reply = t
        .iter()
        .find(|f| f["id"] == "c1")
        .unwrap_or_else(|| panic!("no reply: {t:?}"));
    assert_eq!(reply["type"], "reply");
    assert_eq!(reply["data"]["reason"], "unauthorized", "{reply}");
    assert!(
        reply["data"]["hint"].is_string() || reply["data"]["see"].is_string(),
        "a denial must teach how to open the edge: {reply}"
    );
    assert!(reply["data"]["tree"].is_null(), "the reflect must NOT run");

    // NO AUTO-WATCH: an event on the host reaches nobody.
    kernel
        .emit(
            &AgentId::from(web_id),
            json!({"type": "output", "data": "x"}),
        )
        .await;
    let (t, b) = drain_frames(&mut ws, 400).await;
    assert!(
        !t.iter().any(|f| f["type"] == "event") && b.is_empty(),
        "a sealed leg auto-watched and leaked: {t:?}"
    );

    // `state_subscribe` is dropped, snapshot included.
    let sub = json!({"type": "state_subscribe", "id": "s1"});
    let (t, _) = round_trip(&mut ws, TMessage::Text(sub.to_string())).await;
    assert!(
        !t.iter().any(|f| f["type"] == "state_snapshot"),
        "a sealed leg answered a state snapshot: {t:?}"
    );

    stop(&kernel, web_id).await;
}

/// An open leg DOES auto-watch the host, and every frame runs as the LEG,
/// not as the operator — so an agent that does not accept the leg is refused
/// over the socket even though the leg's own ingress rule is open.
#[tokio::test]
async fn open_leg_autowatches_and_calls_run_as_the_leg() {
    use tokio_tungstenite::tungstenite::Message as TMessage;
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_open_ws";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    // A private agent: nothing may call it.
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "web_ws.tools", "can_call": {"$parent": ["watch"]}, "id": "private_one",
                "callable_by": {},
            }),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");

    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    tokio::time::sleep(Duration::from_millis(150)).await;

    kernel
        .emit(
            &AgentId::from(web_id),
            json!({"type": "output", "data": "hi"}),
        )
        .await;
    let (t, _) = drain_frames(&mut ws, 600).await;
    assert!(
        t.iter().any(|f| f["type"] == "event"),
        "an open leg must auto-watch the host: {t:?}"
    );
    // Every mirrored event names the agent it came from — the name the socket
    // watched — so a bridge on the other end re-emits only what it subscribed to.
    let ev = t.iter().find(|f| f["type"] == "event").unwrap();
    assert_eq!(ev["src"], json!(web_id), "{ev}");
    assert_eq!(
        ev.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["type", "src", "payload"]
    );

    let frame = json!({"type": "call", "target": "private_one", "id": "p1", "payload": {"type": "reflect"}});
    let (t, _) = round_trip(&mut ws, TMessage::Text(frame.to_string())).await;
    let reply = t
        .iter()
        .find(|f| f["id"] == "p1")
        .unwrap_or_else(|| panic!("no reply: {t:?}"));
    assert_eq!(
        reply["data"]["reason"], "unauthorized",
        "a WS call ran with more authority than the leg: {reply}"
    );

    stop(&kernel, web_id).await;
}

/// `target` defaults to `""` (never the URL host), `payload` to
/// `{}`, and the frame `id` is echoed RAW — an int id comes back an int.
#[tokio::test]
async fn call_frame_without_id_or_payload_replies_like_python() {
    use tokio_tungstenite::tungstenite::Message as TMessage;
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_frame_defaults";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    // No target at all: python sends `""`, which the kernel refuses — the URL
    // host is NEVER substituted.
    let (t, _) = round_trip(&mut ws, TMessage::Text(json!({"type": "call"}).to_string())).await;
    let reply = t
        .iter()
        .find(|f| f["type"] == "reply")
        .unwrap_or_else(|| panic!("no reply: {t:?}"));
    assert_eq!(reply["id"], "", "an absent id echoes as the empty string");
    assert_eq!(reply["data"]["error"], "send: target required", "{reply}");

    // A non-string id is echoed VERBATIM and names no execution.
    let frame =
        json!({"type": "call", "target": "kernel", "id": 7, "payload": {"type": "reflect"}});
    let (t, _) = round_trip(&mut ws, TMessage::Text(frame.to_string())).await;
    let reply = t
        .iter()
        .find(|f| f["id"] == 7)
        .unwrap_or_else(|| panic!("the raw id was not echoed: {t:?}"));
    assert_eq!(reply["type"], "reply", "{reply}");
    assert!(
        reply["data"].is_object(),
        "a non-string id must still dispatch: {reply}"
    );

    stop(&kernel, web_id).await;
}

// ── /_assets/* — vendored third-party web deps ─────────────────────
//
// Three.js + xterm + addon-fit are bundled and served at top-level
// URLs so canvas/terminal WebView surfaces don't need CDN access.
// These tests boot the HTTP layer + hit each asset URL with reqwest
// and verify content-type + non-empty body + immutable cache header.

/// The BINARY leg of the REST surface (octet-stream body) — mirror of the
/// Python `web_rest` binary leg. An octet POST of a file_bridge `edit` carries
/// the patch in the raw body and the envelope on `X-Fantastic-Payload`; the
/// reply is JSON `{edited:true}`. A `read_stream` returns an OCTET reply whose
/// body is the raw file bytes and whose `X-Fantastic-Payload` header carries the
/// rest of the envelope. Never base64.
#[tokio::test]
async fn web_rest_octet_leg_edit_and_read_stream() {
    // Kernel with web + web_rest + file_bridge registered.
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    kernel
        .bundles
        .register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle::default());
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
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
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": web_id, "port": port,
            }),
        )])
        .await
        .remove(0)
        .result;
    // file_bridge — OPEN (its own io-bridge gate) + accept the REST surface as a
    // caller (two-sided call gate: ingress side).
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "file_bridge.tools", "allow": ["."], "id": fb_id,
                "root": "served",
                "ingress_rule": "allow_all_ingress",
                "callable_by": { "$self": "*", "$parent": "*", "$subtree": "*", rest_id: "*" },
            }),
        )])
        .await
        .remove(0)
        .result;
    // web_rest child — may reach the file_bridge (egress side).
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": web_id,
                "handler_module": "web_rest.tools", "callable_by": {"$parent": ["get_routes"]}, "id": rest_id,
                "ingress_rule": "allow_all_ingress",
                "can_call": { fb_id: "*" },
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
    let client = reqwest::Client::new();

    // ── edit: the patch rides the raw octet body ─────────────────────────
    let patch = "<<<<<<< SEARCH\nhello world\n=======\ngoodbye world\n>>>>>>> REPLACE\n";
    let resp = client
        .post(&url)
        .header("Content-Type", "application/octet-stream")
        .header(
            "X-Fantastic-Payload",
            r#"{"type":"edit","edits":[{"path":"greeting.txt","hunks":1}]}"#,
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
    assert_eq!(pbody["edited"], true, "patch reply: {pbody}");
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
/// A leg carrying NO `ingress_rule` is sealed, as everywhere else in the io
/// family: it serves no unauthenticated request, and its refusal names the true
/// cause (this leg is sealed), never an EGRESS one ("wr_sealed may not call
/// \"reflect\" on …").
#[tokio::test]
async fn sealed_rest_leg_answers_403_and_teaches_the_repair() {
    let tmp = TempDir::new().unwrap();
    let kernel = mk_kernel(&tmp, false, true);
    let port = free_port();
    let web_id = "web_sealed_rest";
    let rest_id = "wr_sealed";

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": web_id, "port": port}),
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
                   "handler_module": "web_rest.tools", "callable_by": {"$parent": ["get_routes"]}, "id": rest_id,
                   "can_call": {"kernel_state": "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": "kernel_state", "callable_by": {
                "$self": "*", "$parent": "*", "$subtree": "*", rest_id: "*"}}),
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
        .post(format!("http://127.0.0.1:{port}/{rest_id}/kernel_state"))
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
        format!("http://127.0.0.1:{port}/{rest_id}/_reflect/kernel_state"),
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
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": web_id, "port": port}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": web_id,
                   "handler_module": "web_rest.tools", "callable_by": {"$parent": ["get_routes"]}, "id": rest_id,
                   "ingress_rule": "allow_all_ingress",
                   "can_call": {"kernel_state": "*", "kernel": "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": "kernel_state", "callable_by": {
                "$self": "*", "$parent": "*", "$subtree": "*", rest_id: "*"}}),
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
    assert_eq!(body["id"], "kernel_state", "reflect body: {body}");

    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/{rest_id}/_reflect/kernel_state"
        ))
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
        .register("file_bridge.tools", fantastic_file::FileBundle::default());
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
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
               "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": web_id, "port": port}),
        json!({"type": "create_agent", "parent_id": "kernel_state",
               "handler_module": "file_bridge.tools", "allow": ["."], "id": fb_id, "root": "served",
               "ingress_rule": "allow_all_ingress",
               "callable_by": {"$self": "*", "$parent": "*", "$subtree": "*",
                               rest_id: "*", web_id: "*"}}),
        json!({"type": "create_agent", "parent_id": web_id,
               "handler_module": "web_rest.tools", "callable_by": {"$parent": ["get_routes"]}, "id": rest_id,
               "ingress_rule": "allow_all_ingress",
               "can_call": {"kernel_state": "*", "kernel": "*", fb_id: "*"}}),
        json!({"type": "update_agent", "id": "kernel_state", "callable_by": {
               "$self": "*", "$parent": "*", "$subtree": "*", rest_id: "*"}}),
        json!({"type": "update_agent", "id": web_id, "can_call": {
               "$self": "*", "$parent": "*", "$subtree": "*", fb_id: "*"}}),
    ] {
        let reply = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), item)])
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

/// The readme is python's file, byte for byte.
#[test]
fn readme_is_python_verbatim() {
    assert_eq!(
        README,
        include_str!("../../../../../python/bundled_agents/web/host/src/web/readme.md")
    );
}

/// The file proxy types through CPython's `mimetypes` table, with
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

/// Every server→client text frame is python's
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

/// Uvicorn's keepalive contract, pinned at the source (a live twin
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

/// The route table is python's baked routes (no `/`). The rust-only
/// extras are gone: no `/<id>/` server-side render, no `/transport.js`, no
/// `/_assets/*` vendored JS, no `/favicon.ico`.
#[tokio::test]
async fn render_route_is_absent() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_d11", "wr_d11", "fb_d11").await;
    let client = reqwest::Client::new();
    for gone in [
        "/",
        "/web_d11/",
        "/transport.js",
        "/favicon.ico",
        "/favicon.png",
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
    // The baked routes that remain DO serve — `/` is not one of them: the host
    // renders nothing of its own.
    for kept in ["/_assets/favicon.png"] {
        let resp = client
            .get(format!("http://127.0.0.1:{port}{kept}"))
            .send()
            .await
            .expect("get");
        assert_eq!(resp.status().as_u16(), 200, "{kept} must serve");
    }
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

/// An unmatched path is FastAPI's `{"detail":"Not Found"}`, a
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
            .request(
                method.clone(),
                format!("http://127.0.0.1:{port}/rest/kernel_state"),
            )
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
            format!("http://127.0.0.1:{port}/rest/_reflect/kernel_state"),
        )
        .send()
        .await
        .expect("head");
    assert_eq!(resp.status().as_u16(), 405);
    assert_eq!(resp.headers()["allow"], "GET");

    // `/rest/` toggles to `/rest`, which matches nothing → 404.
    let resp = client
        .get(format!("http://127.0.0.1:{port}/rest/"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);
    assert_eq!(resp.text().await.unwrap(), r#"{"detail":"Not Found"}"#);
    stop(&kernel, "web_404").await;
}

/// Starlette's ordered walk: `POST /rest/_reflect` hits the POST
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

/// `GET /rest/_reflect/kernel_state/` toggles to a matching route, so python
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
            "http://127.0.0.1:{port}/rest/_reflect/kernel_state/?tree=none"
        ))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 307);
    assert_eq!(
        resp.headers()["location"],
        format!("http://127.0.0.1:{port}/rest/_reflect/kernel_state?tree=none")
    );
    assert_eq!(resp.headers()["content-length"], "0");
    stop(&kernel, "web_307").await;
}

/// No request-body limit: axum's 2 MiB `DefaultBodyLimit` refused a
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

/// The response header BLOCK, in python's order: uvicorn writes `date`
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
            "POST /rest/kernel_state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\ncontent-type: application/json\r\ncontent-length: 22\r\n\r\n{{\"type\":\"list_agents\"}}"
        ),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert_eq!(names, ["date", "server", "content-length", "content-type"]);

    // 405: date, server, allow, content-length, content-type.
    let (status, names) = header_names(
        port,
        &format!("GET /rest/kernel_state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
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

/// The octet leg's header contract.
#[tokio::test]
async fn octet_leg_header_contract_matches_python() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_oct", "rest", "fb_oct").await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/rest/fb_oct");

    // The reply header is `json.dumps(rest)`: `, `/`: ` separators and
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
        r#"{"path": "na\u00efve.txt", "offset": 0, "next_offset": 7, "eof": true, "size": 7, "origin": "fb_oct na\u00efve.txt bytes 0-7 of 7 \u2014 read_stream {path: na\u00efve.txt, offset: 0, length: 7}"}"#
    );
    assert_eq!(resp.headers()["x-fantastic-bytes-field"], "bytes");
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"naive!\n");

    // An EMPTY payload header parses as `{}` (`headers.get(...) or "{}"`),
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
    // is `send: payload.type required`.)
    assert!(body.get("error").is_some(), "body: {body}");

    // Request header values decode byte-for-byte as latin-1, so a raw
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

    // `x-fantastic-bytes-field` names the field the body lands under.
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

/// The GET `_reflect` shortcuts read their flags exactly as python's
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

    // ...and over the wire: `?readme=true` is 200 (never 400 "invalid digit
    // found in string"), and a null reply is a 404.
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

/// The reflect frame is python's, key for key.
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
        obj.keys().map(String::as_str).take(10).collect::<Vec<_>>(),
        vec![
            "id",
            "sentence",
            "port",
            "host",
            "allowed_origins",
            "running",
            "served_agent_count",
            "surfaces",
            "routes",
            "browser_reachable",
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
        WebBundle
            .verb_docs()
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["reflect", "boot", "stop", "mount", "unmount", "fetch_file"]
    );
    assert_eq!(
        WebBundle.verb_docs()["reflect"],
        json!("Identity + uvicorn port + running flag + currently mounted surfaces (process-local; reflect the live serve for truth).")
    );
    stop(&kernel, "web_r").await;
}

/// `boot` answers `{running, surfaces}`, a missing port RAISES, and
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
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": "web_noport"}),
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

/// `mount` / `unmount` are verbs with python's replies, and an
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
        json!({"error": "web: unknown type 'shutdown'", "reason": "unhandled", "verb": "shutdown"})
    );
    assert_eq!(
        call(json!({"type": "render_html"})).await,
        json!({"error": "web: unknown type 'render_html'", "reason": "unhandled", "verb": "render_html"})
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

/// The twin of python `test_mount.py::
/// test_a_reboot_replaces_a_legs_routes_and_unmount_removes_them_all`: `boot` on
/// a running host re-mounts every surface, and a re-mount REPLACES the leg's
/// routes (never a second copy that outlives `unmount`).
#[tokio::test]
async fn a_reboot_replaces_a_legs_routes_and_unmount_removes_them_all() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_rb", "rest_rb", "fb_rb").await;
    let table = SERVERS
        .lock()
        .get(&AgentId::from("web_rb"))
        .map(|h| Arc::clone(&h.table))
        .expect("a running host");
    let paths = |t: &crate::routes::RouteTable| -> Vec<String> {
        t.iter().map(|r| r.path.clone()).collect()
    };
    let first = paths(&*table.read().await);
    assert!(
        first.iter().any(|p| p.starts_with("/rest_rb/")),
        "{first:?}"
    );
    let again = kernel
        .send(vec![Item::new(
            AgentId::from("web_rb"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(again["running"], json!(true), "{again}");
    assert_eq!(paths(&*table.read().await), first, "the re-boot body");
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("web_rb"),
            json!({"type": "unmount", "child_id": "rest_rb"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(out, json!({"unmounted": "rest_rb"}));
    let left: Vec<String> = paths(&*table.read().await)
        .into_iter()
        .filter(|p| p.starts_with("/rest_rb/"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    stop(&kernel, "web_rb").await;
}

/// Routes change at boot and through `mount`/`unmount`, never through
/// a state subscriber watching for `created`/`routes_changed`.
#[tokio::test]
async fn a_child_created_after_boot_mounts_only_through_mount() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = mk_kernel(&tmp, true, false);
    let web_id = "web_hot";
    for item in [json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": web_id, "port": port})]
    {
        let _ = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), item)])
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
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": web_id,
                   "handler_module": "web_ws.tools", "can_call": {"$parent": ["watch"]}, "callable_by": {"$parent": ["get_routes"]}, "id": "ws_hot",
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

/// Boot ANNOUNCES the URL by sending `say` to `cli`, and falls back to
/// the stderr line only when that send answered something (no cli ⇒ refusal).
#[tokio::test]
async fn boot_announces_say_to_cli() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    kernel.bundles.register("cli_probe.tools", SayProbe);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
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
               "callable_by": {"$self": "*", "$parent": "*", "$subtree": "*", "web_say": "*"}}),
        json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "id": "web_say", "port": port,
               "can_call": {"$self": "*", "$parent": "*", "$subtree": "*", "cli": "*"}}),
    ] {
        let reply = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), item)])
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

/// The announcement names the host the listener BOUND, not a
/// hardcoded `127.0.0.1`: a host bound elsewhere does not answer there. The
/// twin of python `test_boot_announces_say_to_cli`.
#[tokio::test]
async fn boot_announces_the_bound_host() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    kernel.bundles.register("cli_probe.tools", SayProbe);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
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
               "callable_by": {"$self": "*", "$parent": "*", "$subtree": "*", "web_host": "*"}}),
        json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "id": "web_host", "port": port,
               "host": "localhost",
               "can_call": {"$self": "*", "$parent": "*", "$subtree": "*", "cli": "*"}}),
    ] {
        let reply = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), item)])
            .await
            .remove(0)
            .result;
        assert!(reply.get("error").is_none(), "stage: {reply}");
    }
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("web_host"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    let said = SAID.lock().expect("said").clone();
    assert!(
        said.iter().any(|s| s
            == &format!(
            "listening on http://localhost:{port}/ · rest/ws are children (reflect kernel for ids)"
        )),
        "cli never heard the bound host: {said:?}"
    );
    stop(&kernel, "web_host").await;
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

/// A client Close is ECHOED with its payload: the queued reply flushes before the
/// loop ends, so the client sees the code it sent, never `1006` (abnormal).
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
               "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": web_id, "port": port}),
        json!({"type": "create_agent", "parent_id": web_id,
               "handler_module": "web_ws.tools", "can_call": {"$parent": ["watch"]}, "callable_by": {"$parent": ["get_routes"]}, "id": "ws_close",
               "ingress_rule": "allow_all_ingress"}),
    ] {
        let _ = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), item)])
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
    // Uvicorn's 101 block order, plus `server: uvicorn`.
    // This client offers no extension, so none is answered (the negotiation is
    // pinned in `tests_ws_deflate.rs`). RESIDUAL: python sends capitalised
    // header names; hyper lowercases every name.
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

// ─── binary half — a watcher gets the BODY ──────────────────────────

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
        .send(vec![Item::new(
            AgentId::from(web_id),
            json!({"type": "write_stream", "path": "x.bin"}),
        )
        .with_binary_key("bytes", blob.clone())])
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
    let header = wire_head(&wire);
    assert_eq!(header["type"], "event");
    assert_eq!(header["payload"]["type"], "write_stream");
    assert_eq!(header["payload"]["path"], "x.bin");
    assert_eq!(header["payload"]["bytes"], Value::Null);
    assert_eq!(header["_binary_path"], json!(["payload", "bytes"]));
    let body = fantastic_io_bridge::codec::decode_binary_frame(&wire)
        .expect("a well-formed frame")
        .body;
    assert_eq!(body, blob, "the raw body rides beside the head, not base64");

    stop(&kernel, web_id).await;
}

// ─── The REST legs ride the batch ───────────────────

/// A reply that HOLDS a chunk is octet on the JSON leg too, a 0-byte
/// chunk included. Rust answered `application/json` with the bytes as a JSON
/// string, and gated octet on `!body.is_empty()`, so an empty file came back as
/// JSON and a caller could not tell it from a refusal.
#[tokio::test]
async fn json_leg_read_stream_answers_octet_including_empty_chunk() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_jsonoct", "rest", "fb_jsonoct").await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/rest/fb_jsonoct");

    let resp = client
        .post(&url)
        .json(&json!({"type": "read_stream", "path": "hello.txt"}))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["content-type"], "application/octet-stream");
    assert_eq!(resp.headers()["x-fantastic-bytes-field"], "bytes");
    assert_eq!(
        resp.headers()["x-fantastic-payload"],
        r#"{"path": "hello.txt", "offset": 0, "next_offset": 12, "eof": true, "size": 12, "origin": "fb_jsonoct hello.txt bytes 0-12 of 12 \u2014 read_stream {path: hello.txt, offset: 0, length: 12}"}"#
    );
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"hello world\n");

    // AN EMPTY FILE IS STILL A FILE: octet, content-length 0.
    let resp = client
        .post(&url)
        .json(&json!({"type": "read_stream", "path": "empty.txt"}))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["content-type"], "application/octet-stream");
    assert_eq!(resp.headers()["content-length"], "0");
    assert!(resp.bytes().await.unwrap().is_empty());

    // A REFUSAL stays JSON — it carries no chunk.
    let resp = client
        .post(&url)
        .json(&json!({"type": "read_stream", "path": "nope.txt"}))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["reason"], "not_found", "{body}");

    stop(&kernel, "web_jsonoct").await;
}

/// A JSON `write_stream` naming `bytes` as a JSON string is
/// `missing_arg`, and NOTHING is written: JSON has no bytes type, so the only
/// raw chunk on this leg is the octet body.
#[tokio::test]
async fn json_leg_write_stream_string_bytes_is_missing_arg() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_jsonwr", "rest", "fb_jsonwr").await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/rest/fb_jsonwr");

    let resp = client
        .post(&url)
        .json(&json!({"type": "write_stream", "path": "ws_text.txt", "bytes": "abc"}))
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["reason"], "missing_arg", "{body}");
    assert_eq!(
        body["error"], "write_stream: bytes (one raw chunk) required",
        "{body}"
    );
    assert!(
        !tmp.path().join("served").join("ws_text.txt").exists(),
        "the refused write landed anyway"
    );

    stop(&kernel, "web_jsonwr").await;
}

/// The octet leg dispatches THROUGH THE BATCH, so
/// `X-Fantastic-Execution-Id` NAMES the execution (a repeat REPLAYS instead of
/// re-running, and rebinding the id to another target is refused) and the call
/// leaves a row like any other send — an HTTP caller can express a safe repeat.
#[tokio::test]
async fn octet_leg_names_executions_and_validates_type() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_octid", "rest", "fb_octid").await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/rest/fb_octid");
    let post = |payload: &str, id: &str, body: &'static [u8]| {
        client
            .post(&url)
            .header("content-type", "application/octet-stream")
            .header("x-fantastic-payload", payload.to_string())
            .header("x-fantastic-execution-id", id.to_string())
            .body(body)
            .send()
    };

    let first = post(
        r#"{"type":"write_stream","path":"oe1.bin","truncate":true}"#,
        "oe-1",
        b"one",
    )
    .await
    .expect("post");
    assert_eq!(first.status().as_u16(), 200);
    // THE SAME ID REPLAYS the recorded outcome: the file keeps `one`.
    let again = post(
        r#"{"type":"write_stream","path":"oe1.bin","truncate":true}"#,
        "oe-1",
        b"two",
    )
    .await
    .expect("post");
    assert_eq!(again.status().as_u16(), 200);
    assert_eq!(
        std::fs::read(tmp.path().join("served").join("oe1.bin")).unwrap(),
        b"one",
        "a repeated execution id re-ran the write"
    );

    // The execution's own row exists on the target, attributed to the leg.
    let rows = fantastic_kernel::test_support::rows_about(&kernel, &AgentId::from("fb_octid"));
    assert!(
        rows.iter().any(|r| r["kind"] == json!("call")
            && r["verb"] == json!("write_stream")
            && r["caller"] == json!("rest")),
        "the octet dispatch left no call row: {rows:?}"
    );

    // Rebinding a settled id to ANOTHER target is the kernel's refusal, not a
    // second dispatch.
    let other = client
        .post(format!("http://127.0.0.1:{port}/rest/kernel"))
        .header("content-type", "application/octet-stream")
        .header("x-fantastic-payload", r#"{"type":"reflect"}"#)
        .header("x-fantastic-execution-id", "oe-1")
        .body(b"".as_ref())
        .send()
        .await
        .expect("post");
    let body: Value = other.json().await.unwrap();
    assert!(
        body.get("error").is_some(),
        "an id bound to another target dispatched: {body}"
    );

    stop(&kernel, "web_octid").await;
}

// ─── The file proxy ───────────────────────────────────────────

/// A GATE denial is a 404 carrying EXACTLY
/// `error, reason, side, hint` in that order (a missing key as `null`), while a
/// missing file is a bare 404 with no body at all. Rust served the denied file
/// 200 and answered a missing file with text/plain.
#[tokio::test]
async fn gate_denial_404_carries_the_hint_but_a_missing_file_does_not() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_fp", "rest", "fb_fp").await;
    let client = reqwest::Client::new();

    // A served file streams.
    let ok = client
        .get(format!("http://127.0.0.1:{port}/fb_fp/file/hello.txt"))
        .send()
        .await
        .expect("get");
    assert_eq!(ok.status().as_u16(), 200);
    assert_eq!(ok.headers()["content-length"], "12");
    assert_eq!(ok.bytes().await.unwrap().as_ref(), b"hello world\n");

    // AN EMPTY FILE: 200, NO content-length (python sends the header only when
    // the size is truthy), empty body.
    let empty = client
        .get(format!("http://127.0.0.1:{port}/fb_fp/file/empty.txt"))
        .send()
        .await
        .expect("get");
    assert_eq!(empty.status().as_u16(), 200);
    assert!(
        empty.headers().get("content-length").is_none(),
        "an empty file must stream chunked: {:?}",
        empty.headers()
    );
    assert!(empty.bytes().await.unwrap().is_empty());

    // A MISSING FILE: bare 404, no body, no content-type.
    let missing = client
        .get(format!("http://127.0.0.1:{port}/fb_fp/file/nope.txt"))
        .send()
        .await
        .expect("get");
    assert_eq!(missing.status().as_u16(), 404);
    assert!(missing.headers().get("content-type").is_none());
    assert_eq!(missing.headers()["content-length"], "0");

    // SEAL the leg — now the same request is a 404 that TEACHES.
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": "fb_fp", "ingress_rule": "deny_ingress"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    let denied = client
        .get(format!("http://127.0.0.1:{port}/fb_fp/file/hello.txt"))
        .send()
        .await
        .expect("get");
    assert_eq!(denied.status().as_u16(), 404);
    assert_eq!(denied.headers()["content-type"], "application/json");
    let text = denied.text().await.unwrap();
    let parsed: Value = serde_json::from_str(&text).expect("json body");
    let keys: Vec<&str> = parsed
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["error", "reason", "side", "hint"],
        "exactly four keys, in python's file-proxy order: {text}"
    );
    assert_eq!(parsed["reason"], "unauthorized");
    assert_eq!(
        parsed["side"],
        Value::Null,
        "a sealed io gate names no side"
    );
    assert!(parsed["hint"].is_string(), "{text}");

    stop(&kernel, "web_fp").await;
}

/// Python `test_the_web_serves_only_what_the_list_exposes`: the file proxy
/// serves exactly the bridge's `allow` list — an unlisted file is the same bare
/// 404 as a missing one.
#[tokio::test]
async fn the_web_serves_only_what_the_list_exposes() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_al", "rest", "fb_al").await;
    let served = tmp.path().join("served");
    std::fs::create_dir_all(served.join("docs")).unwrap();
    std::fs::write(served.join("docs/a.pdf"), b"%PDF-a").unwrap();
    std::fs::write(served.join("docs/b.pdf"), b"%PDF-b").unwrap();
    std::fs::write(served.join("secret.txt"), b"private\n").unwrap();
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": "fb_al", "allow": ["docs/a.pdf"]}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    let client = reqwest::Client::new();
    let ok = client
        .get(format!("http://127.0.0.1:{port}/fb_al/file/docs/a.pdf"))
        .send()
        .await
        .expect("get");
    assert_eq!(ok.status().as_u16(), 200);
    assert_eq!(ok.bytes().await.unwrap().as_ref(), b"%PDF-a");
    for path in ["docs/b.pdf", "secret.txt", "missing.txt"] {
        let r = client
            .get(format!("http://127.0.0.1:{port}/fb_al/file/{path}"))
            .send()
            .await
            .expect("get");
        assert_eq!(r.status().as_u16(), 404, "{path}");
    }
    stop(&kernel, "web_al").await;
}

/// An agent that is not there at all is a bare 404, before any dispatch.
#[tokio::test]
async fn file_proxy_404_for_missing_agent() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_fpm", "rest", "fb_fpm").await;
    let resp = reqwest::get(format!("http://127.0.0.1:{port}/no_such_agent/file/x.txt"))
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 404);
    assert!(resp.headers().get("content-type").is_none());
    assert_eq!(resp.headers()["content-length"], "0");
    stop(&kernel, "web_fpm").await;
}

/// The JSON BODY goes through CPython's
/// `json.loads(bytes)`: the encoding is DETECTED from the BOM / null-byte
/// pattern, so UTF-8-with-BOM, UTF-16 and UTF-32 bodies all parse. Rust read
/// UTF-8 only and answered 400 for every one of them. A body that will not
/// decode at all raises out of python's handler as a 500.
///
/// STRICT: what python's `io_bridge.loads` refuses — `NaN`/`Infinity`,
/// lone surrogates, ints outside int64, overflowing floats — is the same 400.
#[tokio::test]
async fn json_body_decodes_like_cpython_json_loads() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_jsonenc", "rest", "fb_jsonenc").await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/rest/fb_jsonenc");
    let doc = r#"{"type":"list","path":""}"#;

    let post = |body: Vec<u8>| {
        client
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
            .send()
    };

    // UTF-8 with a BOM.
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(doc.as_bytes());
    let r = post(bom).await.expect("post");
    assert_eq!(r.status().as_u16(), 200, "a BOM'd body was refused");
    let body: Value = r.json().await.unwrap();
    assert!(
        body.get("error").is_none(),
        "the decoded body did not dispatch: {body}"
    );

    // UTF-16-LE with a BOM, and UTF-16-BE without one (the null-byte pattern).
    let mut u16le: Vec<u8> = vec![0xFF, 0xFE];
    for u in doc.encode_utf16() {
        u16le.extend_from_slice(&u.to_le_bytes());
    }
    let r = post(u16le).await.expect("post");
    assert_eq!(r.status().as_u16(), 200, "a UTF-16-LE body was refused");

    let u16be: Vec<u8> = doc.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
    let r = post(u16be).await.expect("post");
    assert_eq!(r.status().as_u16(), 200, "a UTF-16-BE body was refused");

    // UTF-32-LE without a BOM (`XX 00 00 00`).
    let u32le: Vec<u8> = doc.chars().flat_map(|c| (c as u32).to_le_bytes()).collect();
    let r = post(u32le).await.expect("post");
    assert_eq!(r.status().as_u16(), 200, "a UTF-32-LE body was refused");

    // `ff fe 7b` reads as a truncated UTF-16 body: it will not decode, and a
    // body that will not decode is a malformed body — python's 400, with its
    // text (python catches the `UnicodeDecodeError` beside `JSONDecodeError`).
    let r = post(vec![0xFF, 0xFE, 0x7B]).await.expect("post");
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"],
        "web_rest: request body is not valid JSON"
    );

    // Still-malformed JSON is still a 400 with python's text.
    let r = post(b"{not json".to_vec()).await.expect("post");
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"],
        "web_rest: request body is not valid JSON"
    );

    // STRICT: `io_bridge.loads` refuses what not every runtime can hold
    // — an int outside int64, a number that overflows a double — as the same
    // "not valid JSON" 400.
    for bad in [
        r#"{"type":"list","path":"","n":12345678901234567890}"#,
        r#"{"type":"list","path":"","x":1e400}"#,
    ] {
        let r = post(bad.as_bytes().to_vec()).await.expect("post");
        assert_eq!(r.status().as_u16(), 400, "{bad}");
        assert_eq!(
            r.json::<Value>().await.unwrap()["error"],
            "web_rest: request body is not valid JSON"
        );
    }
    // …and the octet leg's payload header the same.
    let r = client
        .post(&url)
        .header("content-type", "application/octet-stream")
        .header(
            "x-fantastic-payload",
            r#"{"type":"list","n":99999999999999999999}"#,
        )
        .body(b"x".to_vec())
        .send()
        .await
        .expect("post");
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"],
        "web_rest: X-Fantastic-Payload is not valid JSON"
    );

    stop(&kernel, "web_jsonenc").await;
}

/// An open socket is WORK, and work is in `ps`: python spawns the
/// outbound pump as the kernel task `ws_drain` on the HOST, and each `call`
/// frame as `ws_call`. Rust ran the drain as a bare `tokio::spawn`, so a live
/// browser connection was invisible to `ps` and to `cancel`.
#[tokio::test]
async fn open_ws_shows_ws_drain_in_ps() {
    use tokio_tungstenite::tungstenite::Message as TMessage;
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web_id = "web_psrows";
    let kernel = stage_web_with_ws(&tmp, port, web_id).await;
    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let drains = kernel
        .tasks
        .rows()
        .into_iter()
        .filter(|r| r["verb"] == json!("ws_drain") && r["agent"] == json!(web_id))
        .count();
    // One for the connection's own inbox, one for the connect-time auto-watch of
    // the host — each watch has its own reader, so every mirrored event can go out
    // stamped with the `src` it came from.
    assert_eq!(
        drains, 2,
        "an open, auto-watching socket shows two `ws_drain`s on the host"
    );

    // A `call` frame is its own registered task under the HOST, not the leg.
    let frame =
        json!({"type": "call", "target": "kernel", "id": "psr", "payload": {"type": "reflect"}});
    let (t, _) = round_trip(&mut ws, TMessage::Text(frame.to_string())).await;
    assert!(t.iter().any(|f| f["id"] == "psr"), "no reply: {t:?}");

    drop(ws);
    stop(&kernel, web_id).await;
}

/// THE EVENT PRECEDES ITS REPLY. Python's kernel fans a payload into
/// every watcher inbox BEFORE the handler runs, and python's cooperative loop
/// puts that `event` frame on the wire before `_on_call` composes the `reply`.
/// Draining the inbox on a PARALLEL task lets the reply overtake the event. A
/// UI sits on this socket, so the order is the contract. 60 iterations, one
/// socket, because it is a RACE.
#[tokio::test]
async fn the_watch_event_always_precedes_its_reply() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as TMessage;
    let tmp = TempDir::new().expect("tmp");
    let port = free_port();
    let web_id = "web_order";
    let _kernel = stage_web_with_ws(&tmp, port, web_id).await;
    let url = format!("ws://127.0.0.1:{port}/{web_id}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");
    // The connect-time auto-watch already mirrors the HOST's inbox here, so a
    // `call` at the host is both the fanned event and the reply.
    for n in 0..60 {
        let id = format!("ord-{n}");
        ws.send(TMessage::Text(
            json!({"type": "call", "target": web_id,
                   "payload": {"type": "reflect"}, "id": id})
            .to_string(),
        ))
        .await
        .expect("send");
        let mut seen_event = false;
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("frame in time")
                .expect("stream open")
                .expect("frame ok");
            let TMessage::Text(t) = msg else { continue };
            let v: Value = serde_json::from_str(&t).expect("json frame");
            match v["type"].as_str() {
                Some("event") if v["payload"]["type"] == json!("reflect") => seen_event = true,
                Some("reply") if v["id"] == json!(id) => {
                    assert!(
                        seen_event,
                        "iteration {n}: the reply overtook the watch event"
                    );
                    break;
                }
                _ => {}
            }
        }
    }
}

// ── C?: the ws client id is python's, not a counter ────────────────────

/// python `web_ws._proxy._make_client_id` — `f"_ws_{secrets.token_hex(3)}"`.
/// Rust minted `_ws_{n:06x}` off a process-global `AtomicU64`, so every capture
/// read the ASCENDING `_ws_000000`/`_ws_000001` family python never produces.
#[test]
fn ws_client_ids_are_pythons_token_hex_3_and_random() {
    let ids: Vec<String> = (0..64).map(|_| crate::ws::make_client_id()).collect();
    for id in &ids {
        let hex = id.strip_prefix("_ws_").unwrap_or_else(|| panic!("{id}"));
        assert_eq!(hex.len(), 6, "{id}");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{id}"
        );
    }
    let uniq: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(uniq.len(), ids.len(), "client ids repeated: {ids:?}");
    let mut sorted = ids.clone();
    sorted.sort();
    assert_ne!(sorted, ids, "client ids ascend like a counter: {ids:?}");
}

/// python `test_foreign_post_is_403_own_and_originless_pass_get_is_not_gated`.
#[tokio::test]
async fn foreign_post_is_403_own_and_originless_pass_get_is_not_gated() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_host(&tmp, port, "web_og", "rest_og", "fb_og").await;
    let c = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/rest_og/kernel_state");
    let body = json!({"type": "reflect"});
    let r = c
        .post(&url)
        .header("Origin", "http://evil.example")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(r.headers()["content-type"], "application/json");
    assert_eq!(
        r.text().await.unwrap(),
        r#"{"error": "origin 'http://evil.example' is not allowed on this host", "reason": "unauthorized", "hint": "add it to the web record's allowed_origins"}"#
    );
    let own = format!("http://localhost:{port}");
    let r = c
        .post(&url)
        .header("Origin", own)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c.post(&url).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = c
        .get(format!("http://127.0.0.1:{port}/_assets/favicon.png"))
        .header("Origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // Loopback by default, and reflect names it.
    let reflect = kernel
        .send(vec![Item::new(
            AgentId::from("web_og"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(reflect["host"], json!("127.0.0.1"));
    assert_eq!(
        reflect["allowed_origins"],
        json!([
            format!("http://127.0.0.1:{port}"),
            format!("http://[::1]:{port}"),
            format!("http://localhost:{port}")
        ])
    );
    stop(&kernel, "web_og").await;
}

/// python `test_foreign_websocket_upgrade_is_refused_listed_origin_passes` —
/// `allowed_origins` is read per request (no restart).
#[tokio::test]
async fn foreign_websocket_upgrade_is_refused_listed_origin_passes() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Error as WsError;
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let kernel = stage_web_with_ws(&tmp, port, "web_ow").await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": "web_ow",
                   "allowed_origins": ["https://ui.example"]}),
        )])
        .await
        .remove(0)
        .result;
    let url = format!("ws://127.0.0.1:{port}/web_ow/ws");
    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut()
        .insert("origin", "http://evil.example".parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Err(WsError::Http(resp)) => assert_eq!(resp.status(), 403),
        other => panic!("expected a refused upgrade, got {:?}", other.map(|_| ())),
    }
    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut()
        .insert("origin", "https://ui.example".parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .expect("listed origin upgrades");
    stop(&kernel, "web_ow").await;
}

/// Every request whose Host is not the server's own
/// authority is refused, GET and the websocket upgrade included.
#[path = "tests_host_guard.rs"]
mod host_guard;

/// The one watch path (python `web_ws/tests/test_watch_path.py`).
#[path = "tests_watch_path.rs"]
mod watch_path;

/// Literal routes before patterns (python `test_mount.py`).
#[path = "tests_route_order.rs"]
mod route_order;

/// The ws leg as the transport of cross-kernel addressing (python
/// `web_ws/tests/test_peer_calls.py`).
#[path = "tests_peer_calls.rs"]
mod peer_calls;

/// A raw body reaches its handler at the field the wire named (python's
/// `_binary_path` / `X-Fantastic-Bytes-Field`), `paste_image` included.
#[path = "tests_binary_path.rs"]
mod binary_path;

/// The ws leg negotiates and speaks permessage-deflate as uvicorn does.
#[path = "tests_ws_deflate.rs"]
mod ws_deflate;

/// A ws message, fragmented or not, is bounded by the
/// one frame cap.
#[path = "tests_ws_message_cap.rs"]
mod ws_message_cap;

/// `_binary_path` as a list of steps across the web legs (python
/// `io_bridge/tests/test_binary_path_steps.py`, web half).
#[path = "tests_binary_steps.rs"]
mod binary_steps;

/// A credential store on a connected kernel is recorded as `[secret]` by the
/// caller (issue #285), and the reply frame carries the marks.
#[path = "tests_credential_forward.rs"]
mod credential_forward;

/// ANY verb whose reply carries raw bytes answers binary on every leg — no verb
/// list (python `io_bridge/tests/test_any_verb_bytes_reply.py`).
#[path = "tests_any_verb_bytes.rs"]
mod any_verb_bytes;

/// A read_stream SOURCE that is not file_bridge: `chunked` answers two chunks
/// whose first carries NO `eof` key; `broken` answers one chunk with neither
/// `eof` nor `next_offset`.
struct OddSource;

#[async_trait::async_trait]
impl fantastic_kernel::bundle::Bundle for OddSource {
    fn name(&self) -> &str {
        "odd_source"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
        Ok(Some(json!({"error": "text channel"})))
    }
    async fn handle_binary(
        &self,
        id: &AgentId,
        header: Value,
        _body: Option<fantastic_kernel::Blob>,
        _kernel: &Arc<Kernel>,
    ) -> Result<
        (
            fantastic_kernel::bundle::Reply,
            Option<fantastic_kernel::Blob>,
        ),
        fantastic_kernel::bundle::BundleError,
    > {
        let offset = header["offset"].as_u64().unwrap_or(0);
        let chunk = |b: &[u8]| Some(fantastic_kernel::Blob::key("bytes", b.to_vec()));
        Ok(match (id.as_str(), offset) {
            ("chunked", 0) => (
                Some(json!({"bytes": null, "next_offset": 3, "size": 6})),
                chunk(b"abc"),
            ),
            ("chunked", _) => (
                Some(json!({"bytes": null, "eof": true, "size": 6})),
                chunk(b"def"),
            ),
            _ => (Some(json!({"bytes": null, "size": 6})), chunk(b"abc")),
        })
    }
}

/// python's file proxy loops until a chunk's `eof` is TRUTHY: a chunk with no
/// `eof` key reads on at its `next_offset`; one with no `next_offset` either
/// raises KeyError mid-stream, so the body is cut short rather than served as
/// whole.
#[tokio::test]
async fn the_file_proxy_reads_on_until_a_truthy_eof() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    kernel.bundles.register("odd_source.tools", OddSource);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    for item in [
        json!({"type": "create_agent", "parent_id": "kernel", "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]},
               "id": "web_odd", "port": port,
               "can_call": {"$self": "*", "$parent": "*", "$subtree": "*",
                            "chunked": "*", "broken": "*"}}),
        json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "odd_source.tools",
               "id": "chunked", "callable_by": {"web_odd": "*"}}),
        json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "odd_source.tools",
               "id": "broken", "callable_by": {"web_odd": "*"}}),
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
            AgentId::from("web_odd"),
            json!({"type": "boot"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(boot["running"], true, "boot: {boot}");
    wait_until_port_accepts(port).await;

    let ok = reqwest::get(format!("http://127.0.0.1:{port}/chunked/file/x.bin"))
        .await
        .expect("get");
    assert_eq!(ok.status().as_u16(), 200);
    assert_eq!(
        ok.bytes().await.unwrap().as_ref(),
        b"abcdef",
        "the second chunk was never read"
    );

    // Aborted mid-stream: the response never completes (the request or the
    // body read fails, depending on when the abort lands).
    let cut = reqwest::get(format!("http://127.0.0.1:{port}/broken/file/x.bin")).await;
    if let Ok(resp) = cut {
        assert!(
            resp.bytes().await.is_err(),
            "a chunk with no next_offset must abort the body"
        );
    }
    stop(&kernel, "web_odd").await;
}

/// starlette's `query_params.get`: `&` is the ONLY separator and a repeated key
/// answers its LAST value (checked against the project venv, python 3.11).
#[test]
fn reflect_query_is_parsed_like_starlette() {
    assert_eq!(
        crate::routes::reflect_payload("readme=1&readme=index"),
        json!({"type": "reflect", "readme": "index"})
    );
    assert_eq!(
        crate::routes::reflect_payload("readme=1;tree=ids"),
        json!({"type": "reflect", "readme": "1;tree=ids"})
    );
    assert_eq!(
        crate::routes::reflect_payload("tree=none&tree=ids&readme=yes"),
        json!({"type": "reflect", "readme": true, "tree": "ids"})
    );
}

/// `web`'s unknown type is a BARE `{error}` (no code,
/// no verb) — unlike its legs' coded refusal. Compared as text: key order is
/// part of the wire and `Value` equality ignores it.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let kernel = Arc::new(Kernel::new());
    for (payload, want) in [
        (
            json!({"type": "frob"}),
            r#"{"error":"web: unknown type 'frob'","reason":"unhandled","verb":"frob"}"#,
        ),
        (
            json!({"type": "it's"}),
            r#"{"error":"web: unknown type \"it's\"","reason":"unhandled","verb":"it's"}"#,
        ),
        (
            json!({}),
            r#"{"error":"web: unknown type None","reason":"unhandled","verb":null}"#,
        ),
    ] {
        let reply = WebBundle
            .handle(&AgentId::from("web_x"), &payload, &kernel)
            .await
            .expect("handled")
            .expect("reply");
        assert_eq!(serde_json::to_string(&reply).unwrap(), want, "{payload}");
    }
}

/// Per-agent server state. Two `web` agents run two
/// listeners; deleting one releases ITS port (the `on_delete` teardown) while
/// the other keeps serving, and its reflect says it is no longer running.
#[tokio::test]
async fn deleting_one_web_agent_frees_only_its_own_listener() {
    let tmp = TempDir::new().unwrap();
    let (pa, pb) = (free_port(), free_port());
    let kernel = stage_host(&tmp, pa, "web_s4a", "rest_s4a", "fb_s4a").await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": HANDLER_MODULE, "callable_by": {"$subtree": ["watch"]}, "can_call": {"$subtree": ["get_routes"]}, "id": "web_s4b", "port": pb}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    wait_until_port_accepts(pb).await;
    let reflect = |id: &'static str| {
        let kernel = Arc::clone(&kernel);
        async move {
            kernel
                .send(vec![Item::new(
                    AgentId::from(id),
                    json!({"type": "reflect"}),
                )])
                .await
                .remove(0)
                .result
        }
    };
    assert_eq!(reflect("web_s4a").await["running"], json!(true));
    assert_eq!(reflect("web_s4b").await["running"], json!(true));
    let gone = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": "web_s4b"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(gone.get("error").is_none(), "{gone}");
    let mut freed = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", pb))
            .await
            .is_err()
        {
            freed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(freed, "the deleted agent's port still accepts");
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", pa))
            .await
            .is_ok(),
        "the other listener is untouched"
    );
    assert_eq!(reflect("web_s4a").await["running"], json!(true));
    stop(&kernel, "web_s4a").await;
}

// The rest leg's gate runs before its body is read, and
// the body is bounded by the one frame cap.
#[path = "tests_rest_ingress_order.rs"]
mod rest_ingress_order;

/// The listener's address is the record's `bind`, else its `host`, else loopback; the
/// guard's identity (`host`) is not moved by a `bind`.
#[test]
fn the_listener_binds_the_bind_field_and_the_guard_keeps_the_host() {
    let rec = |v: Value| v.as_object().cloned().unwrap();
    assert_eq!(listen_host(&rec(json!({}))), "127.0.0.1");
    assert_eq!(
        listen_host(&rec(json!({"host": "192.168.1.5"}))),
        "192.168.1.5"
    );
    assert_eq!(listen_host(&rec(json!({"bind": "0.0.0.0"}))), "0.0.0.0");
    // a bind beats a host: the publish is the listener, the host stays the guard's identity
    let both = rec(json!({"bind": "0.0.0.0", "host": "192.168.1.5"}));
    assert_eq!(listen_host(&both), "0.0.0.0");
    assert_eq!(origin::effective_host(&both), "192.168.1.5");
    // an empty bind is no bind (python's `rec.get("bind") or ...`)
    assert_eq!(
        listen_host(&rec(json!({"bind": "", "host": "0.0.0.0"}))),
        "0.0.0.0"
    );
}

/// The container's listener (#158 decision 4): a `bind` of `0.0.0.0` keeps the
/// loopback identity, so a browser at `localhost:<port>` is admitted by the Host
/// guard and a LAN name is refused with 403.
#[test]
fn wildcard_bind_guard_admits_loopback_host() {
    let rec = json!({"bind": "0.0.0.0", "port": 8088})
        .as_object()
        .cloned()
        .unwrap();
    let req = |host: &str| {
        Request::builder()
            .uri("/web_x/ws")
            .header("host", host)
            .body(Body::empty())
            .unwrap()
    };
    assert!(origin::guard("web_x", &rec, &req("localhost:8088")).is_none());
    let refused = origin::guard("web_x", &rec, &req("192.168.1.5:8088")).expect("LAN host refused");
    assert_eq!(refused.status(), 403);
}
