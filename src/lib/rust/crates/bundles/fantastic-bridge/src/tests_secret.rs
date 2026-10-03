//! SECRET MARKS CROSS EVERY BRIDGE (owner-approved, 2026-10-02).
//!
//! The twin of python `io_bridge/tests/test_secret_across_bridges.py`, bridge
//! half: the ws_bridge and relay_connector legs riding this engine. The web legs
//! (web_ws inbound, web_rest's header, a `<guid>|creds` peer call) are in
//! `fantastic-web`'s `tests_secret.rs`.
//!
//! A batch item's `secret` paths (`fantastic_kernel::secret`) used to stop at the
//! edge of the kernel that held them: a call frame carried no marks, so the
//! RECEIVING kernel recorded the value verbatim (call/settle rows, ledger, state
//! stream, memory store). Now a bridge `call` frame carries `secret: [marks]`
//! beside `payload`, the serving leg puts it on the dispatched item verbatim, and
//! `forward` re-roots the marks on its own call, each in its own spelling
//! (`payload.payload.<f>` → `payload.<f>`, `["payload", "payload", <s>…]` →
//! `["payload", <s>…]`, a `result` mark as is).
//!
//! A RELAY leg is SEALED for secrets (owner, 2026-10-02): relayd records the
//! frames it routes unmasked, so `relay_connector` refuses any call carrying
//! marks, outbound (`forward`) and inbound (a tunneled `call` frame with
//! `secret`) — section (g).
//!
//! Two kernels per test, each with a memory store, so every record a kernel keeps
//! is checked. The ws legs pair over the memory transport; the relay legs dial a
//! fake relay hub over a REAL socket, so the tunnel wrap/unwrap is the
//! production one.

use super::*;
use crate::tests::{register_relay_store, relay_password, with_relay_password};
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::BundleRegistry;
use fantastic_kernel::secret::PLACEHOLDER;
use fantastic_kernel::test_support::register_root_loader;
use fantastic_kernel::Outcome;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const KEY: &str = "sk-cross-0123456789abcdef";
const KEY2: &str = "sk-cross-dotted-key-77aa77aa";
const ISSUED: &str = "tok-cross-issued-fedcba98";

/// Set in the child run of [`nothing_on_the_way_prints_a_value`].
const PROBE_ENV: &str = "FT_BRIDGE_SECRET_PRINT_PROBE";

// ─── the probe the far kernel serves ─────────────────────────────────────────

/// What the vault's handler was handed, in order.
type Got = Arc<Mutex<Vec<Value>>>;

struct Vault(Got);

#[async_trait]
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

// ─── two kernels, each keeping every record ──────────────────────────────────

/// One kernel: its memory store and every state event it publishes.
struct K {
    k: Arc<Kernel>,
    store: String,
    tap: Arc<Mutex<Vec<Value>>>,
}

/// A unique tag per test — `BRIDGES` and the injection seam are process-wide.
fn tag() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!("xs{}", N.fetch_add(1, Ordering::SeqCst))
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

/// python `_kernel`: a booted root with `guid`, a `ram_store` it flushes into,
/// started, and a tap on its state stream.
async fn kernel(guid: &str, store: &str, got: &Got) -> K {
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    reg.register(WS_HANDLER_MODULE, WsBridgeBundle);
    reg.register(RELAY_HANDLER_MODULE, RelayConnectorBundle);
    register_relay_store(&mut reg);
    reg.register("xsecret_probe.tools", Vault(Arc::clone(got)));
    let k = bootstrap(reg, BootstrapOptions::in_memory())
        .expect("boot")
        .kernel;
    patch_root(&k, "guid", json!(guid));
    let made = op(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": root_id(&k),
               "handler_module": fantastic_ram_store::HANDLER_MODULE, "id": store,
               "operator": true, "admins": ["kernel"]}),
    )
    .await;
    assert_eq!(made.status.as_str(), "completed", "{:?}", made.result);
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

/// Everything a kernel KEEPS of its calls: the rows (those still in RAM, and
/// — after a flush — those the store holds), the ledger projection, every state
/// event, the memory store's tables.
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
    // The flusher moves rows out of RAM on its own clock (150 ms debounce): a
    // row is in RAM or in the store, and it is the same row either way.
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

