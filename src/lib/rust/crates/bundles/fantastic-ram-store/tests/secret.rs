//! SECRET VALUES on a call (owner, 2026-10-01; `fantastic_kernel::secret`): the
//! caller marks paths on the batch item; the target gets the real payload and the
//! caller the real reply, while every record and mirror of the call shows
//! `<secret>`.
//!
//! The twin of python `tests/kernel/test_secret.py` — here because the last case
//! needs the real `ram_store` the kernel flushes into.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::secret::PLACEHOLDER;
use fantastic_kernel::test_support::register_root_loader;
use fantastic_kernel::{AgentId, Item, Kernel, Outcome};
use fantastic_ram_store::RamStoreBundle;
use serde_json::{json, Map, Value};

const KEY: &str = "sk-live-0123456789abcdef";
const KEY2: &str = "sk-live-dotted-key-5a5a5a5a";
const ISSUED: &str = "tok-issued-fedcba9876543210";

/// What the probe's handler was handed, in order.
type Got = Arc<Mutex<Vec<Value>>>;

struct Probe(Got);

#[async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "secret_probe.tools"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload["type"].as_str().unwrap_or("");
        if matches!(verb, "boot" | "shutdown" | "reflect") {
            return Ok(Some(
                json!({"id": id.0, "sentence": "secret probe", "verbs": {}}),
            ));
        }
        self.0.lock().unwrap().push(payload.clone());
        match verb {
            "set_key" => {
                let n = payload["key"].as_str().map_or(0, |k| k.chars().count());
                Ok(Some(json!({"ok": true, "len": n})))
            }
            "issue" => Ok(Some(json!({"key": ISSUED, "note": "issued"}))),
            "hold" => Ok(Some(json!({"ok": true}))),
            "issue_dotted" => Ok(Some(
                json!({"a.b": ISSUED, "a": {"b": "nested-plain"}, "items": ["x", ISSUED]}),
            )),
            // The caller's marks as the handler reads them, and as a send the
            // handler makes itself reads them (it never inherits them).
            "marks" => Ok(Some(json!({"paths": fantastic_kernel::current_secret()}))),
            "relay" => {
                let nested = kernel
                    .send(vec![Item::new(id.clone(), json!({"type": "marks"}))])
                    .await
                    .remove(0)
                    .result;
                Ok(Some(json!({
                    "paths": fantastic_kernel::current_secret(),
                    "nested": nested["paths"],
                })))
            }
            "fail" => Err(format!("rejected key {}", payload["key"].as_str().unwrap_or("")).into()),
            other => Ok(Some(json!({"error": format!("unknown verb '{other}'")}))),
        }
    }

    /// `store` keeps what it was handed — the header AND the raw body.
    async fn handle_binary(
        &self,
        id: &AgentId,
        header: Value,
        body: Option<fantastic_kernel::Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<fantastic_kernel::Blob>), BundleError> {
        let blob: Vec<u8> = body.map(|b| b.bytes).unwrap_or_default();
        if header["type"] != json!("store") {
            return Ok((self.handle(id, &header, kernel).await?, None));
        }
        let mut got = header.clone();
        got["blob"] = json!(String::from_utf8_lossy(&blob));
        self.0.lock().unwrap().push(got);
        Ok((Some(json!({"stored": blob.len()})), None))
    }
}

fn seeded() -> (Arc<Kernel>, Got) {
    let got: Got = Arc::default();
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        RamStoreBundle::default(),
    );
    reg.register("secret_probe.tools", Probe(Arc::clone(&got)));
    let booted = bootstrap(reg, BootstrapOptions::in_memory()).expect("boot");
    (booted.kernel, got)
}

fn root_id(k: &Arc<Kernel>) -> String {
    k.root().unwrap().id.0.clone()
}

async fn send(k: &Arc<Kernel>, item: Item) -> Outcome {
    k.send(vec![item]).await.remove(0)
}

