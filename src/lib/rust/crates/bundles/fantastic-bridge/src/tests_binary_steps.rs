//! `_binary_path` is a LIST OF STEPS across a bridge (owner-approved,
//! 2026-10-02) — the twins of python `io_bridge/tests/test_binary_path_steps.py`,
//! bridge half: (c) a relay_connector forward through the relay, and a ws_bridge
//! pair over the memory transport. The ws_bridge → web_ws forward and web_rest's
//! literal binding are in `fantastic-web`'s `tests_binary_steps.rs`.
//!
//! A binary frame's header names where its raw body belongs. It used to spell
//! that as a dotted string, so a payload whose bytes sat at the LITERAL key
//! `"a.b"` went out as `"payload.a.b"` and the receiver rebuilt a NESTED `a` → `b`
//! slot. Now the path is a JSON array: a string step is an object key, taken
//! literally; an integer step is a list index. The relay legs dial a fake relay
//! hub over a REAL socket, so every frame crosses the io_bridge codec both ways.

use super::*;
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::BundleRegistry;
use fantastic_kernel::test_support::register_root_loader;
use fantastic_kernel::{Outcome, Step};

const RAW: &[u8] = b"\x00raw\xff";

/// What the probe was handed: the header, and the body's steps and bytes.
type Got = Arc<Mutex<Vec<Value>>>;

/// python `_echo`: keeps what arrived and answers with bytes at the LITERAL key
/// `x.y` beside a nested `x` → `y`, so the reply crosses the same way back.
struct BinEcho(Got);

#[async_trait]
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
        id: &AgentId,
        header: Value,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        // the kernel dispatches EVERY call here — lifecycle verbs are not a take
        if matches!(
            header["type"].as_str(),
            Some("boot" | "shutdown" | "reflect")
        ) {
            return Ok((self.handle(id, &header, kernel).await?, None));
        }
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

/// A unique tag per test — `BRIDGES` and the injection seam are process-wide.
fn tag() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!("bp{}", N.fetch_add(1, Ordering::SeqCst))
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Outcome {
    k.send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

fn root_id(k: &Arc<Kernel>) -> String {
    k.root().expect("root").id.0.clone()
}

/// A booted, started root with `guid`, a `ram_store` it keeps its execution
/// rows in, the bridge bundles and the probe.
async fn kernel(guid: &str, got: &Got) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    reg.register(WS_HANDLER_MODULE, WsBridgeBundle);
    reg.register(RELAY_HANDLER_MODULE, RelayConnectorBundle);
    crate::tests::register_relay_store(&mut reg);
    reg.register("binpath_echo.tools", BinEcho(Arc::clone(got)));
    let k = bootstrap(reg, BootstrapOptions::in_memory())
        .expect("boot")
        .kernel;
    let patch = |key: &str, v: Value| {
        let mut m = Map::new();
        m.insert(key.to_string(), v);
        k.update(&AgentId::from(root_id(&k)), m);
    };
    patch("guid", json!(guid));
    create(
        &k,
        json!({"handler_module": fantastic_ram_store::HANDLER_MODULE, "id": "memory",
               "operator": true, "admins": ["kernel"]}),
    )
    .await;
    patch("memory_store_id", json!("memory"));
    let out = k.start(false).await;
    assert_eq!(out["started"], json!(true), "{out}");
    k
}

async fn create(k: &Arc<Kernel>, rec: Value) {
    let mut full = json!({"type": "create_agent", "parent_id": root_id(k)});
    for (key, v) in rec.as_object().cloned().unwrap_or_default() {
        full[key] = v;
    }
    let id = full["id"].clone();
    let made = op(k, "kernel", full).await;
    assert_eq!(made.result["id"], id, "{:?}", made.result);
}

async fn boot_leg(k: &Arc<Kernel>, id: &str) {
    let r = op(k, id, json!({"type": "boot"})).await.result;
    assert!(
        r["booted"] == json!(true) || r["already"] == json!(true),
        "{r}"
    );
}

/// The forward python's tests send: `SENT` as the inner payload, its bytes at
/// the LITERAL key `a.b` beside the nested `a` → `b`.
async fn forward_sent(k: &Arc<Kernel>, leg: &str) -> Outcome {
    k.send(vec![Item::new(
        AgentId::from(leg),
        json!({"type": "forward", "target": "echo",
               "payload": {"type": "take", "a": {"b": "kept"}, "a.b": null}}),
    )
    .with_binary(
        vec![Step::from("payload"), Step::from("a.b")],
        RAW.to_vec(),
    )])
    .await
    .remove(0)
}