async fn vault_on(k: &K, caller: &str) {
    let made = op(
        &k.k,
        "kernel",
        json!({"type": "create_agent", "parent_id": root_id(&k.k),
               "handler_module": "xsecret_probe.tools", "id": "vault",
               "callable_by": {caller: "*"}}),
    )
    .await;
    assert_eq!(made.result["id"], json!("vault"), "{:?}", made.result);
}

async fn create_leg(k: &Arc<Kernel>, handler: &str, id: &str, extra: Value) {
    let mut rec = json!({"type": "create_agent", "parent_id": root_id(k),
                         "handler_module": handler, "id": id});
    for (key, v) in extra.as_object().cloned().unwrap_or_default() {
        rec[key] = v;
    }
    let made = op(k, "kernel", rec).await;
    assert_eq!(made.result["id"], json!(id), "{:?}", made.result);
}

/// Up: booted now, or already by `create_agent` (a started kernel boots a new
/// agent; a memory leg injected before it was created came up then).
async fn boot_leg(k: &Arc<Kernel>, id: &str) {
    let r = op(k, id, json!({"type": "boot"})).await.result;
    assert!(
        r["booted"] == json!(true) || r["already"] == json!(true),
        "{r}"
    );
}

/// A FAKE RELAY HUB on a real socket — the relay kernel's routing emulated
/// (python `_testkit_relay.FakeRelayHub`): a peer dials `ws://<hub>/<guid>`, and a
/// `send` to a target guid reaches that guid's socket as a `source`-tagged
/// `event`, the WS frame kind kept.
pub(super) async fn fake_relay() -> String {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    type Peers = Arc<Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<Message>>>>;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let peers: Peers = Arc::default();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let peers = Arc::clone(&peers);
            tokio::spawn(async move {
                let path: Arc<Mutex<String>> = Arc::default();
                let seen = Arc::clone(&path);
                #[allow(clippy::result_large_err)]
                let cb = move |req: &Request, mut resp: Response| {
                    *seen.lock().unwrap() = req.uri().path().trim_start_matches('/').to_string();
                    resp.headers_mut().insert(
                        "Sec-WebSocket-Protocol",
                        HeaderValue::from_static(transport::relay::SUBPROTOCOL),
                    );
                    Ok(resp)
                };
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await else {
                    return;
                };
                let me = path.lock().unwrap().clone();
                let (mut sink, mut source) = ws.split();
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
                peers.lock().unwrap().insert(me.clone(), tx);
                tokio::spawn(async move {
                    while let Some(m) = rx.recv().await {
                        if sink.send(m).await.is_err() {
                            break;
                        }
                    }
                });
                while let Some(Ok(msg)) = source.next().await {
                    let routed = match msg {
                        Message::Text(t) => {
                            let Ok(env) = fantastic_io_bridge::codec::loads(&t) else {
                                continue;
                            };
                            (env["type"] == "send").then(|| {
                                let event = json!({"type": "event", "source": me,
                                                   "payload": env["payload"]});
                                (
                                    env["target"].as_str().unwrap_or("").to_string(),
                                    Message::Text(fantastic_io_bridge::codec::encode_frame(&event)),
                                )
                            })
                        }
                        Message::Binary(b) => {
                            // the delivery nests the frame under the same `payload`
                            // key, so the body's steps carry over unchanged
                            let Ok(f) = fantastic_io_bridge::codec::decode_binary_frame(&b) else {
                                continue;
                            };
                            let Some(path) = f.path else { continue };
                            let env = f.head;
                            (env["type"] == "send").then(|| {
                                let event = json!({"type": "event", "source": me,
                                                   "payload": env["payload"]});
                                (
                                    env["target"].as_str().unwrap_or("").to_string(),
                                    Message::Binary(
                                        fantastic_io_bridge::codec::encode_binary_frame(
                                            &event, &path, &f.body,
                                        ),
                                    ),
                                )
                            })
                        }
                        Message::Close(_) => break,
                        _ => None,
                    };
                    if let Some((target, m)) = routed {
                        let to = peers.lock().unwrap().get(&target).cloned();
                        if let Some(to) = to {
                            let _ = to.send(m);
                        }
                    }
                }
            });
        }
    });
    format!("ws://127.0.0.1:{port}")
}

#[derive(Clone, Copy, Debug)]
enum Leg {
    Ws,
    Relay,
}