async fn mk(k: &Arc<Kernel>) -> AgentId {
    let out = send(
        k,
        Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": root_id(k),
                "handler_module": "secret_probe.tools", "id": "vault",
            }),
        ),
    )
    .await;
    AgentId::from(out.result["id"].as_str().expect("created"))
}

fn secret_item(target: &AgentId, payload: Value, secret: Value) -> Item {
    Item::new(target.clone(), payload).with_secret(secret)
}

async fn ledger(k: &Arc<Kernel>) -> Value {
    send(
        k,
        Item::new(AgentId::from("kernel"), json!({"type": "ledger"})),
    )
    .await
    .result["executions"]
        .clone()
}

fn mentions(x: &Value, value: &str) -> bool {
    x.to_string().contains(value)
}

// ─── the item field ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_bad_secret_field_is_refused_before_anything_runs() {
    let (k, got) = seeded();
    let a = mk(&k).await;
    got.lock().unwrap().clear();
    for (secret, why) in [
        (json!("payload.key"), "non-empty list"),
        (json!([]), "non-empty list"),
        (json!(["key"]), "must start with 'payload.'"),
        (json!(["payload"]), "must start with 'payload.'"),
        (json!(["payload.type"]), "cannot hide payload.type"),
        (json!(["payload.nope"]), "names nothing in the payload"),
        (json!(["payload..key"]), "empty step"),
    ] {
        let out = send(
            &k,
            secret_item(&a, json!({"type": "set_key", "key": KEY}), secret.clone()),
        )
        .await;
        assert!(
            out.status.as_str() == "failed" && out.pid.is_none(),
            "{secret}: {out:?}"
        );
        let r = &out.result;
        assert!(
            r["reason"] == json!("bad_arg") && r["arg"] == json!("secret"),
            "{secret}: {r}"
        );
        assert!(r["error"].as_str().unwrap().contains(why), "{secret}: {r}");
    }
    assert!(
        got.lock().unwrap().is_empty(),
        "a refused item must not reach the handler"
    );
}

// ─── a secret payload field ──────────────────────────────────────────────────

#[tokio::test]
async fn the_target_gets_the_value_and_every_record_shows_the_placeholder() {
    let (k, got) = seeded();
    let a = mk(&k).await;
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let tap = Arc::clone(&events);
    k.add_state_subscriber(Arc::new(move |evt: &Value, _body| {
        tap.lock().unwrap().push(evt.clone());
    }));
    got.lock().unwrap().clear();
    let out = send(
        &k,
        secret_item(
            &a,
            json!({"type": "set_key", "ref": "openai", "key": KEY}),
            json!(["payload.key"]),
        ),
    )
    .await;
    // the handler and the caller see the real thing
    assert_eq!(
        *got.lock().unwrap(),
        vec![json!({"type": "set_key", "ref": "openai", "key": KEY})]
    );
    assert_eq!(out.status.as_str(), "completed");
    assert_eq!(out.result, json!({"ok": true, "len": KEY.len()}));
    // the call row: placeholder, the other fields intact, the PATHS recorded
    let row = k
        .rows
        .get_one(out.pid.unwrap().get())
        .expect("the call row");
    assert_eq!(
        row["payload"],
        json!({"type": "set_key", "ref": "openai", "key": PLACEHOLDER})
    );
    assert_eq!(row["secret"], json!(["payload.key"]));
    // the state stream's send event — payload AND summary
    let sends: Vec<Value> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == json!("send") && e["agent_id"] == json!(a.0))
        .cloned()
        .collect();
    assert!(
        !sends.is_empty() && !sends.iter().any(|e| mentions(e, KEY)),
        "{sends:?}"
    );
    assert_eq!(sends[0]["payload"]["key"], json!(PLACEHOLDER));
    // the ledger read-back
    assert!(!mentions(&ledger(&k).await, KEY));
}

