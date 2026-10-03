//! ANY verb whose reply carries raw bytes answers binary — the twins of python
//! `io_bridge/tests/test_any_verb_bytes_reply.py`.
//!
//! There is no list of binary verbs. A bundle's own verb — here `events`, the
//! shape of a terminal-output poll — is called by a TEXT call (no bytes in the
//! call); its reply carries a raw body, so every leg answers it binary, decided
//! by the body alone:
//!
//!   (a) web_ws: a BINARY reply frame, the body at its `_binary_path` array; the
//!       same probe answering no bytes is a TEXT frame;
//!   (b) web_rest: an octet reply, the field named by `X-Fantastic-Bytes-Field`,
//!       the rest of the reply on `X-Fantastic-Payload`; no bytes ⇒ JSON;
//!   (c) a ws_bridge `forward` through web_ws: the call crosses as TEXT, the
//!       reply as BINARY, and `forward` returns the bytes;
//!   (d) a peer kernel's bridge ENGINE serving the inbound call (host web_rest →
//!       `P-g|probe` over the host's web_ws): text call out, binary reply back,
//!       octet out of REST;
//!   (e) bytes at a NESTED slot: web_ws carries the full path; web_rest's octet
//!       names ONE top-level field, so a nested body has no REST form and is
//!       refused as a stated transport error (500).

use super::binary_steps::{create, kernel_with, op, serve, tag, until};
use super::*;
use fantastic_bridge::transport::{BridgeTransport, Frame};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{Blob, Step};
use futures_util::{SinkExt, StreamExt};
use std::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as TMessage;

const CHUNK: &[u8] = b"\x1b[31mred\x1b[0m\x00\xff\r\n";

/// The verbs the probe was asked, in order (lifecycle verbs aside).
type Calls = Arc<Mutex<Vec<String>>>;

/// python `_probe`: a bundle's OWN verbs, nothing declared anywhere.
struct Probe(Calls);

#[async_trait::async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "anyverb_bytes_probe.tools"
    }
    /// The JSON half — a reply body is not carried.
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(self
            .handle_binary(id, payload.clone(), None, kernel)
            .await?
            .0)
    }
    async fn handle_binary(
        &self,
        id: &AgentId,
        header: Value,
        _body: Option<Blob>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        let verb = header["type"].as_str().unwrap_or("").to_string();
        match verb.as_str() {
            "boot" | "shutdown" => return Ok((None, None)),
            "reflect" => {
                return Ok((
                    Some(json!({"id": id.0, "sentence": "bytes-reply probe", "verbs": {}})),
                    None,
                ))
            }
            _ => {}
        }
        self.0.lock().unwrap().push(verb.clone());
        let chunk = CHUNK.to_vec();
        Ok(match verb.as_str() {
            "events" => (
                Some(json!({"seq": 3, "chunk": null})),
                Some(Blob::key("chunk", chunk)),
            ),
            "idle" => (Some(json!({"seq": 3})), None),
            "nested" => (
                Some(json!({"seq": 3, "out": {"chunk": null}})),
                Some(Blob::new(
                    vec![Step::from("out"), Step::from("chunk")],
                    chunk,
                )),
            ),
            "listed" => (
                Some(json!({"events": [{"n": 0}, {"n": 1, "bytes": null}]})),
                Some(Blob::new(
                    vec![
                        Step::from("events"),
                        Step::from(1usize),
                        Step::from("bytes"),
                    ],
                    chunk,
                )),
            ),
            other => (
                Some(
                    json!({"error": format!("probe: no verb {}", fantastic_bundle::py_repr(other))}),
                ),
                None,
            ),
        })
    }
}

const PROBE: &str = "anyverb_bytes_probe.tools";

fn asked(c: &Calls) -> Vec<String> {
    c.lock().unwrap().clone()
}

/// A binary frame → (its header text as it crossed, its raw body).
fn split(wire: &[u8]) -> (String, Vec<u8>) {
    let h = u32::from_be_bytes([wire[0], wire[1], wire[2], wire[3]]) as usize;
    (
        String::from_utf8(wire[4..4 + h].to_vec()).expect("utf-8 header"),
        wire[4 + h..].to_vec(),
    )
}