/// python `_bridge_pair`: A's leg dials, B's leg SERVES (pinned to A, open,
/// reaching `vault`). Same engine for both bundles; the relay tunnels the frames
/// through the fake hub. Returns (A's leg id, B's leg id).
async fn bridge_pair(ka: &K, kb: &K, leg: Leg) -> (String, String) {
    let t = tag();
    let (a_id, b_id) = (format!("{t}_a"), format!("{t}_b"));
    let serving = json!({"peer_guid": "A-g", "ingress_rule": "allow_all_ingress",
                         "can_call": {"vault": "*"}});
    let (handler, mut extra_a, mut extra_b, hub) = match leg {
        Leg::Ws => {
            inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
            (
                WS_HANDLER_MODULE,
                json!({"transport": "memory", "peer_id": "B"}),
                json!({"transport": "memory", "peer_id": "A"}),
                None,
            )
        }
        Leg::Relay => {
            let hub = fake_relay().await;
            let (ra, rb) = (format!("{t}RA"), format!("{t}RB"));
            (
                RELAY_HANDLER_MODULE,
                json!({"transport": "relay", "relay_url": hub, "guid": ra,
                       "partner_guid": rb, "heartbeat": 0, "reconnect": 0}),
                json!({"transport": "relay", "relay_url": hub, "guid": rb,
                       "partner_guid": ra, "heartbeat": 0, "reconnect": 0}),
                Some(hub),
            )
        }
    };
    for (key, v) in serving.as_object().cloned().unwrap() {
        extra_b[key] = v;
    }
    // a relay leg reads its password from a store before every dial (audit IO-7)
    if let Some(hub) = hub {
        let sa = relay_password(&ka.k, &a_id, &hub, "pw-a").await;
        let sb = relay_password(&kb.k, &b_id, &hub, "pw-b").await;
        with_relay_password(&mut extra_a, &sa);
        with_relay_password(&mut extra_b, &sb);
    }
    create_leg(&ka.k, handler, &a_id, extra_a).await;
    create_leg(&kb.k, handler, &b_id, extra_b).await;
    boot_leg(&ka.k, &a_id).await;
    boot_leg(&kb.k, &b_id).await;
    (a_id, b_id)
}

/// python's `two` fixture.
async fn two() -> (K, K, Got) {
    let got: Got = Arc::default();
    let ka = kernel("A-g", "a_mem", &got).await;
    let kb = kernel("B-g", "b_mem", &got).await;
    (ka, kb, got)
}

fn forward(target: &str, inner: Value) -> Value {
    json!({"type": "forward", "target": target, "payload": inner})
}

// ─── the forward mapping ─────────────────────────────────────────────────────

#[test]
fn forward_re_roots_the_marks_at_the_forwarded_item() {
    let given: Vec<Value> = [
        "payload.payload.key",
        "payload.payload.fields.api_key",
        "result",
        "result.key",
        "payload.target",
        "payload.timeout",
        "payload.payload",
    ]
    .iter()
    .map(|s| json!(s))
    .collect();
    assert_eq!(
        forwarded_secret(&given),
        vec![
            json!("payload.key"),
            json!("payload.fields.api_key"),
            json!("result"),
            json!("result.key"),
            json!("payload")
        ]
    );
    assert!(forwarded_secret(&[]).is_empty());
    // an ARRAY of steps re-roots as an array: one `payload` step dropped
    let arrays = json!([
        ["payload", "payload", "a.b"],
        ["payload", "payload", "items", 0],
        ["result"],
        ["result", "a.b"],
        ["payload", "target"],
        ["payload", "timeout"],
        ["payload", "payload"],
        "payload.payload.key"
    ]);
    assert_eq!(
        Value::Array(forwarded_secret(arrays.as_array().unwrap())),
        json!([
            ["payload", "a.b"],
            ["payload", "items", 0],
            ["result"],
            ["result", "a.b"],
            ["payload"],
            "payload.key"
        ])
    );
}

// ─── (a) a payload mark across a bridge ──────────────────────────────────────

