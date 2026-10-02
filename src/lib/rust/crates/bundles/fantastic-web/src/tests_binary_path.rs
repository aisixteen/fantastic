//! A RAW BODY reaches its handler at the field python puts it under — on every
//! leg. Python's web_ws places a binary frame's body at the header's
//! `_binary_path` (a list of steps: a key as a string, taken literally, a list
//! index as an int), web_rest at `X-Fantastic-Bytes-Field`, and each handler reads
//! ITS OWN key: `exec_bridge.paste_image` reads `data`, `file_bridge.write_stream`
//! reads `bytes`. The kernel's binary channel carries the body WITH that path
//! (`fantastic_kernel::Blob`), so `paste_image` works over the binary channel
//! exactly as it does in python, and a body under a foreign key is not its body.

use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as TMessage;

/// Answers what reached it: the body's path and length, and the header the
/// handler was given (python's payload minus the bytes).
struct Capture;

#[async_trait::async_trait]
impl fantastic_kernel::bundle::Bundle for Capture {
    fn name(&self) -> &str {
        "capture"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
        Ok(Some(json!({"channel": "text", "header": payload})))
    }
    async fn handle_binary(
        &self,
        _id: &AgentId,
        header: Value,
        body: Option<fantastic_kernel::Blob>,
        _kernel: &Arc<Kernel>,
    ) -> Result<
        (
            fantastic_kernel::bundle::Reply,
            Option<fantastic_kernel::Blob>,
        ),
        fantastic_kernel::bundle::BundleError,
    > {
        Ok((
            Some(json!({
                "channel": "binary",
                "path": body.as_ref().map(|b| fantastic_bundle::path::steps_value(&b.path)),
                "len": body.as_ref().map(|b| b.bytes.len()),
                "header": header,
            })),
            None,
        ))
    }
}

/// web `<web>` + its ws and rest legs (`<web>_ws`, `<web>_rest`, open), an
/// exec_bridge `term` and a `cap`ture agent both legs may call. A web id is
/// process-wide (its server is), so each test names its own.
async fn stage(tmp: &TempDir, port: u16, web: &str) -> Arc<Kernel> {
    let ws_leg = format!("{web}_ws");
    let rest_leg = format!("{web}_rest");
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, WebBundle);
    kernel
        .bundles
        .register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    kernel
        .bundles
        .register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    kernel.bundles.register(
        fantastic_exec_bridge::HANDLER_MODULE,
        fantastic_exec_bridge::ExecBridgeBundle,
    );
    kernel.bundles.register("capture.tools", Capture);
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
    let callers = json!({"$self": "*", "$parent": "*", "$children": "*",
                         ws_leg.as_str(): "*", rest_leg.as_str(): "*"});
    for item in [
        json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": HANDLER_MODULE, "id": web, "port": port}),
        json!({"type": "create_agent", "parent_id": web, "handler_module": "web_ws.tools",
               "id": ws_leg, "ingress_rule": "allow_all_ingress",
               "can_call": {"term": "*", "cap": "*"}}),
        json!({"type": "create_agent", "parent_id": web, "handler_module": "web_rest.tools",
               "id": rest_leg, "ingress_rule": "allow_all_ingress",
               "can_call": {"term": "*", "cap": "*"}}),
        json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": fantastic_exec_bridge::HANDLER_MODULE, "id": "term",
               "ingress_rule": "allow_all_ingress", "cwd": tmp.path().to_string_lossy(),
               "allowed": [".*"], "callable_by": callers}),
        json!({"type": "create_agent", "parent_id": "kernel",
               "handler_module": "capture.tools", "id": "cap", "callable_by": callers}),
    ] {
        let r = kernel
            .send(vec![Item::new(AgentId::from("kernel"), item)])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "stage: {r}");
    }
    let boot = kernel
        .send(vec![Item::new(AgentId::from(web), json!({"type": "boot"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(boot["running"], true, "boot: {boot}");
    wait_until_port_accepts(port).await;
    kernel
}

/// A binary `call` frame: `payload` with `null` at `at` (python's head), the
/// body at `_binary_path: ["payload", …at]`.
fn call_frame(id: &str, target: &str, mut payload: Value, at: &[&str], body: &[u8]) -> TMessage {
    let at: Vec<fantastic_kernel::Step> = at.iter().map(|k| (*k).into()).collect();
    fantastic_bundle::path::place(&mut payload, &at, Value::Null).expect("a placeable slot");
    let head = json!({"type": "call", "id": id, "target": target, "payload": payload});
    let mut path = vec![fantastic_kernel::Step::from("payload")];
    path.extend(at);
    TMessage::Binary(fantastic_io_bridge::codec::encode_binary_frame(
        &head, &path, body,
    ))
}

/// The `reply` frame answering `id`, within 5 s.
async fn reply_of(ws: &mut TestWs, id: &str) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame in time")
            .expect("stream open")
            .expect("frame ok");
        if let TMessage::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).expect("json frame");
            if v["type"] == "reply" && v["id"] == id {
                return v["data"].clone();
            }
        }
    }
}

