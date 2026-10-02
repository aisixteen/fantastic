//! `_binary_path` is a LIST OF STEPS on the web legs (owner-approved,
//! 2026-10-02) — the twins of python `io_bridge/tests/test_binary_path_steps.py`,
//! web half: (b) a ws_bridge forward through web_ws, both ways, and (d) web_rest's
//! literal binding forwarded to a peer kernel as `P-g|echo`, its reply named by
//! `X-Fantastic-Bytes-Field`. The relay forward is in `fantastic-bridge`'s
//! `tests_binary_steps.rs`; the guest wire in `fantastic-exec-bridge`'s.
//!
//! A binary frame's header names where its raw body belongs. It used to spell
//! that as a dotted string, so a payload whose bytes sat at the LITERAL key
//! `"a.b"` (which `X-Fantastic-Bytes-Field: a.b` binds) went out as
//! `"payload.a.b"` and the receiver rebuilt a NESTED `a` → `b` slot. Now the path
//! is a JSON array: a string step is an object key, taken literally; an integer
//! step is a list index.
//!
//! A bridge leg's far end is a REAL socket into web_ws (python's `_WsEnd`), so
//! every frame crosses the io_bridge codec; the test keeps every binary head that
//! crossed, both ways, as it crossed.

use super::*;
use fantastic_bridge::transport::{BridgeTransport, Frame};
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::test_support::register_root_loader;
use fantastic_kernel::{Blob, Outcome, Step};
use futures_util::{SinkExt, StreamExt};
use std::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as TMessage;

const RAW: &[u8] = b"\x00raw\xff";

/// What the probe was handed: the header, and the body's steps and bytes.
type Got = Arc<Mutex<Vec<Value>>>;

/// python `_echo`: keeps what arrived and answers with bytes at the LITERAL key
/// `x.y` beside a nested `x` → `y`, so the reply crosses the same way back.
struct BinEcho(Got);

#[async_trait::async_trait]
impl Bundle for BinEcho {
    fn name(&self) -> &str {
        "binpath_echo.tools"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload["type"].as_str().unwrap_or("");
        if matches!(verb, "boot" | "shutdown" | "reflect") {
            return Ok(Some(
                json!({"id": id.0, "sentence": "binary path probe", "verbs": {}}),
            ));
        }
        Ok(Some(json!({"error": format!("text channel: {verb}")})))
    }
    async fn handle_binary(
        &self,
        _id: &AgentId,
        header: Value,
        body: Option<Blob>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        // python `sorted(payload)` — the header's keys and the body's own key
        let mut keys: Vec<String> = header
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(Step::Key(k)) = body.as_ref().and_then(|b| b.path.first()) {
            keys.push(k.clone());
        }
        keys.sort();
        self.0.lock().unwrap().push(json!({
            "header": header,
            "path": body.as_ref().map(|b| fantastic_bundle::path::steps_value(&b.path)),
            "bytes": body.as_ref().map(|b| b.bytes.clone()),
        }));
        let back = fantastic_kernel::body_at(body.as_ref(), "a.b")
            .unwrap_or_default()
            .to_vec();
        Ok((
            Some(json!({"keys": keys, "x.y": null, "x": {"y": "kept"}})),
            Some(Blob::key("x.y", back)),
        ))
    }
}

/// python `_got == [SENT]`: the body reached the far handler at the literal
/// key, the nested twin as sent.
fn assert_got_sent(got: &Got) {
    assert_eq!(
        *got.lock().unwrap(),
        vec![json!({"header": {"type": "take", "a": {"b": "kept"}},
                    "path": ["a.b"], "bytes": RAW})]
    );
}

// ─── two kernels ─────────────────────────────────────────────────────────────