#[tokio::test]
async fn a_watcher_mirror_shows_the_placeholder() {
    let (k, _got) = seeded();
    let a = mk(&k).await;
    let mut rx = k.ensure_inbox(&AgentId::from("spy"));
    assert!(k.watch(&a, AgentId::from("spy")).await.is_none());
    send(
        &k,
        secret_item(
            &a,
            json!({"type": "set_key", "key": KEY}),
            json!(["payload.key"]),
        ),
    )
    .await;
    let mut mirrored = Vec::new();
    while let Some(it) = rx.try_recv() {
        mirrored.push(it);
    }
    assert!(!mirrored.is_empty());
    assert!(
        !mirrored.iter().any(|m| mentions(&m.payload, KEY)
            || m.body
                .as_ref()
                .is_some_and(|b| String::from_utf8_lossy(&b.bytes).contains(KEY))),
        "{mirrored:?}"
    );
}

#[tokio::test]
async fn an_error_text_quoting_the_value_is_scrubbed_in_the_record() {
    let (k, _got) = seeded();
    let a = mk(&k).await;
    let out = send(
        &k,
        secret_item(
            &a,
            json!({"type": "fail", "key": KEY}),
            json!(["payload.key"]),
        ),
    )
    .await;
    assert_eq!(out.status.as_str(), "failed");
    assert!(
        out.result["error"].as_str().unwrap().contains(KEY),
        "the caller holds the real outcome"
    );
    let led = ledger(&k).await;
    assert!(!mentions(&led, KEY), "{led}");
    let pid = json!(out.pid.unwrap().get());
    let mine = led
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == pid)
        .expect("the execution");
    assert!(
        mine["outcome"]["error"]
            .as_str()
            .unwrap()
            .contains(PLACEHOLDER),
        "{mine}"
    );
}

// ─── a secret result ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_secret_result_reaches_the_caller_and_is_kept_as_the_placeholder() {
    let (k, _got) = seeded();
    let a = mk(&k).await;
    let out = send(
        &k,
        secret_item(&a, json!({"type": "issue"}), json!(["result.key"]))
            // named: the settle keeps the whole result
            .with_id("issue-1"),
    )
    .await;
    assert_eq!(out.result, json!({"key": ISSUED, "note": "issued"}));
    let led = ledger(&k).await;
    assert!(!mentions(&led, ISSUED), "{led}");
    let mine = led
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == json!("issue-1"))
        .expect("the named execution");
    assert_eq!(
        mine["outcome"]["result"],
        json!({"key": PLACEHOLDER, "note": "issued"})
    );
}

#[tokio::test]
async fn the_whole_result_can_be_secret() {
    let (k, _got) = seeded();
    let a = mk(&k).await;
    let out = send(
        &k,
        secret_item(&a, json!({"type": "issue"}), json!(["result"])).with_id("issue-2"),
    )
    .await;
    assert_eq!(out.result["key"], json!(ISSUED));
    let led = ledger(&k).await;
    let mine = led
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == json!("issue-2"))
        .expect("the named execution");
    assert_eq!(mine["outcome"]["result"], json!(PLACEHOLDER));
}

// ─── the record cannot answer a replay ───────────────────────────────────────

/// A `ram_store` `memory` under the root, named by the root; the kernel started.
async fn with_store(k: &Arc<Kernel>) {
    let r = send(
        k,
        Item::new(
            AgentId::from("kernel"),
            json!({
                "type": "create_agent", "parent_id": root_id(k), "id": "memory",
                "handler_module": "ram_store.tools", "operator": true, "admins": ["kernel"],
            }),
        ),
    )
    .await
    .result;
    assert_eq!(r["id"], json!("memory"), "{r}");
    let mut patch = Map::new();
    patch.insert("memory_store_id".to_string(), json!("memory"));
    k.update(&AgentId::from(root_id(k)), patch);
    let out = k.start(false).await;
    assert_eq!(out["started"], json!(true), "{out}");
}