/// `paste_image` over the BINARY channel, from both wire legs: the image is the
/// body at `data` — a WS frame's `_binary_path: ["payload", "data"]`, a REST call's
/// `X-Fantastic-Bytes-Field: data` — and lands in the PTY job's scratch file.
/// A body under `bytes` is not `data`: python's `paste_image` answers
/// `bad_arg data "raw bytes"`, and so does this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_image_reads_its_body_at_data_over_ws_and_rest() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web = "web_bp_paste";
    let kernel = stage(&tmp, port, web).await;
    let started = kernel
        .send(vec![Item::new(
            AgentId::from("term"),
            json!({"type": "start", "command": "/bin/cat", "pty": {"cols": 80, "rows": 24}}),
        )])
        .await
        .remove(0)
        .result;
    let job = started["job_id"].clone();
    assert_eq!(started["pty"], json!(true), "{started}");
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web}/ws"))
        .await
        .expect("ws connect");
    let png: &[u8] = b"\x89PNG\r\n\x1a\n-ws-";

    ws.send(call_frame(
        "p1",
        "term",
        json!({"type": "paste_image", "job_id": job, "mime": "image/png"}),
        &["data"],
        png,
    ))
    .await
    .unwrap();
    let r = reply_of(&mut ws, "p1").await;
    assert_eq!(r["bytes"], json!(png.len()), "{r}");
    let saved = r["path"].as_str().expect("a saved path").to_string();
    assert!(saved.ends_with(".png"), "{r}");
    assert_eq!(std::fs::read(&saved).unwrap(), png, "the body IS the image");

    // the same body under `bytes` is not the image
    ws.send(call_frame(
        "p2",
        "term",
        json!({"type": "paste_image", "job_id": job, "mime": "image/png"}),
        &["bytes"],
        png,
    ))
    .await
    .unwrap();
    let r = reply_of(&mut ws, "p2").await;
    assert_eq!(
        (&r["reason"], &r["arg"], &r["error"]),
        (
            &json!("bad_arg"),
            &json!("data"),
            &json!("paste_image: data must be raw bytes")
        ),
        "{r}"
    );

    // REST: the octet body bound under the NAMED field
    let rest_png: &[u8] = b"\x89PNG\r\n\x1a\n-rest-";
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/{web}_rest/term"))
        .header("content-type", "application/octet-stream")
        .header(
            "x-fantastic-payload",
            json!({"type": "paste_image", "job_id": job, "mime": "image/png"}).to_string(),
        )
        .header("x-fantastic-bytes-field", "data")
        .body(rest_png.to_vec())
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    let r: Value = resp.json().await.expect("json");
    assert_eq!(r["bytes"], json!(rest_png.len()), "{r}");
    assert_eq!(
        std::fs::read(r["path"].as_str().unwrap()).unwrap(),
        rest_png
    );

    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("term"),
            json!({"type": "stop", "job_id": job}),
        )])
        .await;
    stop(&kernel, web).await;
}