/// A unique tag per test — the web listeners and the bridge state are
/// process-wide, keyed by agent id.
fn tag() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("bp{}", N.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Outcome {
    k.send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

fn root_id(k: &Arc<Kernel>) -> String {
    k.root().expect("root").id.0.clone()
}

fn patch_root(k: &Arc<Kernel>, key: &str, value: Value) {
    let mut m = Map::new();
    m.insert(key.to_string(), value);
    k.update(&AgentId::from(root_id(k)), m);
}

/// `create_agent` under the root (or `parent_id` when the record names one).
async fn create(k: &Arc<Kernel>, rec: Value) -> Value {
    let mut full = json!({"type": "create_agent", "parent_id": root_id(k)});
    for (key, v) in rec.as_object().cloned().unwrap_or_default() {
        full[key] = v;
    }
    op(k, "kernel", full).await.result
}

/// A booted, started root with `guid`, the memory store it keeps its rows in,
/// every leg this file drives, and the probe.
async fn kernel(guid: &str, got: &Got) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    reg.register(HANDLER_MODULE, WebBundle);
    reg.register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    reg.register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    reg.register(
        fantastic_bridge::WS_HANDLER_MODULE,
        fantastic_bridge::WsBridgeBundle,
    );
    reg.register("binpath_echo.tools", BinEcho(Arc::clone(got)));
    let k = bootstrap(reg, BootstrapOptions::in_memory())
        .expect("boot")
        .kernel;
    patch_root(&k, "guid", json!(guid));
    let made = create(
        &k,
        json!({"handler_module": fantastic_ram_store::HANDLER_MODULE, "id": "memory",
               "operator": true, "admins": ["kernel"]}),
    )
    .await;
    assert_eq!(made["id"], json!("memory"), "{made}");
    patch_root(&k, "memory_store_id", json!("memory"));
    let out = k.start(false).await;
    assert_eq!(out["started"], json!(true), "{out}");
    k
}

/// A `web` on a free port with the given child legs, booted and accepting.
async fn serve(k: &Arc<Kernel>, web_id: &str, legs: &[Value]) -> u16 {
    let port = free_port();
    let made = create(
        k,
        json!({"handler_module": HANDLER_MODULE, "id": web_id, "port": port}),
    )
    .await;
    assert_eq!(made["id"], json!(web_id), "{made}");
    for leg in legs {
        let mut rec = leg.clone();
        rec["parent_id"] = json!(web_id);
        let made = create(k, rec).await;
        assert!(made.get("error").is_none(), "{made}");
    }
    let r = op(k, web_id, json!({"type": "boot"})).await.result;
    assert_eq!(r["running"], true, "boot: {r}");
    wait_until_port_accepts(port).await;
    port
}

async fn until(what: &str, pred: impl Fn() -> bool) {
    let end = tokio::time::Instant::now() + Duration::from_secs(3);
    while !pred() {
        assert!(tokio::time::Instant::now() < end, "{what}: not reached");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// python `_WsEnd`: a bridge leg's memory half whose far end is a REAL socket
/// into `web_id`'s web_ws — every frame crosses the codec. Returns the binary
/// heads that crossed, both ways, in order, `_binary_path` and all (the TEST's
/// capture). `hello` makes the leg's kernel the host's peer under that guid.
async fn ws_end(
    port: u16,
    web_id: &str,
    half: Arc<dyn BridgeTransport>,
    hello: Option<&str>,
) -> Arc<Mutex<Vec<Value>>> {
    let heads: Arc<Mutex<Vec<Value>>> = Arc::default();
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
    let (down, seen) = (Arc::clone(&half), Arc::clone(&heads));
    tokio::spawn(async move {
        while let Some(Ok(msg)) = source.next().await {
            match msg {
                TMessage::Text(t) => {
                    let v: Value = serde_json::from_str(&t).expect("json frame");
                    let _ = down.send_frame(v).await;
                }
                TMessage::Binary(b) => {
                    seen.lock().unwrap().push(wire_head(&b));
                    let f = fantastic_io_bridge::codec::decode_binary_frame(&b).expect("frame");
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
    let seen = Arc::clone(&heads);
    tokio::spawn(async move {
        while let Ok(f) = half.recv_frame().await {
            let m = match f {
                Frame::Text(v) => TMessage::Text(fantastic_io_bridge::codec::encode_frame(&v)),
                Frame::Binary(h, b) => {
                    let wire =
                        fantastic_io_bridge::codec::encode_binary_frame(&h, &b.path, &b.bytes);
                    seen.lock().unwrap().push(wire_head(&wire));
                    TMessage::Binary(wire)
                }
            };
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });
    heads
}

fn binary_paths(heads: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
    heads
        .lock()
        .unwrap()
        .iter()
        .map(|h| h["_binary_path"].clone())
        .collect()
}

// ─── (b) a ws_bridge forward through web_ws ──────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_literal_key_crosses_a_ws_bridge_forward_both_ways() {
    let got: Got = Arc::default();
    let ka = kernel("A-g", &got).await;
    let kb = kernel("B-g", &got).await;
    let t = tag();
    let (b_web, b_ws, a_leg) = (format!("{t}_web"), format!("{t}_ws"), format!("{t}_a"));
    let port = serve(
        &kb,
        &b_web,
        &[json!({"handler_module": "web_ws.tools", "id": b_ws,
                 "ingress_rule": "allow_all_ingress", "can_call": {"echo": "*"}})],
    )
    .await;
    let made = create(
        &kb,
        json!({"handler_module": "binpath_echo.tools", "id": "echo",
               "callable_by": {b_ws.as_str(): "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!("echo"), "{made}");
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
    let heads = ws_end(port, &b_web, half, None).await;

    let out = ka
        .send(vec![Item::new(
            AgentId::from(a_leg.as_str()),
            json!({"type": "forward", "target": "echo",
                   "payload": {"type": "take", "a": {"b": "kept"}, "a.b": null}}),
        )
        .with_binary(
            vec![Step::from("payload"), Step::from("a.b")],
            RAW.to_vec(),
        )])
        .await
        .remove(0);
    assert_got_sent(&got);
    assert_eq!(
        serde_json::to_string(&out.result).unwrap(),
        r#"{"keys":["a","a.b","type"],"x.y":null,"x":{"y":"kept"}}"#
    );
    assert_eq!(out.body, Some(Blob::key("x.y", RAW.to_vec())));
    assert_eq!(
        binary_paths(&heads),
        vec![json!(["payload", "a.b"]), json!(["data", "x.y"])]
    );
    let h = heads.lock().unwrap().clone();
    assert_eq!(
        h[0]["payload"],
        json!({"type": "take", "a": {"b": "kept"}, "a.b": null}),
        "the null sits at the literal key, the nested twin untouched"
    );
    assert_eq!(
        h[1]["data"],
        json!({"keys": ["a", "a.b", "type"], "x.y": null, "x": {"y": "kept"}})
    );
    stop(&kb, &b_web).await;
}

// ─── (d) web_rest's literal binding, forwarded to a peer kernel ──────────────

/// H serves web_rest + web_ws; P dials H's web_ws with a ws_bridge and says
/// `hello`, so H reaches P's agents as `P-g|<agent>`. A REST post binds the body
/// to the literal key `a.b`; H forwards it to `P-g|echo` over the socket; the
/// reply's bytes at `x.y` come back the same way and out as the octet body,
/// `X-Fantastic-Bytes-Field` naming THAT key (python `_first_bytes_field`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_rest_literal_binding_stays_literal_across_a_bridge() {
    let got: Got = Arc::default();
    let kh = kernel("H-g", &got).await;
    let kp = kernel("P-g", &got).await;
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
                   "ingress_rule": "allow_all_ingress", "can_call": {"P-g|echo": "*"}}),
        ],
    )
    .await;
    let made = create(
        &kp,
        json!({"handler_module": fantastic_bridge::WS_HANDLER_MODULE, "id": p_leg,
               "transport": "memory", "peer_id": h_web, "peer_guid": "H-g",
               "ingress_rule": "allow_all_ingress", "can_call": {"echo": "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!(p_leg), "{made}");
    let caller = format!("H-g|{h_rest}");
    let made = create(
        &kp,
        json!({"handler_module": "binpath_echo.tools", "id": "echo",
               "callable_by": {caller.as_str(): "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!("echo"), "{made}");
    let half = fantastic_bridge::inject_one(&AgentId::from(p_leg.as_str()));
    let booted = op(&kp, &p_leg, json!({"type": "boot"})).await.result;
    assert_eq!(booted["booted"], json!(true), "{booted}");
    let heads = ws_end(port, &h_web, half, Some("P-g")).await;
    until("P is H's peer", || kh.peer("P-g").is_some()).await;

    let r = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/{h_rest}/P-g|echo"))
        .header("content-type", "application/octet-stream")
        .header(
            "X-Fantastic-Payload",
            json!({"type": "take", "a": {"b": "kept"}}).to_string(),
        )
        .header("X-Fantastic-Bytes-Field", "a.b")
        .body(RAW.to_vec())
        .send()
        .await
        .expect("post");
    assert_eq!(r.status().as_u16(), 200);
    let field = r.headers()["x-fantastic-bytes-field"]
        .to_str()
        .unwrap()
        .to_string();
    let rest: Value =
        serde_json::from_str(r.headers()["x-fantastic-payload"].to_str().unwrap()).unwrap();
    let body = r.bytes().await.unwrap();
    assert_got_sent(&got);
    assert_eq!(&body[..], RAW);
    assert_eq!(field, "x.y", "the reply's first top-level bytes field");
    assert_eq!(
        rest,
        json!({"keys": ["a", "a.b", "type"], "x": {"y": "kept"}})
    );
    assert_eq!(
        binary_paths(&heads),
        vec![json!(["payload", "a.b"]), json!(["data", "x.y"])]
    );
    stop(&kh, &h_web).await;
}