async fn a_marked_payload_crosses(leg: Leg) {
    let (ka, kb, got) = two().await;
    vault_on(&kb, "A-g|*").await;
    let (a_leg, _) = bridge_pair(&ka, &kb, leg).await;
    let out = op_marked(
        &ka.k,
        &a_leg,
        forward(
            "vault",
            json!({"type": "set_key", "ref": "openai", "key": KEY}),
        ),
        json!(["payload.payload.key"]),
    )
    .await;
    // the far TARGET got the value; the CALLER the real reply
    assert_eq!(
        *got.lock().unwrap(),
        vec![json!({"type": "set_key", "ref": "openai", "key": KEY})]
    );
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(out.result, json!({"ok": true, "len": KEY.len()}));
    // the RECEIVING kernel: its call row is masked and names the paths
    let rb = records(&kb).await;
    let row = call_row(&rb, "vault", "set_key");
    assert_eq!(
        row["payload"],
        json!({"type": "set_key", "ref": "openai", "key": PLACEHOLDER})
    );
    assert_eq!(row["secret"], json!(["payload.key"]), "{row}");
    assert_kept_out("B", &rb, &[KEY]);
    // the calling kernel masks its own record of the forward
    let ra = records(&ka).await;
    let fwd = call_row(&ra, &a_leg, "forward");
    assert_eq!(
        fwd["payload"]["payload"]["key"],
        json!(PLACEHOLDER),
        "{fwd}"
    );
    assert_kept_out("A", &ra, &[KEY]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_marked_payload_crosses_a_ws_bridge_and_both_kernels_mask_it() {
    a_marked_payload_crosses(Leg::Ws).await;
}

// ─── (b) a result mark masks the RECEIVING settle row ────────────────────────

async fn a_result_mark_masks_the_receiving_settle(leg: Leg) {
    let (ka, kb, _got) = two().await;
    vault_on(&kb, "A-g|*").await;
    let (a_leg, _) = bridge_pair(&ka, &kb, leg).await;
    let whole = op_marked(
        &ka.k,
        &a_leg,
        forward("vault", json!({"type": "issue"})),
        json!(["result"]),
    )
    .await;
    assert_eq!(whole.result, json!({"key": ISSUED, "note": "issued"}));
    let rb = records(&kb).await;
    let row = call_row(&rb, "vault", "issue");
    assert_eq!(row["secret"], json!(["result"]), "{row}");
    // the frame id NAMES the far execution, so its settle keeps the whole result
    // — the one place the value would have landed
    assert_eq!(
        settle_of(&rb, &row["id"])["outcome"]["result"],
        json!(PLACEHOLDER)
    );
    assert_kept_out("B", &rb, &[ISSUED]);
    assert_kept_out("A", &records(&ka).await, &[ISSUED]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_mark_masks_the_receiving_settle_row_over_a_ws_bridge() {
    a_result_mark_masks_the_receiving_settle(Leg::Ws).await;
}

/// The control: what the marks prevent is real.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_mark_the_receiving_settle_keeps_the_value() {
    let (ka, kb, _got) = two().await;
    vault_on(&kb, "A-g|*").await;
    let (a_leg, _) = bridge_pair(&ka, &kb, Leg::Ws).await;
    op(&ka.k, &a_leg, forward("vault", json!({"type": "issue"}))).await;
    let rb = records(&kb).await;
    let row = call_row(&rb, "vault", "issue");
    assert!(row.get("secret").is_none(), "{row}");
    assert_eq!(
        settle_of(&rb, &row["id"])["outcome"]["result"]["key"],
        json!(ISSUED)
    );
}

// ─── (b2) an ARRAY mark (a literal dotted key) across a bridge ───────────────

/// `["payload", "payload", "a.b"]` on the forward rides as `["payload", "a.b"]`:
/// the far kernel masks the KEY `"a.b"` (not `a` → `b`), and records the marks as
/// the frame carried them — arrays and strings side by side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_array_mark_masks_a_literal_dotted_key_across_a_ws_bridge() {
    let (ka, kb, got) = two().await;
    vault_on(&kb, "A-g|*").await;
    let (a_leg, _) = bridge_pair(&ka, &kb, Leg::Ws).await;
    let sent = json!({"type": "issue_dotted", "key": KEY, "a.b": KEY2,
                      "a": {"b": "nested-plain"}});
    let out = op_marked(
        &ka.k,
        &a_leg,
        forward("vault", sent.clone()),
        json!([
            "payload.payload.key",
            ["payload", "payload", "a.b"],
            ["result", "a.b"]
        ]),
    )
    .await;
    assert_eq!(*got.lock().unwrap(), vec![sent]);
    assert_eq!(
        out.result,
        json!({"a.b": ISSUED, "a": {"b": "nested-plain"}}),
        "{out:?}"
    );
    let rb = records(&kb).await;
    let row = call_row(&rb, "vault", "issue_dotted");
    assert_eq!(
        row["secret"],
        json!(["payload.key", ["payload", "a.b"], ["result", "a.b"]]),
        "{row}"
    );
    assert_eq!(
        row["payload"],
        json!({"type": "issue_dotted", "key": PLACEHOLDER, "a.b": PLACEHOLDER,
               "a": {"b": "nested-plain"}}),
        "{row}"
    );
    // the frame id names the far execution: its settle keeps the whole result
    assert_eq!(
        settle_of(&rb, &row["id"])["outcome"]["result"],
        json!({"a.b": PLACEHOLDER, "a": {"b": "nested-plain"}})
    );
    assert_kept_out("B", &rb, &[KEY, KEY2, ISSUED]);
    let ra = records(&ka).await;
    let fwd = call_row(&ra, &a_leg, "forward");
    assert_eq!(
        fwd["payload"]["payload"]["a.b"],
        json!(PLACEHOLDER),
        "{fwd}"
    );
    assert_eq!(
        fwd["payload"]["payload"]["a"],
        json!({"b": "nested-plain"}),
        "{fwd}"
    );
    assert_kept_out("A", &ra, &[KEY, KEY2, ISSUED]);
}

// ─── (c) a malformed mark is refused before dispatch ─────────────────────────

/// python `_BAD`: a frame's raw `secret` and the words of its refusal.
fn bad_marks() -> Vec<(Value, &'static str)> {
    vec![
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_mark_on_a_bridge_frame_is_refused_before_dispatch() {
    let (_ka, kb, got) = two().await;
    vault_on(&kb, "A-g|*").await;
    let b_leg = format!("{}_b", tag());
    create_leg(
        &kb.k,
        WS_HANDLER_MODULE,
        &b_leg,
        json!({"transport": "memory", "peer_id": "A", "peer_guid": "A-g",
               "ingress_rule": "allow_all_ingress", "can_call": {"vault": "*"}}),
    )
    .await;
    let peer = inject_one(&AgentId::from(b_leg.as_str()));
    boot_leg(&kb.k, &b_leg).await;
    for (n, (secret, why)) in bad_marks().into_iter().enumerate() {
        peer.send_frame(json!({
            "type": "call", "id": format!("bad-{n}"), "target": "vault",
            "payload": {"type": "set_key", "key": KEY},
            "caller": "A-g|kernel", "secret": secret,
        }))
        .await
        .expect("send");
        let frame = tokio::time::timeout(std::time::Duration::from_secs(3), peer.recv_frame())
            .await
            .expect("a reply in time")
            .expect("open")
            .into_value();
        let data = &frame["data"];
        assert_eq!(
            (&data["reason"], &data["arg"]),
            (&json!("bad_arg"), &json!("secret")),
            "{secret}: {data}"
        );
        let err = data["error"].as_str().unwrap();
        assert!(err.contains(why) && !err.contains(KEY), "{secret}: {data}");
    }
    assert!(
        got.lock().unwrap().is_empty(),
        "a refused item must not reach the handler"
    );
    let rb = records(&kb).await;
    assert!(
        !rb.rows
            .iter()
            .any(|r| r["agent"] == "vault" && r["verb"] == "set_key"),
        "{:?}",
        rb.rows
    );
    assert_kept_out("B", &rb, &[KEY]);
}

async fn a_forward_mark_mapping_to_nothing_valid(secret: Value, why: &str) {
    let (ka, kb, got) = two().await;
    vault_on(&kb, "A-g|*").await;
    let (a_leg, _) = bridge_pair(&ka, &kb, Leg::Ws).await;
    let out = op_marked(
        &ka.k,
        &a_leg,
        forward("vault", json!({"type": "set_key", "key": KEY})),
        secret,
    )
    .await;
    let r = &out.result;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("bad_arg"), &json!("secret")),
        "{r}"
    );
    assert!(r["error"].as_str().unwrap().contains(why), "{r}");
    assert!(got.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forward_mark_on_the_whole_inner_payload_is_the_far_kernels_refusal() {
    a_forward_mark_mapping_to_nothing_valid(
        json!(["payload.payload"]),
        "must start with 'payload.'",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forward_mark_on_the_inner_verb_is_the_far_kernels_refusal() {
    a_forward_mark_mapping_to_nothing_valid(
        json!(["payload.payload.type"]),
        "cannot hide payload.type",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_array_forward_mark_on_the_whole_inner_payload_is_the_far_kernels_refusal() {
    a_forward_mark_mapping_to_nothing_valid(
        json!([["payload", "payload"]]),
        "must start with 'payload' and a step under it",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_array_forward_mark_on_the_inner_verb_is_the_far_kernels_refusal() {
    a_forward_mark_mapping_to_nothing_valid(
        json!([["payload", "payload", "type"]]),
        "cannot hide payload.type",
    )
    .await;
}

// ─── (f) no leg, transport or bridge keeps or mirrors the unmasked frame ─────

async fn no_leg_or_bridge_mirrors_the_value(leg: Leg) {
    let (ka, kb, _got) = two().await;
    vault_on(&kb, "A-g|*").await;
    let (a_leg, b_leg) = bridge_pair(&ka, &kb, leg).await;
    // watchers on every agent the call passes: both legs and the far target
    let mut spies = Vec::new();
    for (k, aid) in [
        (&ka.k, a_leg.as_str()),
        (&kb.k, b_leg.as_str()),
        (&kb.k, "vault"),
    ] {
        let spy = AgentId::from(format!("spy_{aid}"));
        let rx = k.ensure_inbox(&spy);
        assert!(k.watch(&AgentId::from(aid), spy).await.is_none());
        spies.push((aid.to_string(), rx));
    }
    let out = op_marked(
        &ka.k,
        &a_leg,
        forward("vault", json!({"type": "set_key", "key": KEY})),
        json!(["payload.payload.key", "result"]),
    )
    .await;
    assert_eq!(out.result, json!({"ok": true, "len": KEY.len()}));
    for (aid, mut rx) in spies {
        let mirrored: Vec<_> = std::iter::from_fn(|| rx.try_recv()).collect();
        for m in &mirrored {
            assert!(!m.payload.to_string().contains(KEY), "{aid}: {m:?}");
            assert!(
                !m.body
                    .as_ref()
                    .is_some_and(|b| String::from_utf8_lossy(&b.bytes).contains(KEY)),
                "{aid}: {m:?}"
            );
        }
    }
    // the bridge state holds no frame once the call settled
    for id in [&a_leg, &b_leg] {
        let st = BRIDGES
            .lock()
            .get(&AgentId::from(id.as_str()))
            .cloned()
            .expect("booted");
        assert_eq!(st.pending_count(), 0, "{id}");
    }
    assert_kept_out("A", &records(&ka).await, &[KEY]);
    assert_kept_out("B", &records(&kb).await, &[KEY]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_ws_leg_or_bridge_mirrors_the_value() {
    no_leg_or_bridge_mirrors_the_value(Leg::Ws).await;
}

// ─── (g) the relay seal ──────────────────────────────────────────────────────

/// python `_SEALED`.
fn sealed() -> Value {
    json!({
        "error": transport::relay::SECRET_SEALED,
        "reason": "unauthorized",
        "hint": transport::relay::SECRET_SEALED_HINT,
    })
}

/// Outbound: ANY mark on the forward call — one that would cross, a result mark,
/// even one on forward's own field — is refused; nothing reaches the far kernel,
/// and the caller's own records stay masked.
async fn the_relay_refuses_a_marked_forward(secret: Value) {
    let (ka, kb, got) = two().await;
    vault_on(&kb, "A-g|*").await;
    let (a_leg, _) = bridge_pair(&ka, &kb, Leg::Relay).await;
    let mut fwd = forward("vault", json!({"type": "set_key", "key": KEY}));
    fwd["timeout"] = json!(5);
    let out = op_marked(&ka.k, &a_leg, fwd, secret.clone()).await;
    assert_eq!(out.status.as_str(), "errored", "{secret}: {:?}", out.result);
    assert_eq!(out.result, sealed(), "{secret}");
    assert!(got.lock().unwrap().is_empty(), "nothing crossed the relay");
    let rb = records(&kb).await;
    assert!(
        !rb.rows
            .iter()
            .any(|r| r["agent"] == "vault" && r["verb"] == "set_key"),
        "{:?}",
        rb.rows
    );
    if secret != json!(["result"]) {
        assert_kept_out("A", &records(&ka).await, &[KEY]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_refuses_a_forward_with_a_payload_mark_before_a_frame_leaves() {
    the_relay_refuses_a_marked_forward(json!(["payload.payload.key"])).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_refuses_a_forward_with_a_result_mark_before_a_frame_leaves() {
    the_relay_refuses_a_marked_forward(json!(["result"])).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_refuses_a_forward_with_a_mark_on_its_own_field_before_a_frame_leaves() {
    the_relay_refuses_a_marked_forward(json!(["payload.timeout"])).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_refuses_a_forward_with_an_array_mark_before_a_frame_leaves() {
    the_relay_refuses_a_marked_forward(json!([["payload", "payload", "key"]])).await;
}

/// Inbound: a tunneled `call` frame that names `secret` is answered with the
/// seal's refusal; the far target never runs and the serving kernel records no
/// call to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_refuses_an_inbound_marked_frame_and_dispatches_nothing() {
    let (ka, kb, got) = two().await;
    vault_on(&kb, "A-g|*").await;
    let hub = fake_relay().await;
    let t = tag();
    let (ra, rb) = (format!("{t}RA"), format!("{t}RB"));
    let b_leg = format!("{t}_b");
    let mut extra = json!({"transport": "relay", "relay_url": hub, "guid": rb,
                           "partner_guid": ra, "heartbeat": 0, "reconnect": 0,
                           "peer_guid": "A-g", "ingress_rule": "allow_all_ingress",
                           "can_call": {"vault": "*"}});
    let store = relay_password(&kb.k, &b_leg, &hub, "pw-b").await;
    with_relay_password(&mut extra, &store);
    create_leg(&kb.k, RELAY_HANDLER_MODULE, &b_leg, extra).await;
    boot_leg(&kb.k, &b_leg).await;
    // the far end: a bare relay transport dialing the same hub as A's guid
    let ta = transport::relay::RelayTransport::connect(
        &hub,
        &ra,
        Arc::new(|| Box::pin(async { Ok(String::new()) })),
        &rb,
        0.0,
        0.0,
        Value::Null,
        transport::relay::RelaySpawn::new(&ka.k, &AgentId::from("ta")),
    )
    .await
    .expect("dial the hub");
    let reply = |ta: Arc<transport::relay::RelayTransport>| async move {
        tokio::time::timeout(std::time::Duration::from_secs(3), ta.recv_frame())
            .await
            .expect("a reply in time")
            .expect("open")
            .into_value()
    };
    ta.send_frame(json!({
        "type": "call", "id": "sealed-1", "target": "vault",
        "payload": {"type": "set_key", "key": KEY},
        "caller": "A-g|kernel", "secret": ["payload.key"],
    }))
    .await
    .expect("send");
    let r = reply(Arc::clone(&ta)).await;
    assert_eq!(
        r,
        json!({"type": "reply", "id": "sealed-1", "data": sealed()}),
        "{r}"
    );
    // the same leg still serves an UNMARKED call — the seal is about marks only
    ta.send_frame(json!({
        "type": "call", "id": "plain-1", "target": "vault",
        "payload": {"type": "issue"}, "caller": "A-g|kernel",
    }))
    .await
    .expect("send");
    let r = reply(Arc::clone(&ta)).await;
    assert_eq!(r["data"], json!({"key": ISSUED, "note": "issued"}), "{r}");
    assert_eq!(*got.lock().unwrap(), vec![json!({"type": "issue"})]);
    let rec = records(&kb).await;
    // (`vault_on` creates through `create_agent`, which boots it — a `boot` row
    // python's `k.create` does not make)
    let verbs: Vec<&Value> = rec
        .rows
        .iter()
        .filter(|r| r["agent"] == "vault" && r["kind"] == "call" && r["verb"] != "boot")
        .map(|r| &r["verb"])
        .collect();
    assert_eq!(verbs, vec![&json!("issue")], "{:?}", rec.rows);
    assert_kept_out("B", &rec, &[KEY]);
    ta.close().await;
}

/// (f) python's capsys/caplog: nothing on the way PRINTS a value. libtest
/// swallows a test's own output, so this re-runs every test of this module in a
/// child with `--nocapture` and reads what reached the real stdout/stderr.
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
        printed.contains("the_relay_refuses_an_inbound_marked_frame_and_dispatches_nothing"),
        "the child ran nothing:\n{printed}"
    );
    for v in [KEY, KEY2, ISSUED] {
        assert!(!printed.contains(v), "{v:?} printed:\n{printed}");
    }
}
