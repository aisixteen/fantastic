//! SECRET MARKS CROSS EVERY WEB LEG (owner-approved, 2026-10-02).
//!
//! The twin of python `io_bridge/tests/test_secret_across_bridges.py`, web half:
//! web_ws puts an inbound frame's `secret` on the dispatched item verbatim,
//! web_rest reads it from `X-Fantastic-Secret` (a JSON array of marks — dotted
//! strings and/or arrays of steps), and a
//! `<guid>|<agent>` send from the host puts its item's marks on the call frame
//! to the peer. The bridge legs (ws_bridge / relay_connector) are in
//! `fantastic-bridge`'s `tests_secret.rs`.
//!
//! Two kernels per test over REAL sockets (this crate's server, a tungstenite
//! client or a ws_bridge dialing it), each with a memory store, so every record
//! a kernel keeps is checked.

use super::*;
use fantastic_bridge::transport::{BridgeTransport, Frame};
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::secret::PLACEHOLDER;
use fantastic_kernel::test_support::register_root_loader;
use fantastic_kernel::Outcome;
use futures_util::{SinkExt, StreamExt};
use std::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as TMessage;

const KEY: &str = "sk-cross-0123456789abcdef";
const KEY2: &str = "sk-cross-dotted-key-77aa77aa";
const ISSUED: &str = "tok-cross-issued-fedcba98";

/// Set in the child run of [`nothing_on_the_way_prints_a_value`].
const PROBE_ENV: &str = "FT_WEB_SECRET_PRINT_PROBE";

// ─── the probe the far kernel serves ─────────────────────────────────────────

/// What the vault's handler was handed, in order.
type Got = Arc<Mutex<Vec<Value>>>;

struct Vault(Got);

#[async_trait::async_trait]
impl Bundle for Vault {
    fn name(&self) -> &str {
        "xsecret_probe.tools"
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
                json!({"id": id.0, "sentence": "cross-kernel secret probe", "verbs": {}}),
            ));
        }
        self.0.lock().unwrap().push(payload.clone());
        Ok(Some(match verb {
            "set_key" => {
                json!({"ok": true, "len": payload["key"].as_str().map_or(0, |k| k.chars().count())})
            }
            "issue" => json!({"key": ISSUED, "note": "issued"}),
            "issue_dotted" => json!({"a.b": ISSUED, "a": {"b": "nested-plain"}}),
            other => json!({"error": format!("unknown verb '{other}'")}),
        }))
    }
}

// ─── a kernel keeping every record ───────────────────────────────────────────

struct K {
    k: Arc<Kernel>,
    store: String,
    tap: Arc<Mutex<Vec<Value>>>,
}