#[tokio::test]
async fn the_store_never_holds_the_value_and_a_replay_is_refused() {
    let (k, got) = seeded();
    with_store(&k).await;
    let a = mk(&k).await;
    got.lock().unwrap().clear();
    let key_item = || {
        secret_item(
            &a,
            json!({"type": "set_key", "key": KEY}),
            json!(["payload.key"]),
        )
        .with_id("key-1")
    };
    let first = send(&k, key_item()).await;
    assert_eq!(first.status.as_str(), "completed");
    // in RAM the execution still answers a re-send (the same process holds it)
    let again = send(&k, key_item()).await;
    assert_eq!(again.result, first.result);
    assert_eq!(got.lock().unwrap().len(), 1);
    k.flush_executions().await;
    let tables = send(
        &k,
        Item::new(AgentId::from("memory"), json!({"type": "dump"})),
    )
    .await
    .result["tables"]
        .clone();
    assert!(
        !mentions(&tables, KEY),
        "the memory store holds the secret value"
    );
    // once it lives only in the store, the record cannot answer — refused, loud
    let first_pid = first.pid.unwrap().get();
    k.ledger.forget(first_pid);
    assert!(!k.ledger.has(first_pid));
    let replay = send(&k, key_item()).await;
    assert_eq!(replay.status.as_str(), "failed", "{replay:?}");
    assert!(
        replay.result["reason"] == json!("bad_arg")
            && replay.result["error"]
                .as_str()
                .unwrap()
                .contains("carried secrets"),
        "{replay:?}"
    );
    assert_eq!(
        got.lock().unwrap().len(),
        1,
        "the refused replay must not run the handler"
    );
}

// ─── the binary channel ──────────────────────────────────────────────────────
//
// Rust carries a raw request body BESIDE the payload (`Item::with_binary`); it IS
// `payload.bytes`, as python's payload dict holds it. Marked, it is hidden from
// every record and mirror (python writes the placeholder over the bytes); a mark
// on any other field leaves the body riding as before.

async fn store_secret(k: &Arc<Kernel>, a: &AgentId, secret: Value) -> Outcome {
    send(
        k,
        Item::new(a.clone(), json!({"type": "store", "label": KEY}))
            .with_binary_key("bytes", KEY.as_bytes().to_vec())
            .with_secret(secret),
    )
    .await
}

#[tokio::test]
async fn a_marked_request_body_reaches_the_handler_and_no_record_or_mirror() {
    let (k, got) = seeded();
    let a = mk(&k).await;
    let events: Arc<Mutex<Vec<(Value, bool)>>> = Arc::default();
    let tap = Arc::clone(&events);
    k.add_state_subscriber(Arc::new(move |evt: &Value, body| {
        tap.lock().unwrap().push((evt.clone(), body.is_some()));
    }));
    let mut rx = k.ensure_inbox(&AgentId::from("spy"));
    assert!(k.watch(&a, AgentId::from("spy")).await.is_none());
    got.lock().unwrap().clear();
    let out = store_secret(&k, &a, json!(["payload.bytes", "payload.label"])).await;
    assert_eq!(out.status.as_str(), "completed", "{out:?}");
    assert_eq!(
        *got.lock().unwrap(),
        vec![json!({"type": "store", "label": KEY, "blob": KEY})],
        "the handler gets the real header and body"
    );
    let row = k
        .rows
        .get_one(out.pid.unwrap().get())
        .expect("the call row");
    assert_eq!(
        row["payload"],
        json!({"type": "store", "label": PLACEHOLDER, "bytes": PLACEHOLDER})
    );
    let mirrored: Vec<_> = std::iter::from_fn(|| rx.try_recv()).collect();
    assert!(!mirrored.is_empty());
    for m in &mirrored {
        assert!(m.body.is_none(), "a hidden body is never mirrored: {m:?}");
        assert!(!mentions(&m.payload, KEY), "{m:?}");
    }
    let sends: Vec<(Value, bool)> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|(e, _)| e["kind"] == json!("send") && e["agent_id"] == json!(a.0))
        .cloned()
        .collect();
    assert!(!sends.is_empty());
    for (e, with_body) in &sends {
        assert!(!with_body && !mentions(e, KEY), "{e}");
        assert_eq!(e["payload"]["bytes"], json!(PLACEHOLDER));
    }
}