// ─── (a) web_ws, (b) web_rest, (e) nested — one kernel ──────────────────────

/// One kernel serving web + its ws and rest legs, both granted the probe.
struct One {
    kernel: Arc<Kernel>,
    web: String,
    rest: String,
    port: u16,
    calls: Calls,
}

async fn one() -> One {
    let calls: Calls = Arc::default();
    let kernel = kernel_with("K-g", PROBE, Probe(Arc::clone(&calls))).await;
    let t = tag();
    let (web, ws, rest) = (format!("{t}_web"), format!("{t}_ws"), format!("{t}_rest"));
    let port = serve(
        &kernel,
        &web,
        &[
            json!({"handler_module": "web_ws.tools", "id": ws,
                   "ingress_rule": "allow_all_ingress", "can_call": {"probe": "*"}}),
            json!({"handler_module": "web_rest.tools", "id": rest,
                   "ingress_rule": "allow_all_ingress", "can_call": {"probe": "*"}}),
        ],
    )
    .await;
    let made = create(
        &kernel,
        json!({"handler_module": PROBE, "id": "probe",
               "callable_by": {ws.as_str(): "*", rest.as_str(): "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!("probe"), "{made}");
    One {
        kernel,
        web,
        rest,
        port,
        calls,
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Send a TEXT call frame; return the raw reply message for `fid` (an
/// auto-watch `event` may precede it).
async fn ws_call(ws: &mut Ws, fid: &str, payload: Value) -> TMessage {
    let frame = json!({"type": "call", "id": fid, "target": "probe", "payload": payload});
    ws.send(TMessage::Text(frame.to_string()))
        .await
        .expect("send");
    for _ in 0..12 {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame in time")
            .expect("open")
            .expect("frame");
        let head: Value = match &msg {
            TMessage::Text(t) => serde_json::from_str(t).expect("json"),
            TMessage::Binary(b) => wire_head(b),
            _ => continue,
        };
        if head["type"] == "reply" && head["id"] == fid {
            return msg;
        }
    }
    panic!("no reply for {fid}");
}

async fn ws_open(o: &One) -> Ws {
    tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/{}/ws", o.port, o.web))
        .await
        .expect("ws connect")
        .0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_web_ws_custom_verb_text_call_gets_a_binary_reply_frame() {
    let o = one().await;
    let mut ws = ws_open(&o).await;
    let TMessage::Binary(wire) = ws_call(&mut ws, "e1", json!({"type": "events"})).await else {
        panic!("a text call to a verb whose reply holds bytes answers a BINARY frame");
    };
    let (head, body) = split(&wire);
    assert_eq!(
        head,
        r#"{"type": "reply", "id": "e1", "data": {"seq": 3, "chunk": null}, "_binary_path": ["data", "chunk"]}"#
    );
    assert_eq!(body, CHUNK);

    // the SAME probe answering no bytes: a plain TEXT frame
    let TMessage::Text(text) = ws_call(&mut ws, "e2", json!({"type": "idle"})).await else {
        panic!("a reply with no bytes is a TEXT frame");
    };
    assert_eq!(text, r#"{"type": "reply", "id": "e2", "data": {"seq": 3}}"#);
    assert_eq!(asked(&o.calls), ["events", "idle"]);
    stop(&o.kernel, &o.web).await;
}

/// (e) over web_ws: the reply's bytes at any depth, the full path on the frame.
async fn ws_carries_bytes_at_depth(verb: &str, head: &str) {
    let o = one().await;
    let mut ws = ws_open(&o).await;
    let TMessage::Binary(wire) = ws_call(&mut ws, "n1", json!({"type": verb})).await else {
        panic!("{verb}: a BINARY frame");
    };
    let (got, body) = split(&wire);
    assert_eq!(got, head, "{verb}");
    assert_eq!(body, CHUNK, "{verb}");
    assert_eq!(asked(&o.calls), [verb]);
    stop(&o.kernel, &o.web).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e_web_ws_carries_bytes_at_a_nested_key() {
    ws_carries_bytes_at_depth(
        "nested",
        r#"{"type": "reply", "id": "n1", "data": {"seq": 3, "out": {"chunk": null}}, "_binary_path": ["data", "out", "chunk"]}"#,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e_web_ws_carries_bytes_inside_a_list() {
    ws_carries_bytes_at_depth(
        "listed",
        r#"{"type": "reply", "id": "n1", "data": {"events": [{"n": 0}, {"n": 1, "bytes": null}]}, "_binary_path": ["data", "events", 1, "bytes"]}"#,
    )
    .await;
}

async fn rest_post(o: &One, payload: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}/{}/probe", o.port, o.rest))
        .json(&payload)
        .send()
        .await
        .expect("post")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b_web_rest_custom_verb_json_call_gets_an_octet_reply() {
    let o = one().await;
    let r = rest_post(&o, json!({"type": "events"})).await;
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
    assert_eq!(r.headers()["x-fantastic-bytes-field"], "chunk");
    assert_eq!(r.headers()["x-fantastic-payload"], r#"{"seq": 3}"#);
    assert_eq!(&r.bytes().await.unwrap()[..], CHUNK);

    // no bytes in the reply ⇒ plain JSON, no octet headers
    let r = rest_post(&o, json!({"type": "idle"})).await;
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-type"], "application/json");
    assert!(r.headers().get("x-fantastic-bytes-field").is_none());
    assert!(r.headers().get("x-fantastic-payload").is_none());
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"seq": 3}));
    assert_eq!(asked(&o.calls), ["events", "idle"]);
    stop(&o.kernel, &o.web).await;
}

/// (e) over web_rest. The octet reply names ONE top-level field
/// (`X-Fantastic-Bytes-Field` is a literal key, never a path), so bytes below
/// the top level cannot ride it, and JSON has no bytes: the leg answers its
/// stated transport refusal — after dispatching the verb once.
async fn rest_has_no_form_for_bytes_below_the_top_level(verb: &str) {
    let o = one().await;
    let r = rest_post(&o, json!({"type": verb})).await;
    assert_eq!(r.status().as_u16(), 500, "{verb}");
    assert_eq!(r.headers()["content-type"], "application/json", "{verb}");
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({
            "error": "web_rest: the reply has no JSON representation on this leg",
            "reason": "transport_error",
            "hint": "this verb answers with in-process objects; call it in-process, not over HTTP",
        }),
        "{verb}"
    );
    assert_eq!(asked(&o.calls), [verb], "dispatched once");
    stop(&o.kernel, &o.web).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e_web_rest_bytes_at_a_nested_key_have_no_rest_form() {
    rest_has_no_form_for_bytes_below_the_top_level("nested").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e_web_rest_bytes_inside_a_list_have_no_rest_form() {
    rest_has_no_form_for_bytes_below_the_top_level("listed").await;
}

// ─── two kernels ─────────────────────────────────────────────────────────────

/// One frame as it crossed between a bridge leg and web_ws: its direction (`in`
/// is toward web_ws), its kind (`text` | `binary`), its JSON head as it crossed
/// (`_binary_path` and all), and the raw body of a binary one.
type Crossed = Arc<Mutex<Vec<(&'static str, &'static str, Value, Vec<u8>)>>>;

/// python `_WsEnd`: a bridge leg's memory half whose far end is a REAL socket
/// into `web_id`'s web_ws — every frame crosses the codec, and every frame is
/// kept as it crossed. `hello` makes the leg's kernel the host's peer.
async fn ws_end(
    port: u16,
    web_id: &str,
    half: Arc<dyn BridgeTransport>,
    hello: Option<&str>,
) -> Crossed {
    let wire: Crossed = Arc::default();
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web_id}/ws"))
        .await
        .expect("ws connect");
    let (mut sink, mut source) = ws.split();
    if let Some(guid) = hello {
        sink.send(TMessage::Text(
            json!({"type": "hello", "guid": guid}).to_string(),
        ))
        .await
        .expect("hello");
    }
    // web_ws → the leg
    let (down, seen) = (Arc::clone(&half), Arc::clone(&wire));
    tokio::spawn(async move {
        while let Some(Ok(msg)) = source.next().await {
            match msg {
                TMessage::Text(t) => {
                    let v: Value = serde_json::from_str(&t).expect("json frame");
                    seen.lock()
                        .unwrap()
                        .push(("out", "text", v.clone(), Vec::new()));
                    let _ = down.send_frame(v).await;
                }
                TMessage::Binary(b) => {
                    let f = fantastic_io_bridge::codec::decode_binary_frame(&b).expect("frame");
                    seen.lock()
                        .unwrap()
                        .push(("out", "binary", wire_head(&b), f.body.clone()));
                    let _ = match f.path {
                        Some(path) => down.send_binary(f.head, Blob::new(path, f.body)).await,
                        None => down.send_frame(f.head).await,
                    };
                }
                TMessage::Close(_) => break,
                _ => {}
            }
        }
        down.close().await;
    });
    // the leg → web_ws
    let seen = Arc::clone(&wire);
    tokio::spawn(async move {
        while let Ok(f) = half.recv_frame().await {
            let m = match f {
                Frame::Text(v) => {
                    seen.lock()
                        .unwrap()
                        .push(("in", "text", v.clone(), Vec::new()));
                    TMessage::Text(fantastic_io_bridge::codec::encode_frame(&v))
                }
                Frame::Binary(h, b) => {
                    let wire =
                        fantastic_io_bridge::codec::encode_binary_frame(&h, &b.path, &b.bytes);
                    seen.lock()
                        .unwrap()
                        .push(("in", "binary", wire_head(&wire), b.bytes.clone()));
                    TMessage::Binary(wire)
                }
            };
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });
    wire
}

/// The call/reply frames that crossed (auto-watch events and hellos aside).
fn calls_and_replies(wire: &Crossed) -> Vec<(&'static str, &'static str, Value, Vec<u8>)> {
    wire.lock()
        .unwrap()
        .iter()
        .filter(|(_, _, head, _)| matches!(head["type"].as_str(), Some("call" | "reply")))
        .cloned()
        .collect()
}

/// The reply head both bridge cases expect: the body at `["data", "chunk"]`.
fn assert_binary_reply(head: &Value, body: &[u8], call_id: &Value) {
    assert_eq!(
        serde_json::to_string(head).unwrap(),
        serde_json::to_string(&json!({
            "type": "reply",
            "id": call_id,
            "data": {"seq": 3, "chunk": null},
            "_binary_path": ["data", "chunk"],
        }))
        .unwrap()
    );
    assert_eq!(body, CHUNK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c_a_ws_bridge_forward_text_call_binary_reply() {
    let calls: Calls = Arc::default();
    let ka = kernel_with("A-g", PROBE, Probe(Arc::clone(&calls))).await;
    let kb = kernel_with("B-g", PROBE, Probe(Arc::clone(&calls))).await;
    let t = tag();
    let (b_web, b_ws, a_leg) = (format!("{t}_web"), format!("{t}_ws"), format!("{t}_a"));
    let port = serve(
        &kb,
        &b_web,
        &[json!({"handler_module": "web_ws.tools", "id": b_ws,
                 "ingress_rule": "allow_all_ingress", "can_call": {"probe": "*"}})],
    )
    .await;
    let made = create(
        &kb,
        json!({"handler_module": PROBE, "id": "probe", "callable_by": {b_ws.as_str(): "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!("probe"), "{made}");
    let made = create(
        &ka,
        json!({"handler_module": fantastic_bridge::WS_HANDLER_MODULE, "id": a_leg,
               "transport": "memory", "peer_id": b_web}),
    )
    .await;
    assert_eq!(made["id"], json!(a_leg), "{made}");
    let half = fantastic_bridge::inject_one(&AgentId::from(a_leg.as_str()));
    let booted = op(&ka, &a_leg, json!({"type": "boot"})).await.result;
    assert_eq!(booted["booted"], json!(true), "{booted}");
    let wire = ws_end(port, &b_web, half, None).await;

    // a PLAIN item: no body on the call, nothing declared
    let out = op(
        &ka,
        &a_leg,
        json!({"type": "forward", "target": "probe", "payload": {"type": "events"}}),
    )
    .await;
    assert_eq!(
        out.result,
        json!({"seq": 3, "chunk": null}),
        "{:?}",
        out.result
    );
    assert_eq!(out.body, Some(Blob::key("chunk", CHUNK.to_vec())));
    let crossed = calls_and_replies(&wire);
    assert_eq!(
        crossed
            .iter()
            .map(|(d, k, _, _)| (*d, *k))
            .collect::<Vec<_>>(),
        [("in", "text"), ("out", "binary")]
    );
    let call = &crossed[0].2;
    assert_eq!(call["target"], "probe");
    assert_eq!(call["payload"], json!({"type": "events"}));
    assert_binary_reply(&crossed[1].2, &crossed[1].3, &call["id"]);
    assert_eq!(asked(&calls), ["events"]);
    stop(&kb, &b_web).await;
}

/// H serves web_rest + web_ws; P dials H's web_ws with a ws_bridge and says
/// `hello`. A JSON post to H's REST reaches `P-g|probe`: H's web_ws sends the
/// call as TEXT, P's bridge ENGINE serves it and replies BINARY, H's REST
/// answers octet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d_a_peer_bridge_engine_answers_a_text_call_with_bytes() {
    let calls: Calls = Arc::default();
    let kh = kernel_with("H-g", PROBE, Probe(Arc::clone(&calls))).await;
    let kp = kernel_with("P-g", PROBE, Probe(Arc::clone(&calls))).await;
    let t = tag();
    let (h_web, h_ws, h_rest, p_leg) = (
        format!("{t}_web"),
        format!("{t}_ws"),
        format!("{t}_rest"),
        format!("{t}_p"),
    );
    let port = serve(
        &kh,
        &h_web,
        &[
            json!({"handler_module": "web_ws.tools", "id": h_ws,
                   "ingress_rule": "allow_all_ingress"}),
            json!({"handler_module": "web_rest.tools", "id": h_rest,
                   "ingress_rule": "allow_all_ingress", "can_call": {"P-g|probe": "*"}}),
        ],
    )
    .await;
    let made = create(
        &kp,
        json!({"handler_module": fantastic_bridge::WS_HANDLER_MODULE, "id": p_leg,
               "transport": "memory", "peer_id": h_web, "peer_guid": "H-g",
               "ingress_rule": "allow_all_ingress", "can_call": {"probe": "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!(p_leg), "{made}");
    let caller = format!("H-g|{h_rest}");
    let made = create(
        &kp,
        json!({"handler_module": PROBE, "id": "probe", "callable_by": {caller.as_str(): "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!("probe"), "{made}");
    let half = fantastic_bridge::inject_one(&AgentId::from(p_leg.as_str()));
    let booted = op(&kp, &p_leg, json!({"type": "boot"})).await.result;
    assert_eq!(booted["booted"], json!(true), "{booted}");
    let wire = ws_end(port, &h_web, half, Some("P-g")).await;
    until("P is H's peer", || !kh.peer_routes("P-g").is_empty()).await;

    let r = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/{h_rest}/P-g|probe"))
        .json(&json!({"type": "events"}))
        .send()
        .await
        .expect("post");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
    assert_eq!(r.headers()["x-fantastic-bytes-field"], "chunk");
    assert_eq!(r.headers()["x-fantastic-payload"], r#"{"seq": 3}"#);
    assert_eq!(&r.bytes().await.unwrap()[..], CHUNK);
    let crossed = calls_and_replies(&wire);
    assert_eq!(
        crossed
            .iter()
            .map(|(d, k, _, _)| (*d, *k))
            .collect::<Vec<_>>(),
        [("out", "text"), ("in", "binary")]
    );
    let call = &crossed[0].2;
    assert_eq!(call["target"], "probe");
    assert_eq!(call["payload"], json!({"type": "events"}));
    assert_binary_reply(&crossed[1].2, &crossed[1].3, &call["id"]);
    assert_eq!(asked(&calls), ["events"]);
    stop(&kh, &h_web).await;
}