/// python `_got == [SENT]` and `out["result"] == BACK`.
fn assert_crossed_both_ways(got: &Got, out: &Outcome) {
    assert_eq!(
        *got.lock().unwrap(),
        vec![json!({"header": {"type": "take", "a": {"b": "kept"}},
                    "path": ["a.b"], "bytes": RAW})],
        "the body reached the far handler at the literal key, the nested twin as sent"
    );
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(
        serde_json::to_string(&out.result).unwrap(),
        r#"{"keys":["a","a.b","type"],"x.y":null,"x":{"y":"kept"}}"#
    );
    assert_eq!(
        out.body,
        Some(Blob::key("x.y", RAW.to_vec())),
        "the reply's literal key came back as one key"
    );
}

// ─── (c) a relay_connector forward through the relay ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_literal_key_crosses_a_relay_forward_both_ways() {
    let got: Got = Arc::default();
    let ka = kernel("A-g", &got).await;
    let kb = kernel("B-g", &got).await;
    create(
        &kb,
        json!({"handler_module": "binpath_echo.tools", "id": "echo",
               "callable_by": {"A-g|*": "*"}}),
    )
    .await;
    let hub = fake_relay_hub().await;
    let t = tag();
    let (a_id, b_id, ra, rb) = (
        format!("{t}_a"),
        format!("{t}_b"),
        format!("{t}RA"),
        format!("{t}RB"),
    );
    let mut a = json!({"handler_module": RELAY_HANDLER_MODULE, "id": a_id, "transport": "relay",
                       "relay_url": hub, "guid": ra, "partner_guid": rb,
                       "heartbeat": 0, "reconnect": 0});
    let mut b = json!({"handler_module": RELAY_HANDLER_MODULE, "id": b_id, "transport": "relay",
                       "relay_url": hub, "guid": rb, "partner_guid": ra,
                       "heartbeat": 0, "reconnect": 0, "peer_guid": "A-g",
                       "ingress_rule": "allow_all_ingress", "can_call": {"echo": "*"}});
    let sa = crate::tests::relay_password(&ka, &a_id, &hub, "pw-a").await;
    let sb = crate::tests::relay_password(&kb, &b_id, &hub, "pw-b").await;
    crate::tests::with_relay_password(&mut a, &sa);
    crate::tests::with_relay_password(&mut b, &sb);
    create(&ka, a).await;
    create(&kb, b).await;
    boot_leg(&ka, &a_id).await;
    boot_leg(&kb, &b_id).await;
    let out = forward_sent(&ka, &a_id).await;
    assert_crossed_both_ways(&got, &out);
}

async fn fake_relay_hub() -> String {
    super::secret_across::fake_relay().await
}

// ─── a ws_bridge pair: the serving leg's call and reply ──────────────────────

/// The serving leg places the call's body at its literal key and ships the
/// reply's body at `["data", "x.y"]` — over the memory pair the frame is the
/// envelope and the body at its steps, exactly what the codec writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_literal_key_crosses_a_ws_bridge_pair_both_ways() {
    let got: Got = Arc::default();
    let ka = kernel("A-g", &got).await;
    let kb = kernel("B-g", &got).await;
    create(
        &kb,
        json!({"handler_module": "binpath_echo.tools", "id": "echo",
               "callable_by": {"A-g|*": "*"}}),
    )
    .await;
    let t = tag();
    let (a_id, b_id) = (format!("{t}_a"), format!("{t}_b"));
    inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
    create(
        &ka,
        json!({"handler_module": WS_HANDLER_MODULE, "id": a_id,
               "transport": "memory", "peer_id": "B"}),
    )
    .await;
    create(
        &kb,
        json!({"handler_module": WS_HANDLER_MODULE, "id": b_id,
               "transport": "memory", "peer_id": "A", "peer_guid": "A-g",
               "ingress_rule": "allow_all_ingress", "can_call": {"echo": "*"}}),
    )
    .await;
    boot_leg(&ka, &a_id).await;
    boot_leg(&kb, &b_id).await;
    let out = forward_sent(&ka, &a_id).await;
    assert_crossed_both_ways(&got, &out);
}