#[tokio::test]
async fn a_mark_on_another_field_keeps_the_body_riding() {
    let (k, got) = seeded();
    let a = mk(&k).await;
    let mut rx = k.ensure_inbox(&AgentId::from("spy"));
    assert!(k.watch(&a, AgentId::from("spy")).await.is_none());
    got.lock().unwrap().clear();
    let out = send(
        &k,
        Item::new(a.clone(), json!({"type": "store", "label": KEY}))
            .with_binary_key("bytes", b"body".to_vec())
            .with_secret(json!(["payload.label"])),
    )
    .await;
    assert_eq!(out.status.as_str(), "completed", "{out:?}");
    let row = k
        .rows
        .get_one(out.pid.unwrap().get())
        .expect("the call row");
    assert_eq!(
        row["payload"],
        json!({"type": "store", "label": PLACEHOLDER, "bytes": {"len": 4}})
    );
    let mirrored: Vec<_> = std::iter::from_fn(|| rx.try_recv()).collect();
    assert!(mirrored
        .iter()
        .any(|m| m.body.as_ref().is_some_and(|b| b.bytes == b"body")));
    assert!(!mirrored.iter().any(|m| mentions(&m.payload, KEY)));
}

/// `current_secret()` — the paths the caller marked on the item that started
/// THIS dispatch, empty when none; a send the handler makes itself starts with
/// none (python `_current_secret`, set per dispatch task).
#[tokio::test]
async fn the_handler_reads_its_callers_marks_and_its_own_sends_inherit_none() {
    let (k, _got) = seeded();
    let a = mk(&k).await;
    let plain = send(&k, Item::new(a.clone(), json!({"type": "marks"}))).await;
    assert_eq!(plain.result, json!({"paths": []}));
    let out = send(
        &k,
        Item::new(a.clone(), json!({"type": "relay", "key": KEY}))
            .with_secret(json!(["payload.key", "result.paths"])),
    )
    .await;
    assert_eq!(out.status.as_str(), "completed", "{out:?}");
    assert_eq!(
        out.result,
        json!({"paths": ["payload.key", "result.paths"], "nested": []})
    );
    // outside any dispatch there are none
    assert!(fantastic_kernel::current_secret().is_empty());
}

// ─── a mark as an ARRAY OF STEPS (owner, 2026-10-03) ─────────────────────────

fn call_row(k: &Arc<Kernel>, out: &Outcome) -> Value {
    Value::Object(
        k.rows
            .get_one(out.pid.expect("a pid").get())
            .expect("the call row"),
    )
}

/// `["payload", "a.b"]` is the key `"a.b"`, taken literally; the dotted string
/// `payload.a.b` keeps meaning `a` → `b`.
#[tokio::test]
async fn an_array_mark_masks_a_literal_dotted_key_and_a_dotted_one_does_not() {
    let (k, got) = seeded();
    let a = mk(&k).await;
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let tap = Arc::clone(&events);
    k.add_state_subscriber(Arc::new(move |evt: &Value, _body| {
        tap.lock().unwrap().push(evt.clone());
    }));
    got.lock().unwrap().clear();
    let sent = json!({"type": "hold", "a.b": KEY, "a": {"b": "nested-plain"}});
    let out = send(
        &k,
        secret_item(&a, sent.clone(), json!([["payload", "a.b"]])),
    )
    .await;
    assert!(
        out.status.as_str() == "completed" && out.result == json!({"ok": true}),
        "{out:?}"
    );
    assert_eq!(
        *got.lock().unwrap(),
        vec![sent],
        "the target gets the real payload"
    );
    let row = call_row(&k, &out);
    assert_eq!(
        row["payload"],
        json!({"type": "hold", "a.b": PLACEHOLDER, "a": {"b": "nested-plain"}}),
        "{row}"
    );
    assert_eq!(row["secret"], json!([["payload", "a.b"]]), "{row}");
    let sends: Vec<Value> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == json!("send") && e["agent_id"] == json!(a.0))
        .cloned()
        .collect();
    assert!(
        !sends.is_empty() && !sends.iter().any(|e| mentions(e, KEY)),
        "{sends:?}"
    );
    assert!(!mentions(&ledger(&k).await, KEY));
    // the dotted spelling of the same text is the NESTED path
    let out = send(
        &k,
        secret_item(
            &a,
            json!({"type": "hold", "a.b": "literal-plain", "a": {"b": KEY}}),
            json!(["payload.a.b"]),
        ),
    )
    .await;
    assert_eq!(
        call_row(&k, &out)["payload"],
        json!({"type": "hold", "a.b": "literal-plain", "a": {"b": PLACEHOLDER}})
    );
}

