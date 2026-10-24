//! A credential store on a CONNECTED kernel (issue #285): the calling kernel
//! records its `set` fields and `get` reply fields as `[secret]`, over a real
//! socket into the far `web_ws`. The wire half: a reply frame carries `secret`
//! (dotted paths into `data`) only when the reply's bundle named some.

use super::binary_steps::{create, kernel_with, op, serve, tag};
use super::*;
use futures_util::{SinkExt, StreamExt};
use std::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as TMessage;

const VALUE: &str = "sk-FAR-STORE-7c1d9e3a";
const REF: &str = "far/main";

/// A kernel whose probe module is a credential store bundle (in-memory keychain).
async fn kernel(guid: &str) -> Arc<Kernel> {
    kernel_with(
        guid,
        fantastic_credential_store::HANDLER_MODULE,
        fantastic_credential_store::test_support::bundle(),
    )
    .await
}

/// Kernel B: a web with an open web_ws leg `ws`, and a store `creds` holding
/// `far/main` (declared) that the leg may call. Returns `(port, web, ws)`.
async fn serve_store(kb: &Arc<Kernel>) -> (u16, String, String) {
    let t = tag();
    let (web, ws) = (format!("{t}_web"), format!("{t}_ws"));
    let port = serve(
        kb,
        &web,
        &[json!({"handler_module": "web_ws.tools", "callable_by": {"$parent": ["get_routes"]}, "id": ws,
                 "ingress_rule": "allow_all_ingress",
                 "can_call": {"$parent": ["watch"], "creds": "*"}})],
    )
    .await;
    let made = create(
        kb,
        json!({"handler_module": fantastic_credential_store::HANDLER_MODULE, "id": "creds",
               "backend": "keychain", "refs": {REF: {"host": "api.example.com"}},
               "callable_by": {ws.as_str(): ["get", "set", "list"]}}),
    )
    .await;
    assert_eq!(made["id"], json!("creds"), "{made}");
    (port, web, ws)
}

/// One text frame out, the first `reply` with `id` back.
async fn ws_call(port: u16, web: &str, frame: Value) -> Value {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{web}/ws"))
        .await
        .expect("ws connect");
    let (mut sink, mut source) = ws.split();
    let id = frame["id"].clone();
    sink.send(TMessage::Text(frame.to_string()))
        .await
        .expect("send");
    while let Some(Ok(msg)) = source.next().await {
        if let TMessage::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).expect("json frame");
            if v["type"] == "reply" && v["id"] == id {
                return v;
            }
        }
    }
    panic!("no reply");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_reply_frame_carries_the_outcomes_secret_marks() {
    let kb = kernel("B-g").await;
    let (port, web, _ws) = serve_store(&kb).await;
    let set = op(
        &kb,
        "creds",
        json!({"type": "set", "ref": REF, "fields": {"api_key": VALUE}}),
    )
    .await;
    assert_eq!(set.result["stored"], json!(true), "{}", set.result);

    let got = ws_call(
        port,
        &web,
        json!({"type": "call", "id": "c-get", "target": "creds",
               "payload": {"type": "get", "ref": REF}}),
    )
    .await;
    assert_eq!(got["data"]["fields"]["api_key"], json!(VALUE), "{got}");
    assert_eq!(got["secret"], json!(["fields.api_key"]), "{got}");

    // a plain verb's reply frame has no `secret` key at all
    let plain = ws_call(
        port,
        &web,
        json!({"type": "call", "id": "c-list", "target": "creds", "payload": {"type": "list"}}),
    )
    .await;
    assert!(plain["data"]["refs"].is_array(), "{plain}");
    assert!(plain.get("secret").is_none(), "{plain}");
    stop(&kb, &web).await;
}