/// A unique tag per test — the web listeners and the bridge state are
/// process-wide, keyed by agent id.
fn tag() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("xw{}", N.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Outcome {
    k.send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

async fn op_marked(k: &Arc<Kernel>, target: &str, payload: Value, secret: Value) -> Outcome {
    k.send(vec![
        Item::new(AgentId::from(target), payload).with_secret(secret)
    ])
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

/// python `_kernel`: a root with `guid`, a `ram_store` it flushes into, started,
/// and a tap on its state stream. Every leg this file drives is installable.
async fn kernel(guid: &str, store: &str, got: &Got) -> K {
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
    reg.register(
        fantastic_credential_store::HANDLER_MODULE,
        fantastic_credential_store::test_support::bundle(),
    );
    reg.register("xsecret_probe.tools", Vault(Arc::clone(got)));
    let k = bootstrap(reg, BootstrapOptions::in_memory())
        .expect("boot")
        .kernel;
    patch_root(&k, "guid", json!(guid));
    let made = create(
        &k,
        json!({"handler_module": fantastic_ram_store::HANDLER_MODULE, "id": store,
               "operator": true, "admins": ["kernel"]}),
    )
    .await;
    assert_eq!(made["id"], json!(store), "{made}");
    patch_root(&k, "memory_store_id", json!(store));
    let out = k.start(false).await;
    assert_eq!(out["started"], json!(true), "{out}");
    let tap: Arc<Mutex<Vec<Value>>> = Arc::default();
    let t = Arc::clone(&tap);
    k.add_state_subscriber(Arc::new(move |evt: &Value, body| {
        let mut e = evt.clone();
        if let Some(b) = body {
            e["__body"] = json!(String::from_utf8_lossy(&b.bytes));
        }
        t.lock().unwrap().push(e);
    }));
    K {
        k,
        store: store.to_string(),
        tap,
    }
}

/// `create_agent` under the root (or `parent_id` when the record names one).
async fn create(k: &Arc<Kernel>, rec: Value) -> Value {
    let mut full = json!({"type": "create_agent", "parent_id": root_id(k)});
    for (key, v) in rec.as_object().cloned().unwrap_or_default() {
        full[key] = v;
    }
    op(k, "kernel", full).await.result
}

/// A `web` on a free port with the given child legs, booted (the boot mounts
/// every child) and accepting. Returns the port.
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

async fn vault_on(k: &K, caller: &str) {
    let made = create(
        &k.k,
        json!({"handler_module": "xsecret_probe.tools", "id": "vault",
               "callable_by": {caller: "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!("vault"), "{made}");
}

/// Everything a kernel KEEPS of its calls: the rows (in RAM, or — after a flush
/// — in the store), the ledger projection, every state event, the store.
struct Recs {
    rows: Vec<Value>,
    ledger: Value,
    events: Value,
    store: Value,
}

async fn records(k: &K) -> Recs {
    let mut rows: Vec<Value> = k.k.rows.all().into_iter().map(Value::Object).collect();
    let ledger = op(&k.k, "kernel", json!({"type": "ledger"})).await.result["executions"].clone();
    k.k.flush_executions().await;
    let store = op(&k.k, &k.store, json!({"type": "dump"})).await.result["tables"].clone();
    // the flusher moves rows out of RAM on its own clock: a row is in RAM or in
    // the store, the same row either way
    for r in store["rows"].as_array().cloned().unwrap_or_default() {
        let row = r["row"].clone();
        if !rows.iter().any(|x| x["id"] == row["id"]) {
            rows.push(row);
        }
    }
    let events = Value::Array(k.tap.lock().unwrap().clone());
    Recs {
        rows,
        ledger,
        events,
        store,
    }
}

fn assert_kept_out(who: &str, recs: &Recs, values: &[&str]) {
    for (where_, data) in [
        ("rows", Value::Array(recs.rows.clone())),
        ("ledger", recs.ledger.clone()),
        ("events", recs.events.clone()),
        ("store", recs.store.clone()),
    ] {
        let text = data.to_string();
        for v in values {
            assert!(!text.contains(v), "{who}: {v:?} in {where_}");
        }
    }
}

fn call_row<'a>(recs: &'a Recs, agent: &str, verb: &str) -> &'a Value {
    let rows: Vec<&Value> = recs
        .rows
        .iter()
        .filter(|r| r["kind"] == "call" && r["agent"] == agent && r["verb"] == verb)
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    rows[0]
}

fn settle_of<'a>(recs: &'a Recs, call_id: &Value) -> &'a Value {
    let rows: Vec<&Value> = recs
        .rows
        .iter()
        .filter(|r| r["kind"] == "settle" && &r["parent"] == call_id)
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    rows[0]
}

/// The next `reply` frame on a client socket, within 3 s.
async fn next_reply(ws: &mut TestWs) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("a frame in time")
            .expect("stream open")
            .expect("frame ok");
        if let TMessage::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).expect("json frame");
            if v["type"] == "reply" {
                return v;
            }
        }
    }
}