/// Both spellings in one list: every payload and result mark masks, the call row
/// records the marks as given, and the handler reads them back as given.
#[tokio::test]
async fn a_mixed_list_of_string_and_array_marks() {
    let (k, _got) = seeded();
    let a = mk(&k).await;
    let marks = json!([
        "payload.key",
        ["payload", "a.b"],
        ["result", "a.b"],
        ["result", "items", 1]
    ]);
    let out = send(
        &k,
        secret_item(
            &a,
            json!({"type": "issue_dotted", "key": KEY, "a.b": KEY2}),
            marks.clone(),
        )
        // named: the settle keeps the whole result
        .with_id("mixed-1"),
    )
    .await;
    assert_eq!(
        out.result,
        json!({"a.b": ISSUED, "a": {"b": "nested-plain"}, "items": ["x", ISSUED]}),
        "the caller holds the real reply"
    );
    let row = call_row(&k, &out);
    assert_eq!(row["secret"], marks, "{row}");
    assert_eq!(
        row["payload"],
        json!({"type": "issue_dotted", "key": PLACEHOLDER, "a.b": PLACEHOLDER}),
        "{row}"
    );
    let led = ledger(&k).await;
    for v in [KEY, KEY2, ISSUED] {
        assert!(!mentions(&led, v), "{v}: {led}");
    }
    let mine = led
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == json!("mixed-1"))
        .expect("the named execution");
    assert_eq!(
        mine["outcome"]["result"],
        json!({"a.b": PLACEHOLDER, "a": {"b": "nested-plain"}, "items": ["x", PLACEHOLDER]}),
        "{mine}"
    );
    // `current_secret()` is the marks exactly as given, an array as an array
    let got = send(
        &k,
        secret_item(
            &a,
            json!({"type": "marks", "key": KEY, "a.b": KEY2}),
            marks.clone(),
        ),
    )
    .await;
    assert_eq!(got.result, json!({"paths": marks}), "{got:?}");
}

#[tokio::test]
async fn array_steps_are_exact_an_int_indexes_a_list_a_string_names_a_key() {
    let (k, _got) = seeded();
    let a = mk(&k).await;
    let payload = json!({"type": "hold", "items": [KEY, "plain"], "m": {"0": KEY2}});
    let with = |field: &str, v: Value| {
        let mut p = payload.clone();
        p[field] = v;
        p
    };
    for (secret, shown) in [
        (
            json!([["payload", "items", 0]]),
            with("items", json!([PLACEHOLDER, "plain"])),
        ),
        (
            json!([["payload", "m", "0"]]),
            with("m", json!({"0": PLACEHOLDER})),
        ),
        (
            json!(["payload.items.0"]),
            with("items", json!([PLACEHOLDER, "plain"])),
        ),
    ] {
        let out = send(&k, secret_item(&a, payload.clone(), secret.clone())).await;
        assert_eq!(out.status.as_str(), "completed", "{secret}: {out:?}");
        assert_eq!(call_row(&k, &out)["payload"], shown, "{secret}");
    }
    // a string step is never an index, an int step never a key
    for secret in [
        json!([["payload", "items", "0"]]),
        json!([["payload", "m", 0]]),
    ] {
        let out = send(&k, secret_item(&a, payload.clone(), secret.clone())).await;
        assert_eq!(
            out.result["error"],
            json!(format!(
                "send: secret path {} names nothing in the payload",
                fantastic_bundle::py_repr_value(&secret[0])
            )),
            "{out:?}"
        );
    }
}