/// ANY path under the frame's `payload` — nested included — reaches the handler
/// as the body's path, and the header it gets holds nothing at that slot (python
/// holds the bytes there). REST binds the field literally.
#[tokio::test]
async fn a_body_reaches_its_handler_at_the_path_the_wire_named() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web = "web_bp_path";
    let kernel = stage(&tmp, port, web).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web}/ws"))
        .await
        .expect("ws connect");
    ws.send(call_frame(
        "n1",
        "cap",
        json!({"type": "take", "meta": {"k": 1}}),
        &["meta", "blob"],
        b"abc",
    ))
    .await
    .unwrap();
    assert_eq!(
        reply_of(&mut ws, "n1").await,
        json!({"channel": "binary", "path": ["meta", "blob"], "len": 3,
               "header": {"type": "take", "meta": {"k": 1}}})
    );
    // a LITERAL dotted key is one step: the body lands at the key `a.b`, the
    // nested twin untouched
    ws.send(call_frame(
        "n2",
        "cap",
        json!({"type": "take", "a": {"b": "kept"}}),
        &["a.b"],
        b"xy",
    ))
    .await
    .unwrap();
    assert_eq!(
        reply_of(&mut ws, "n2").await,
        json!({"channel": "binary", "path": ["a.b"], "len": 2,
               "header": {"type": "take", "a": {"b": "kept"}}})
    );
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/{web}_rest/cap"))
        .header("content-type", "application/octet-stream")
        .header("x-fantastic-payload", r#"{"type": "take", "k": 2}"#)
        .header("x-fantastic-bytes-field", "data")
        .body(b"wxyz".to_vec())
        .send()
        .await
        .expect("post");
    let r: Value = resp.json().await.expect("json");
    assert_eq!(
        r,
        json!({"channel": "binary", "path": ["data"], "len": 4,
               "header": {"type": "take", "k": 2}})
    );
    stop(&kernel, web).await;
}

/// The REST octet leg's `X-Fantastic-Bytes-Field` names ONE LITERAL TOP-LEVEL KEY
/// — python's `payload[header or "bytes"] = body` (python
/// `test_the_bytes_field_header_names_a_literal_top_level_key`). `a.b` binds the
/// body to the key `"a.b"`, never a dotted path into the envelope: a nested `a.b`
/// already in X-Fantastic-Payload is left as sent, and an envelope with no `a` is
/// not refused (it was `bad_arg binary` here). The call row records the key where
/// python's dict holds it. An absent header binds `bytes`. (The WS frame's
/// `_binary_path` is a list of steps — the test above; this header names one key.)
#[tokio::test]
async fn the_bytes_field_header_names_a_literal_top_level_key() {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let web = "web_bp_literal";
    let kernel = stage(&tmp, port, web).await;
    let post = |payload: Value, field: Option<&str>| {
        let mut req = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/{web}_rest/cap"))
            .header("content-type", "application/octet-stream")
            .header("x-fantastic-payload", payload.to_string());
        if let Some(f) = field {
            req = req.header("x-fantastic-bytes-field", f);
        }
        async move {
            let resp = req.body(b"raw".to_vec()).send().await.expect("post");
            assert_eq!(resp.status().as_u16(), 200);
            resp.json::<Value>().await.expect("json")
        }
    };
    assert_eq!(
        post(json!({"type": "take", "a": {"b": "kept"}}), Some("a.b")).await,
        json!({"channel": "binary", "path": ["a.b"], "len": 3,
               "header": {"type": "take", "a": {"b": "kept"}}})
    );
    let row = kernel
        .rows
        .all()
        .into_iter()
        .find(|r| r.get("verb") == Some(&json!("take")))
        .expect("the call row");
    assert_eq!(
        serde_json::to_string(&row["payload"]).unwrap(),
        r#"{"type":"take","a":{"b":"kept"},"a.b":{"len":3}}"#,
        "the body sits at the literal key, the nested `a.b` untouched"
    );
    assert_eq!(
        post(json!({"type": "take"}), Some("a.b")).await,
        json!({"channel": "binary", "path": ["a.b"], "len": 3, "header": {"type": "take"}}),
        "no `a` in the envelope is no refusal — the key is literal"
    );
    assert_eq!(
        post(json!({"type": "take"}), None).await,
        json!({"channel": "binary", "path": ["bytes"], "len": 3, "header": {"type": "take"}})
    );
    stop(&kernel, web).await;
}