async fn until(what: &str, pred: impl Fn() -> bool) {
    let end = tokio::time::Instant::now() + Duration::from_secs(3);
    while !pred() {
        assert!(tokio::time::Instant::now() < end, "{what}: not reached");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ─── (a) a bridge dialing web_ws: the production ws topology ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bridge_dialing_web_ws_carries_the_marks() {
    let got: Got = Arc::default();
    let ka = kernel("A-g", "a_mem", &got).await;
    let kb = kernel("B-g", "b_mem", &got).await;
    let t = tag();
    let (b_web, b_ws, a_leg) = (format!("{t}_web"), format!("{t}_ws"), format!("{t}_a"));
    let port = serve(
        &kb.k,
        &b_web,
        &[json!({"handler_module": "web_ws.tools", "id": b_ws,
                 "ingress_rule": "allow_all_ingress", "can_call": {"vault": "*"}})],
    )
    .await;
    vault_on(&kb, &b_ws).await;
    let made = create(
        &ka.k,
        json!({"handler_module": fantastic_bridge::WS_HANDLER_MODULE, "id": a_leg,
               "transport": "ws", "host": "127.0.0.1", "port": port, "peer_id": b_web}),
    )
    .await;
    assert_eq!(made["id"], json!(a_leg), "{made}");
    let booted = op(&ka.k, &a_leg, json!({"type": "boot"})).await.result;
    assert!(
        booted["booted"] == json!(true) || booted["already"] == json!(true),
        "{booted}"
    );
    let out = op_marked(
        &ka.k,
        &a_leg,
        json!({"type": "forward", "target": "vault", "payload": {"type": "issue"}}),
        json!(["result.key"]),
    )
    .await;
    assert_eq!(out.result, json!({"key": ISSUED, "note": "issued"}));
    // the frame web_ws received named the call — the settle keeps its whole
    // result — and carried the mark, so B records the placeholder
    let rb = records(&kb).await;
    let row = call_row(&rb, "vault", "issue");
    assert_eq!(row["secret"], json!(["result.key"]), "{row}");
    assert_eq!(row["named"], json!(true), "{row}");
    assert_eq!(
        settle_of(&rb, &row["id"])["outcome"]["result"],
        json!({"key": PLACEHOLDER, "note": "issued"})
    );
    assert_kept_out("B", &rb, &[ISSUED]);
    assert_kept_out("A", &records(&ka).await, &[ISSUED]);
    stop(&kb.k, &b_web).await;
}

// ─── (c) a malformed mark on a web_ws frame is refused before dispatch ───────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_mark_on_a_web_ws_frame_is_refused_before_dispatch() {
    let got: Got = Arc::default();
    let kb = kernel("B-g", "b_mem", &got).await;
    let t = tag();
    let (b_web, b_ws) = (format!("{t}_web"), format!("{t}_ws"));
    let port = serve(
        &kb.k,
        &b_web,
        &[json!({"handler_module": "web_ws.tools", "id": b_ws,
                 "ingress_rule": "allow_all_ingress", "can_call": {"vault": "*"}})],
    )
    .await;
    vault_on(&kb, &b_ws).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{b_web}/ws"))
        .await
        .expect("ws connect");
    for (n, (secret, why)) in [
        (json!("payload.key"), "must be a non-empty list of paths"),
        (json!([]), "must be a non-empty list of paths"),
        (json!(["payload.nope"]), "names nothing in the payload"),
        (json!(["payload.type"]), "cannot hide payload.type"),
        (json!(["payload"]), "must start with 'payload.'"),
        // arrays of steps
        (json!([[]]), "path [] has no steps"),
        (json!([["payload", true]]), "step True is neither a string"),
        (
            json!([["nope", "key"]]),
            "must start with 'payload' and a step under it",
        ),
        (json!([["payload", "type"]]), "cannot hide payload.type"),
        (
            json!([["payload", "nope"]]),
            "path ['payload', 'nope'] names nothing in the payload",
        ),
        (
            json!(["payload.key", ["payload", "k.x"]]),
            "names nothing in the payload",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let frame = json!({"type": "call", "id": format!("bad-{n}"), "target": "vault",
                           "payload": {"type": "set_key", "key": KEY}, "secret": secret});
        ws.send(TMessage::Text(frame.to_string()))
            .await
            .expect("send");
        let data = next_reply(&mut ws).await["data"].clone();
        assert_eq!(
            (&data["reason"], &data["arg"]),
            (&json!("bad_arg"), &json!("secret")),
            "{secret}: {data}"
        );
        assert!(
            data["error"].as_str().unwrap().contains(why),
            "{secret}: {data}"
        );
    }
    assert!(got.lock().unwrap().is_empty());
    stop(&kb.k, &b_web).await;
}

// ─── (d) web_rest: the X-Fantastic-Secret header ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_rest_carries_the_marks_in_a_header() {
    let got: Got = Arc::default();
    let kb = kernel("B-g", "b_mem", &got).await;
    let t = tag();
    let (b_web, b_rest) = (format!("{t}_web"), format!("{t}_rest"));
    let port = serve(
        &kb.k,
        &b_web,
        &[json!({"handler_module": "web_rest.tools", "id": b_rest,
                 "ingress_rule": "allow_all_ingress", "can_call": {"vault": "*"}})],
    )
    .await;
    vault_on(&kb, &b_rest).await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/{b_rest}/vault");
    let r = client
        .post(&url)
        .json(&json!({"type": "set_key", "key": KEY}))
        .header("X-Fantastic-Secret", json!(["payload.key"]).to_string())
        .send()
        .await
        .expect("post");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"ok": true, "len": KEY.len()})
    );
    let r = client
        .post(&url)
        .json(&json!({"type": "issue"}))
        .header("X-Fantastic-Secret", r#"["result.key"]"#)
        .header("X-Fantastic-Execution-Id", "issue-rest-1")
        .send()
        .await
        .expect("post");
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"key": ISSUED, "note": "issued"})
    );
    // an element may be an ARRAY of steps: the key "a.b", taken literally
    let r = client
        .post(&url)
        .json(&json!({"type": "issue_dotted", "a.b": KEY2, "a": {"b": "nested-plain"}}))
        .header(
            "X-Fantastic-Secret",
            json!([["payload", "a.b"], ["result", "a.b"]]).to_string(),
        )
        .header("X-Fantastic-Execution-Id", "issue-rest-2")
        .send()
        .await
        .expect("post");
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"a.b": ISSUED, "a": {"b": "nested-plain"}})
    );
    // a malformed array is the kernel's own refusal, in its words
    let r = client
        .post(&url)
        .json(&json!({"type": "set_key", "key": KEY}))
        .header("X-Fantastic-Secret", r#"[["payload", "a.b"]]"#)
        .send()
        .await
        .expect("post");
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({
            "error": "send: secret path ['payload', 'a.b'] names nothing in the payload",
            "reason": "bad_arg",
            "arg": "secret",
        })
    );
    // a header that is not JSON is the leg's own 400 — nothing dispatched
    let r = client
        .post(&url)
        .json(&json!({"type": "set_key", "key": KEY}))
        .header("X-Fantastic-Secret", "payload.key")
        .send()
        .await
        .expect("post");
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"error": "web_rest: X-Fantastic-Secret is not valid JSON"})
    );
    // JSON of the wrong shape is the kernel's own refusal
    let r = client
        .post(&url)
        .json(&json!({"type": "set_key", "key": KEY}))
        .header("X-Fantastic-Secret", r#""payload.key""#)
        .send()
        .await
        .expect("post");
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        (&body["reason"], &body["arg"]),
        (&json!("bad_arg"), &json!("secret")),
        "{body}"
    );
    assert_eq!(
        *got.lock().unwrap(),
        vec![
            json!({"type": "set_key", "key": KEY}),
            json!({"type": "issue"}),
            json!({"type": "issue_dotted", "a.b": KEY2, "a": {"b": "nested-plain"}}),
        ]
    );
    let rb = records(&kb).await;
    assert_eq!(
        call_row(&rb, "vault", "set_key")["payload"]["key"],
        json!(PLACEHOLDER)
    );
    let issue = call_row(&rb, "vault", "issue");
    assert_eq!(
        settle_of(&rb, &issue["id"])["outcome"]["result"]["key"],
        json!(PLACEHOLDER)
    );
    let dotted = call_row(&rb, "vault", "issue_dotted");
    assert_eq!(
        dotted["secret"],
        json!([["payload", "a.b"], ["result", "a.b"]]),
        "{dotted}"
    );
    assert_eq!(
        dotted["payload"],
        json!({"type": "issue_dotted", "a.b": PLACEHOLDER, "a": {"b": "nested-plain"}}),
        "{dotted}"
    );
    assert_eq!(
        settle_of(&rb, &dotted["id"])["outcome"]["result"],
        json!({"a.b": PLACEHOLDER, "a": {"b": "nested-plain"}})
    );
    assert_kept_out("B", &rb, &[KEY, KEY2, ISSUED]);
    stop(&kb.k, &b_web).await;
}