/// The array refusals, and the dotted-string ones byte for byte unchanged.
#[tokio::test]
async fn each_malformed_mark_is_refused_with_its_own_text() {
    let (k, got) = seeded();
    let a = mk(&k).await;
    got.lock().unwrap().clear();
    let step = "is neither a string (an object key) nor a non-negative integer (a list index)";
    let root = "must start with 'payload' and a step under it, or with 'result'";
    let hide = "cannot hide payload.type — the verb is how every row is read";
    for (secret, text) in [
        (json!([[]]), "path [] has no steps".to_string()),
        (
            json!([["payload", true]]),
            format!("path ['payload', True] step True {step}"),
        ),
        (
            json!([["payload", 1.5]]),
            format!("path ['payload', 1.5] step 1.5 {step}"),
        ),
        (
            json!([["payload", null]]),
            format!("path ['payload', None] step None {step}"),
        ),
        (
            json!([["payload", -1]]),
            format!("path ['payload', -1] step -1 {step}"),
        ),
        (
            json!([["payload", ["a"]]]),
            format!("path ['payload', ['a']] step ['a'] {step}"),
        ),
        (json!([["key"]]), format!("path ['key'] {root}")),
        (json!([[0, "key"]]), format!("path [0, 'key'] {root}")),
        (json!([["payload"]]), format!("path ['payload'] {root}")),
        (json!([["payload", "type"]]), hide.to_string()),
        (
            json!([["payload", "nope"]]),
            "path ['payload', 'nope'] names nothing in the payload".to_string(),
        ),
        (
            json!([["payload", "a", "b"]]),
            "path ['payload', 'a', 'b'] names nothing in the payload".to_string(),
        ),
        (
            json!(["payload.key", ["payload", "nope"]]),
            "path ['payload', 'nope'] names nothing in the payload".to_string(),
        ),
        (
            json!([5]),
            "path must be a dotted string or an array of steps, got 5".to_string(),
        ),
        (
            json!([{"payload": "key"}]),
            "path must be a dotted string or an array of steps, got {'payload': 'key'}".to_string(),
        ),
        // the dotted-string texts, unchanged
        (
            json!("payload.key"),
            "must be a non-empty list of paths, got 'payload.key'".to_string(),
        ),
        (
            json!([]),
            "must be a non-empty list of paths, got []".to_string(),
        ),
        (
            json!([""]),
            "path must be a non-empty string, got ''".to_string(),
        ),
        (
            json!(["payload..key"]),
            "path 'payload..key' has an empty step".to_string(),
        ),
        (
            json!(["key"]),
            "path 'key' must start with 'payload.' or be 'result[.…]'".to_string(),
        ),
        (
            json!(["payload"]),
            "path 'payload' must start with 'payload.' or be 'result[.…]'".to_string(),
        ),
        (json!(["payload.type"]), hide.to_string()),
        (
            json!(["payload.a.b"]),
            "path 'payload.a.b' names nothing in the payload".to_string(),
        ),
    ] {
        let out = send(
            &k,
            secret_item(
                &a,
                json!({"type": "set_key", "key": KEY, "a.b": KEY2}),
                secret.clone(),
            ),
        )
        .await;
        assert!(
            out.status.as_str() == "failed" && out.pid.is_none(),
            "{secret}: {out:?}"
        );
        assert_eq!(
            out.result,
            json!({"error": format!("send: secret {text}"), "reason": "bad_arg", "arg": "secret"}),
            "{secret}"
        );
    }
    assert!(
        got.lock().unwrap().is_empty(),
        "a refused item must not reach the handler"
    );
}