/// Everything kernel `k` keeps of its calls: the rows, the ledger, the state
/// stream and, after a flush, the memory store — as one text.
async fn everything_kept(k: &Arc<Kernel>, events: &Mutex<Vec<Value>>) -> String {
    k.flush_executions().await;
    let dumped = op(k, "memory", json!({"type": "dump"})).await.result;
    let seen = format!("{:?}", events.lock().unwrap());
    format!(
        "{}\n{}\n{}\n{}",
        serde_json::to_string(&k.rows.all()).unwrap(),
        serde_json::to_string(&k.ledger.entries()).unwrap(),
        seen,
        dumped
    )
}

/// Kernel A with a ws_bridge `leg` into B's web_ws, plus a state-stream tap.
async fn connected_caller(port: u16, web: &str) -> (Arc<Kernel>, String, Arc<Mutex<Vec<Value>>>) {
    let ka = kernel("A-g").await;
    let leg = format!("{}_a", tag());
    let made = create(
        &ka,
        json!({"handler_module": fantastic_bridge::WS_HANDLER_MODULE, "id": leg,
               "transport": "memory", "peer_id": web}),
    )
    .await;
    assert_eq!(made["id"], json!(leg), "{made}");
    let half = fantastic_bridge::inject_one(&AgentId::from(leg.as_str()));
    let booted = op(&ka, &leg, json!({"type": "boot"})).await.result;
    assert_eq!(booted["booted"], json!(true), "{booted}");
    super::binary_steps::ws_end_for_tests(port, web, half).await;
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let tap = Arc::clone(&events);
    let _token = ka.add_state_subscriber(Arc::new(move |ev: &Value, _b| {
        tap.lock().unwrap().push(ev.clone());
    }));
    (ka, leg, events)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_get_to_a_connected_credential_store_is_recorded_as_secret() {
    let kb = kernel("B-g").await;
    let (port, web, _ws) = serve_store(&kb).await;
    let set = op(
        &kb,
        "creds",
        json!({"type": "set", "ref": REF, "fields": {"api_key": VALUE}}),
    )
    .await;
    assert_eq!(set.result["stored"], json!(true), "{}", set.result);
    let (ka, leg, events) = connected_caller(port, &web).await;

    let out = ka
        .send(vec![Item::new(
            AgentId::from(leg.as_str()),
            json!({"type": "forward", "target": "creds",
                   "payload": {"type": "get", "ref": REF}}),
        )
        .with_id("far-get-1")])
        .await
        .remove(0);
    assert_eq!(
        out.result["fields"]["api_key"],
        json!(VALUE),
        "{}",
        out.result
    );

    let kept = everything_kept(&ka, &events).await;
    assert!(
        kept.contains("far-get-1"),
        "the get is not recorded: {kept}"
    );
    assert!(kept.contains("[secret]"), "{kept}");
    assert!(!kept.contains(VALUE), "the caller kept the value: {kept}");
    stop(&kb, &web).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_set_to_a_connected_credential_store_never_records_the_value_on_the_caller() {
    let kb = kernel("B-g").await;
    let (port, web, _ws) = serve_store(&kb).await;
    let (ka, leg, events) = connected_caller(port, &web).await;

    let out = ka
        .send(vec![Item::new(
            AgentId::from(leg.as_str()),
            json!({"type": "forward", "target": "creds",
                   "payload": {"type": "set", "ref": REF, "fields": {"api_key": VALUE}}}),
        )
        .with_id("far-set-1")])
        .await
        .remove(0);
    assert_eq!(out.result["stored"], json!(true), "{}", out.result);

    let kept = everything_kept(&ka, &events).await;
    assert!(
        kept.contains("far-set-1"),
        "the set is not recorded: {kept}"
    );
    assert!(kept.contains("[secret]"), "{kept}");
    assert!(!kept.contains(VALUE), "the caller kept the value: {kept}");
    // B really stored it
    let there = op(&kb, "creds", json!({"type": "get", "ref": REF})).await;
    assert_eq!(
        there.result["fields"]["api_key"],
        json!(VALUE),
        "{}",
        there.result
    );
    stop(&kb, &web).await;
}