// ─── (e) <guid>|creds from another kernel ────────────────────────────────────

/// python `_WsEnd` + `_page_peer`: P's bridge leg's far end is a REAL socket into
/// H's web_ws. The page says `hello`, and from then on H reaches P's agents as
/// `P-g|<agent>`. Every frame H sends is kept in `wire` (the TEST's capture,
/// not anything a kernel keeps).
async fn page_socket(
    port: u16,
    web_id: &str,
    half: Arc<dyn BridgeTransport>,
) -> Arc<Mutex<Vec<Value>>> {
    let wire: Arc<Mutex<Vec<Value>>> = Arc::default();
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web_id}/ws"))
        .await
        .expect("ws connect");
    let (mut sink, mut source) = ws.split();
    sink.send(TMessage::Text(
        json!({"type": "hello", "guid": "P-g"}).to_string(),
    ))
    .await
    .expect("hello");
    let (down, seen) = (Arc::clone(&half), Arc::clone(&wire));
    tokio::spawn(async move {
        while let Some(Ok(msg)) = source.next().await {
            match msg {
                TMessage::Text(t) => {
                    let v: Value = serde_json::from_str(&t).expect("json frame");
                    seen.lock().unwrap().push(v.clone());
                    let _ = down.send_frame(v).await;
                }
                TMessage::Binary(b) => {
                    // the head as it crossed, `_binary_path` and all
                    seen.lock().unwrap().push(wire_head(&b));
                    let f = fantastic_io_bridge::codec::decode_binary_frame(&b).expect("frame");
                    let _ = match f.path {
                        Some(path) => {
                            down.send_binary(f.head, fantastic_kernel::Blob::new(path, f.body))
                                .await
                        }
                        None => down.send_frame(f.head).await,
                    };
                }
                TMessage::Close(_) => break,
                _ => {}
            }
        }
        down.close().await;
    });
    tokio::spawn(async move {
        while let Ok(f) = half.recv_frame().await {
            let m = match f {
                Frame::Text(v) => TMessage::Text(fantastic_io_bridge::codec::encode_frame(&v)),
                Frame::Binary(h, b) => TMessage::Binary(
                    fantastic_io_bridge::codec::encode_binary_frame(&h, &b.path, &b.bytes),
                ),
            };
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });
    wire
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guid_qualified_credential_set_and_get_cross_with_their_marks() {
    let got: Got = Arc::default();
    let kh = kernel("H-g", "h_mem", &got).await;
    let kp = kernel("P-g", "p_mem", &got).await;
    let tmp = TempDir::new().unwrap();
    let creds_file = tmp.path().join("creds.yaml");
    let t = tag();
    let (h_web, h_ws, p_leg) = (format!("{t}_web"), format!("{t}_ws"), format!("{t}_p"));
    let port = serve(
        &kh.k,
        &h_web,
        &[json!({"handler_module": "web_ws.tools", "id": h_ws,
                 "ingress_rule": "allow_all_ingress"})],
    )
    .await;
    let made = create(
        &kp.k,
        json!({"handler_module": fantastic_bridge::WS_HANDLER_MODULE, "id": p_leg,
               "transport": "memory", "peer_id": h_web, "peer_guid": "H-g",
               "ingress_rule": "allow_all_ingress", "can_call": {"p_creds": "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!(p_leg), "{made}");
    let made = create(
        &kp.k,
        json!({"handler_module": fantastic_credential_store::HANDLER_MODULE, "id": "p_creds",
               "file": creds_file.to_string_lossy(),
               "refs": {"openai/main": {"host": "api.openai.com",
                                        "readers": ["H-g|kernel"],
                                        "writers": ["H-g|kernel"]}},
               "callable_by": {"H-g|kernel": "*"}}),
    )
    .await;
    assert_eq!(made["id"], json!("p_creds"), "{made}");
    let half = fantastic_bridge::inject_one(&AgentId::from(p_leg.as_str()));
    let booted = op(&kp.k, &p_leg, json!({"type": "boot"})).await.result;
    assert_eq!(booted["booted"], json!(true), "{booted}");
    let wire = page_socket(port, &h_web, half).await;
    until("P is H's peer", || !kh.k.peer_routes("P-g").is_empty()).await;

    let put = op_marked(
        &kh.k,
        "P-g|p_creds",
        json!({"type": "set", "ref": "openai/main", "fields": {"api_key": KEY}}),
        json!(["payload.fields"]),
    )
    .await;
    assert_eq!(put.status.as_str(), "completed", "{:?}", put.result);
    assert_eq!(
        put.result,
        json!({"ref": "openai/main", "host": "api.openai.com",
               "fields": ["api_key"], "stored": true})
    );
    let fetched = op_marked(
        &kh.k,
        "P-g|p_creds",
        json!({"type": "get", "ref": "openai/main"}),
        json!(["result"]),
    )
    .await;
    assert_eq!(
        fetched.result["fields"],
        json!({"api_key": KEY}),
        "{:?}",
        fetched.result
    );
    // the frames H sent carried the item's marks verbatim
    let marks: Vec<Value> = wire
        .lock()
        .unwrap()
        .iter()
        .filter(|f| f["type"] == "call")
        .map(|f| f["secret"].clone())
        .collect();
    assert_eq!(marks, vec![json!(["payload.fields"]), json!(["result"])]);
    // the value lives in P's backend, and in NO record on either side
    assert!(std::fs::read_to_string(&creds_file).unwrap().contains(KEY));
    let rp = records(&kp).await;
    assert_eq!(
        call_row(&rp, "p_creds", "set")["payload"]["fields"],
        json!(PLACEHOLDER)
    );
    let get_row = call_row(&rp, "p_creds", "get");
    assert_eq!(
        settle_of(&rp, &get_row["id"])["outcome"]["result"],
        json!(PLACEHOLDER)
    );
    assert_kept_out("P", &rp, &[KEY]);
    let rh = records(&kh).await;
    assert_eq!(
        call_row(&rh, "P-g|p_creds", "set")["payload"]["fields"],
        json!(PLACEHOLDER)
    );
    assert_kept_out("H", &rh, &[KEY]);
    // an UNMARKED set from H is refused by P's store, as for a local caller
    let bare = op(
        &kh.k,
        "P-g|p_creds",
        json!({"type": "set", "ref": "openai/main", "fields": {"api_key": "sk-unmarked-x"}}),
    )
    .await;
    assert_eq!(bare.result["arg"], json!("secret"), "{:?}", bare.result);
    stop(&kh.k, &h_web).await;
}

// ─── (f) nothing on the way prints a value ───────────────────────────────────

/// python's capsys/caplog: libtest swallows a test's own output, so this re-runs
/// every test of this module in a child with `--nocapture` and reads what
/// reached the real stdout/stderr.
#[test]
fn nothing_on_the_way_prints_a_value() {
    if std::env::var_os(PROBE_ENV).is_some() {
        return; // the child run: the scenarios themselves are what it runs
    }
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["secret_across::", "--nocapture"])
        .env(PROBE_ENV, "1")
        .output()
        .expect("re-run the scenarios");
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the scenarios failed:\n{printed}");
    assert!(
        printed.contains("a_guid_qualified_credential_set_and_get_cross_with_their_marks"),
        "the child ran nothing:\n{printed}"
    );
    for v in [KEY, KEY2, ISSUED] {
        assert!(!printed.contains(v), "{v:?} printed:\n{printed}");
    }
}
