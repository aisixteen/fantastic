//! Unit tests for [`crate::send`].
use crate::outcome::{Blob, Item, Step};

use super::*;
use crate::agent::Agent;
use crate::test_support::as_agent;
use serde_json::Map;
use std::path::Path;

/// One BINARY item through the batch — the only send path; bytes ride beside
/// the payload (`Item::with_binary`).
async fn send_bin(
    kernel: &Arc<Kernel>,
    target: &AgentId,
    payload: Value,
    blob: Vec<u8>,
) -> (Value, Option<Vec<u8>>) {
    let out = kernel
        .send(vec![
            Item::new(target.clone(), payload).with_binary_key("bytes", blob)
        ])
        .await
        .remove(0);
    (out.result, out.body.map(|b| b.bytes))
}

fn make_agent(id: &str) -> Arc<Agent> {
    Agent::new(
        id.into(),
        None,
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

#[test]
fn summarize_never_truncates() {
    // It used to cut at 160 chars + "...". A summary that silently drops the end of
    // a payload lies about what was sent, and the reader cannot tell a short payload
    // from a cut one — while the dropped part is exactly what you opened telemetry
    // to find. Eliding is a RENDER decision, where whoever is looking can see it
    // happen. Mirrors py `test_summary_shrinks_bytes_but_never_truncates`.
    let big = "x".repeat(500);
    let v = json!({ "text": big });
    let s = summarize_payload(&v, None);
    assert!(s.contains(&big), "the payload must survive whole");
    assert!(!s.ends_with("..."));
}

#[test]
fn summarize_short_payload_unchanged() {
    let v = json!({"type": "ping"});
    let s = summarize_payload(&v, None);
    // python `json.dumps` separators: `, ` and `: `, not rust's compact form.
    assert_eq!(s, r#"{"type": "ping"}"#);
}

#[test]
fn current_sender_outside_scope_is_none() {
    assert!(current_sender().is_none());
}

#[tokio::test]
async fn the_dispatch_identity_propagates_to_nested_async() {
    let r = as_agent(AgentId::from("alice"), async {
        current_sender().map(|s| s.0)
    })
    .await;
    assert_eq!(r.as_deref(), Some("alice"));
}

#[tokio::test]
async fn send_to_missing_target_returns_error() {
    let kernel = Arc::new(Kernel::new());
    let v = kernel
        .send(vec![Item::new(
            AgentId::from("nope"),
            json!({"type": "ping"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(v["error"].as_str().unwrap_or("").contains("no agent"));
}

#[tokio::test]
async fn send_kernel_alias_routes_to_root() {
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // Root has no handler_module → reflect is answered natively.
    let v = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    // Reflect returns at least an id + a tree (full shape exercised
    // in reflect.rs tests + integration tests).
    assert!(v.is_object(), "reflect must return an object: {v:?}");
}

#[tokio::test]
async fn watch_then_emit_fans_payload_to_watcher() {
    let kernel = Arc::new(Kernel::new());
    let src = make_agent("src");
    kernel.register(Arc::clone(&src));
    let watcher = make_agent("watcher");
    kernel.register(Arc::clone(&watcher));
    let mut rx_watcher = kernel.ensure_inbox(&watcher.id);
    kernel
        .watch(&AgentId::from("src"), AgentId::from("watcher"))
        .await;
    kernel
        .emit(&AgentId::from("src"), json!({"type": "hello", "k": 1}))
        .await;
    // Watcher receives the RAW emit payload (python parity — see
    // `Kernel::fanout`). A state subscriber gets the `kind:"emit"` event;
    // a watcher sees what was actually emitted.
    let got = tokio::time::timeout(std::time::Duration::from_millis(100), rx_watcher.recv())
        .await
        .expect("watcher receive within 100ms")
        .expect("channel still open");
    assert_eq!(got.payload["type"], "hello");
    assert_eq!(got.payload["k"], 1);
}

#[tokio::test]
async fn unwatch_stops_fanout() {
    let kernel = Arc::new(Kernel::new());
    let src = make_agent("src");
    kernel.register(Arc::clone(&src));
    let watcher = make_agent("watcher");
    kernel.register(Arc::clone(&watcher));
    let mut rx_w = kernel.ensure_inbox(&watcher.id);
    kernel
        .watch(&AgentId::from("src"), AgentId::from("watcher"))
        .await;
    kernel
        .unwatch(&AgentId::from("src"), &AgentId::from("watcher"))
        .await;
    kernel
        .emit(&AgentId::from("src"), json!({"type": "after_unwatch"}))
        .await;
    // Nothing queued.
    assert!(
        rx_w.try_recv().is_none(),
        "an unwatched mirror still delivered"
    );
}

/// a16 (audit 2026-09-28): an emit onto ANOTHER agent's inbox passes the
/// two-sided gate under verb `emit`; a denied emit is dropped. Emitting on your
/// own inbox is how every bundle publishes, so it stays ungated.
#[tokio::test]
async fn emit_on_another_agent_needs_both_halves() {
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("root");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel.register(granted(
        "em_open",
        Some("root"),
        "can_call",
        json!({"em_target": ["emit"]}),
    ));
    kernel.register(granted("em_closed", Some("root"), "can_call", json!({})));
    kernel.register(granted(
        "em_target",
        Some("root"),
        "callable_by",
        json!({"em_open": ["emit"], "em_closed": ["emit"]}),
    ));
    let mut rx = kernel.ensure_inbox(&AgentId::from("em_target"));
    let target = AgentId::from("em_target");
    as_agent(AgentId::from("em_closed"), {
        let kernel = Arc::clone(&kernel);
        let target = target.clone();
        async move { kernel.emit(&target, json!({"type": "poke", "n": 1})).await }
    })
    .await;
    assert!(
        rx.try_recv().is_none(),
        "an emit with no egress grant must be dropped"
    );
    as_agent(AgentId::from("em_open"), {
        let kernel = Arc::clone(&kernel);
        let target = target.clone();
        async move { kernel.emit(&target, json!({"type": "poke", "n": 2})).await }
    })
    .await;
    let got = rx.try_recv().expect("the granted emit landed");
    assert_eq!(got.payload, json!({"type": "poke", "n": 2}));
    // emitting on your OWN inbox — ungated
    as_agent(AgentId::from("em_target"), {
        let kernel = Arc::clone(&kernel);
        let target = target.clone();
        async move { kernel.emit(&target, json!({"type": "self", "n": 3})).await }
    })
    .await;
    assert_eq!(rx.try_recv().expect("own emit").payload["n"], 3);
}

#[tokio::test]
async fn a_binary_item_to_a_missing_target_is_refused() {
    let kernel = Arc::new(Kernel::new());
    let (v, body) = send_bin(
        &kernel,
        &AgentId::from("nope"),
        json!({"type": "frob"}),
        vec![1, 2, 3],
    )
    .await;
    assert!(v["error"].as_str().unwrap_or("").contains("no agent"));
    assert_eq!(body, None);
}

#[tokio::test]
async fn a_binary_item_routes_through_bundle_handle_binary() {
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    struct EchoBytes;
    #[async_trait::async_trait]
    impl Bundle for EchoBytes {
        fn name(&self) -> &str {
            "echo_bytes"
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            _payload: &Value,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            Ok(Some(json!({"error": "text path not exercised here"})))
        }
        async fn handle_binary(
            &self,
            _agent_id: &AgentId,
            _header: Value,
            body: Option<crate::outcome::Blob>,
            _kernel: &Arc<Kernel>,
        ) -> Result<(Reply, Option<crate::outcome::Blob>), BundleError> {
            let blob: Vec<u8> = body.map(|b| b.bytes).unwrap_or_default();
            // Echo the bytes back as the reply BODY (symmetric channel).
            let len = blob.len();
            Ok((
                Some(json!({"ok": true, "len": len})),
                Some(Blob::key("bytes", blob)),
            ))
        }
    }
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("echo_bytes.tools", EchoBytes);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let agent = Agent::new(
        AgentId::from("eb"),
        Some("echo_bytes.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("eb"),
        true,
    );
    kernel.register(Arc::clone(&agent));
    let (reply, body) = send_bin(
        &kernel,
        &AgentId::from("eb"),
        json!({"type": "ingest"}),
        vec![0u8; 64],
    )
    .await;
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["len"], 64);
    // Symmetric channel: bytes flow back as the reply body too.
    assert_eq!(body.expect("a chunk came back").len(), 64);
}

#[tokio::test]
async fn binary_dispatch_refusals_match_the_text_channel() {
    // C34: python has ONE dispatch path for both channels, so a binary send to
    // a bare agent (no handler_module) or one whose bundle is not installed
    // answers the exact same UNHANDLED shape + `verb` the text channel does —
    // not a different, binary-only wording.
    let kernel = Arc::new(Kernel::new());
    let bare = make_agent("bare");
    kernel.register(Arc::clone(&bare));
    let (v, body) = send_bin(
        &kernel,
        &AgentId::from("bare"),
        json!({"type": "ingest"}),
        vec![1],
    )
    .await;
    assert_eq!(v["reason"], "unhandled");
    assert_eq!(v["verb"], "ingest");
    assert_eq!(
        v["error"],
        "agent 'bare' has no handler_module; cannot answer verb 'ingest'"
    );
    assert_eq!(body, None);

    let orphan = Agent::new(
        AgentId::from("orphan_b"),
        Some("nope_state.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("orphan_b"),
        true,
    );
    kernel.register(Arc::clone(&orphan));
    let (v, body) = send_bin(
        &kernel,
        &AgentId::from("orphan_b"),
        json!({"type": "ingest"}),
        vec![1],
    )
    .await;
    assert_eq!(v["reason"], "unhandled");
    assert_eq!(v["verb"], "ingest");
    assert_eq!(
        v["error"],
        "agent 'orphan_b': handler_module 'nope_state.tools' cannot be \
         imported in this runtime: ModuleNotFoundError: No module named \
         'nope_state'"
    );
    assert_eq!(body, None);
}

#[tokio::test]
async fn a_binary_bundle_error_settles_raised() {
    // C34: a bundle error on the binary channel settles `raised:true`,
    // exactly like the text channel's bundle-error path.
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    struct Boom;
    #[async_trait::async_trait]
    impl Bundle for Boom {
        fn name(&self) -> &str {
            "boom"
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            _payload: &Value,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            Ok(None)
        }
        async fn handle_binary(
            &self,
            _agent_id: &AgentId,
            _header: Value,
            _body: Option<crate::outcome::Blob>,
            _kernel: &Arc<Kernel>,
        ) -> Result<(Reply, Option<crate::outcome::Blob>), BundleError> {
            Err(BundleError::from("kaboom"))
        }
    }
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("boom.tools", Boom);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let agent = Agent::new(
        AgentId::from("bm"),
        Some("boom.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("bm"),
        true,
    );
    kernel.register(Arc::clone(&agent));
    let (v, body) = send_bin(
        &kernel,
        &AgentId::from("bm"),
        json!({"type": "ingest"}),
        vec![1],
    )
    .await;
    assert_eq!(v["raised"], true);
    assert!(v["error"].as_str().unwrap().contains("kaboom"));
    assert_eq!(body, None);
}

#[tokio::test]
async fn list_agents_returns_records_for_every_registered() {
    let kernel = Arc::new(Kernel::new());
    let a = make_agent("a_1");
    let b = make_agent("b_1");
    kernel.register(Arc::clone(&a));
    kernel.register(Arc::clone(&b));
    kernel.set_root(Arc::clone(&a));
    let v = kernel
        .send(vec![Item::new(
            AgentId::from("a_1"),
            json!({"type": "list_agents"}),
        )])
        .await
        .remove(0)
        .result;
    let agents = v["agents"].as_array().expect("list_agents.agents array");
    let ids: Vec<&str> = agents
        .iter()
        .filter_map(|x| x.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(ids, vec!["a_1", "b_1"]);
}

#[tokio::test]
async fn list_agents_points_back_at_the_briefing() {
    // C23: python's `_LIST_HINT` reads `<type>reflect</type>` — the TAG form,
    // not `{type:'reflect'}` — since that is the wire grammar a caller
    // actually sends.
    let kernel = Arc::new(Kernel::new());
    let a = make_agent("a_1");
    kernel.register(Arc::clone(&a));
    kernel.set_root(Arc::clone(&a));
    let v = kernel
        .send(vec![Item::new(
            AgentId::from("a_1"),
            json!({"type": "list_agents"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(v["more"].as_str().unwrap(), LIST_HINT);
    assert!(v["more"].as_str().unwrap().contains("<type>reflect</type>"));
    assert!(!v["more"].as_str().unwrap().contains("{type:'reflect'}"));
}

#[tokio::test]
async fn handler_module_not_installed_is_the_python_unhandled_shape() {
    // C24: `agent <id!r>: handler_module <hm!r> cannot be imported in this
    // runtime: <tail>` — the stable PREFIX is pinned; the exception tail is
    // not (python's own wording drifts between runs), so only the prefix and
    // the `reason`/`verb` machine fields are checked.
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let orphan = Agent::new(
        AgentId::from("orphan"),
        Some("nope_state.tools".to_string()),
        Some(AgentId::from("kernel_state")),
        Map::new(),
        Path::new("/tmp/nowhere").join("orphan"),
        true,
    );
    kernel.register(Arc::clone(&orphan));
    let v = kernel
        .send(vec![Item::new(
            AgentId::from("orphan"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(v["reason"], "unhandled");
    assert_eq!(v["verb"], "reflect");
    assert_eq!(
        v["error"],
        "agent 'orphan': handler_module 'nope_state.tools' cannot be \
         imported in this runtime: ModuleNotFoundError: No module named \
         'nope_state'"
    );
}

// ─── #2 — current_origin: the CALLER, not the callee ────────────────

#[tokio::test]
async fn a_handler_reads_its_caller_not_itself() {
    // `current_sender()` inside a handler is that handler's OWN agent — the
    // callee. A bundle whose payload NAMES a resource needs the CALLER, which
    // is what `current_origin()` carries.
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    use crate::send::current_origin;
    use crate::test_support::as_agent;
    use std::sync::Mutex;

    static SEEN: Mutex<Option<(Option<String>, Option<String>)>> = Mutex::new(None);

    struct Spy;
    #[async_trait::async_trait]
    impl Bundle for Spy {
        fn name(&self) -> &str {
            "origin_spy"
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            _payload: &Value,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            *SEEN.lock().unwrap() = Some((
                crate::send::current_sender().map(|a| a.0),
                current_origin().map(|a| a.0),
            ));
            Ok(Some(json!({"ok": true})))
        }
    }

    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("origin_spy.tools", Spy);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);

    // Two-sided gate: the edge exists only if BOTH ends grant it, so the agent
    // half of this test needs real wiring — an unregistered id standing in for a
    // handler is (correctly) deny-all.
    let mut spy_meta = Map::new();
    spy_meta.insert("callable_by".into(), json!({"caller": "*"}));
    let spy = Agent::new(
        AgentId::from("spy"),
        Some("origin_spy.tools".to_string()),
        None,
        spy_meta,
        Path::new("/tmp/nowhere").join("spy"),
        true,
    );
    kernel.register(Arc::clone(&spy));

    let mut caller_meta = Map::new();
    caller_meta.insert("can_call".into(), json!({"spy": "*"}));
    let caller = Agent::new(
        AgentId::from("caller"),
        None,
        None,
        caller_meta,
        Path::new("/tmp/nowhere").join("caller"),
        true,
    );
    kernel.register(Arc::clone(&caller));

    // OPERATOR send: there is no caller, so there is no origin.
    let r = kernel
        .send(vec![Item::new(spy.id.clone(), json!({"type": "poke"}))])
        .await
        .remove(0)
        .result;
    let (sender, origin) = SEEN
        .lock()
        .unwrap()
        .take()
        .unwrap_or_else(|| panic!("handler never ran; reply={r}"));
    assert_eq!(sender.as_deref(), Some("spy"), "sender is the CALLEE");
    assert_eq!(origin, None, "an operator send has no origin");

    // AGENT send: origin names the CALLER; sender still names the callee.
    let _ = as_agent(AgentId::from("caller"), async {
        kernel
            .send(vec![Item::new(spy.id.clone(), json!({"type": "poke"}))])
            .await
            .remove(0)
            .result
    })
    .await;
    let (sender, origin) = SEEN
        .lock()
        .unwrap()
        .take()
        .expect("the granted edge must reach the handler");
    assert_eq!(sender.as_deref(), Some("spy"));
    assert_eq!(
        origin.as_deref(),
        Some("caller"),
        "origin must be the CALLER — the whole reason the third local exists"
    );
}

// ─── the READ side of the gate: watch + the state stream ────────────

#[tokio::test]
async fn an_ungranted_agent_cannot_watch_another() {
    // A watcher receives its target's raw inbound payloads VERBATIM — the model
    // prompt, the document body, whatever was sent. So an ungated `watch` is a
    // read side-channel around `callable_by`: an agent that may not CALL an
    // agent could still read everything sent TO it.
    //
    // Found 2026-08-27 in a client audit: a browser leg granted exactly
    // `assistant:[send]` could mirror ANY agent's inbox in its session and read
    // the packed confidential context out of the model's prompt.
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let secret = make_agent("secret");
    kernel.register(Arc::clone(&secret));
    let nosy = make_agent("nosy");
    kernel.register(Arc::clone(&nosy));

    let denial = crate::test_support::as_agent(AgentId::from("nosy"), async {
        kernel
            .watch(&AgentId::from("secret"), AgentId::from("nosy"))
            .await
    })
    .await;
    assert!(denial.is_some(), "an ungranted watch was allowed");
    assert_eq!(denial.unwrap()["reason"], "unauthorized");
    // ...and nothing was registered, so no payload can ever reach it.
    assert!(!secret
        .watcher_ids
        .read()
        .await
        .contains(&AgentId::from("nosy")));
}

#[tokio::test]
async fn an_operator_watch_stays_ungated() {
    // `sender = None` is the origin root of trust — the bootstrap wiring a
    // renderer, a test driver. Same rule as send.
    let kernel = Arc::new(Kernel::new());
    let src = make_agent("src");
    kernel.register(Arc::clone(&src));
    // The watcher REGISTERS FIRST — `watch` mirrors into an inbox, it does not
    // create one on the caller's behalf, and being a registered AGENT is not
    // being a registered READER.
    kernel.register(make_agent("cli"));
    let _rx = kernel.ensure_inbox(&AgentId::from("cli"));
    assert!(kernel
        .watch(&AgentId::from("src"), AgentId::from("cli"))
        .await
        .is_none());
    assert!(src.watcher_ids.read().await.contains(&AgentId::from("cli")));
}

#[tokio::test]
async fn the_state_stream_needs_the_roots_own_grant_too() {
    // The broadest read the kernel offers: no agent taps it without a grant on
    // both ends that names `state_subscribe` (or `*`) on the root.
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let mut meta = Map::new();
    // A wildcard on the root — deliberately generous, and still not enough.
    meta.insert("can_call".into(), json!({"kernel_state": "*"}));
    let leaf = Agent::new(
        AgentId::from("leaf"),
        None,
        Some(root.id.clone()),
        meta,
        Path::new("/tmp/nowhere").join("leaf"),
        true,
    );
    kernel.register(Arc::clone(&leaf));

    let denial = crate::test_support::as_agent(AgentId::from("leaf"), async {
        kernel.authorize_state_subscribe(&AgentId::from("leaf"))
    })
    .await;
    assert!(
        denial.is_some(),
        "a wildcard covered the whole state stream"
    );
    assert_eq!(denial.unwrap()["reason"], "unauthorized");

    // An OPERATOR tap is ungated.
    assert!(kernel
        .authorize_state_subscribe(&AgentId::from("cli"))
        .is_none());
}

// ─── C20: the gate's denial shapes are python's ────────────────

/// An agent with an explicit grant, registered under `parent`.
fn granted(id: &str, parent: Option<&str>, field: &str, grant: Value) -> Arc<Agent> {
    let mut meta = Map::new();
    meta.insert(field.into(), grant);
    Agent::new(
        AgentId::from(id),
        None,
        parent.map(AgentId::from),
        meta,
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

fn keys(v: &Value) -> Vec<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

#[tokio::test]
async fn gate_denial_keys_are_error_reason_hint_side() {
    // Python builds these through `refusal(code, error, hint=…, side=…)`:
    // `refusal` writes `error, reason`, then `hint` if truthy, then merges the
    // extras — so `side` is LAST. The port wrote `side` before `hint`, and the
    // measured `prompt_req_05.json` msg 8 disagreed key for key.
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let store = make_agent("kernel_store");
    kernel.register(Arc::clone(&store));
    let totals = make_agent("totals");
    kernel.register(Arc::clone(&totals));

    // Egress: the sender holds no edge to the target.
    let d = kernel
        .gate(&AgentId::from("totals"), "write_stream", &store)
        .expect("deny-all gate let an ungranted call through");
    // Python's `!r` — single quotes, not rust's Debug double quotes.
    assert_eq!(
        d["error"],
        "totals may not call 'write_stream' on kernel_store"
    );
    assert_eq!(keys(&d), ["error", "reason", "hint", "side"]);
    assert_eq!(d["side"], "egress");

    // Ingress: the sender may call out, the target does not accept it.
    let ws = make_agent("ws");
    kernel.register(Arc::clone(&ws));
    let rest = granted("rest", Some("kernel_state"), "can_call", json!({"ws": "*"}));
    kernel.register(Arc::clone(&rest));
    let d = kernel
        .gate(&AgentId::from("rest"), "reflect", &ws)
        .expect("an unaccepted call was admitted");
    assert_eq!(d["error"], "ws does not accept 'reflect' from rest");
    assert_eq!(keys(&d), ["error", "reason", "hint", "side"]);
    assert_eq!(d["side"], "ingress");
}

// ── C36: the call row and its settle ───────────────────────────────────

/// A bundle that answers whatever the payload asks for: `{"type":"work"}` is a
/// plain reply, `{"type":"nope"}` a domain refusal with a `reason`.
struct C36Bundle;

#[async_trait::async_trait]
impl crate::bundle::Bundle for C36Bundle {
    fn name(&self) -> &str {
        "c36"
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "nope" => Ok(Some(
                json!({"error": "х".repeat(300), "reason": "not_found", "extra": 1}),
            )),
            // C89: echoes back WHATEVER `error` value the caller sent, so one
            // handler can probe every falsy shape python's truthiness admits.
            "echo_error" => Ok(Some(
                json!({"error": payload.get("error").cloned().unwrap_or(Value::Null)}),
            )),
            _ => Ok(Some(json!({"ok": true}))),
        }
    }
}

fn c36_kernel() -> (Arc<Kernel>, Arc<Agent>) {
    let mut kernel = Kernel::new();
    let mut reg = crate::bundle::BundleRegistry::new();
    reg.register("c36.tools", C36Bundle);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let worker = Agent::new(
        AgentId::from("w"),
        Some("c36.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("w"),
        true,
    );
    kernel.register(Arc::clone(&worker));
    (kernel, worker)
}

#[tokio::test]
async fn a_dispatch_writes_a_call_row_with_outcome() {
    let (kernel, worker) = c36_kernel();
    let out = kernel
        .send(vec![
            Item::new(worker.id.clone(), json!({"type": "work"})).with_id("named-one")
        ])
        .await
        .remove(0);
    let rows = crate::test_support::rows_about(&kernel, &worker.id);
    assert_eq!(rows.len(), 1, "one call row on the TARGET: {rows:?}");
    let call = &rows[0];
    // PYTHON'S KEY ORDER: the row as it was written, then the two keys the store
    // adds (`{**row, "id": rid, "agent": agent_id}` — `id` keeps its place).
    // The whole call goes down — the payload verbatim, the retry budget, the
    // attempt count, the deadline — and is what the kernel flushes into its
    // memory store.
    assert_eq!(
        call.keys().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "id", "kind", "verb", "parent", "caller", "payload", "named", "retries", "attempts",
            "timeout", "name", "agent", "ts", "outcome"
        ],
        "…and `outcome` is stamped onto the end at settle"
    );
    assert_eq!(call["payload"], json!({"type": "work"}));
    assert_eq!(call["named"], json!(true));
    assert_eq!(call["retries"], json!(0));
    assert_eq!(call["attempts"], json!(1));
    // NO DEADLINE ⇒ `null` in RAM — python's row carries the key the same way.
    assert_eq!(call["timeout"], Value::Null);
    assert!(crate::yaml_io::row_to_doc(call).contains("\ntimeout: null\n"));
    assert_eq!(call["kind"], json!("call"));
    assert_eq!(call["verb"], json!("work"));
    assert_eq!(
        call["parent"],
        Value::Null,
        "no dispatch was nested around it"
    );
    assert_eq!(
        call["caller"],
        Value::Null,
        "an operator send has no origin"
    );
    assert_eq!(call["name"], json!("named-one"));
    assert_eq!(call["id"].as_i64(), out.pid.map(crate::TaskId::get));
    // The stamp is NESTED under `outcome`, and completed carries status alone.
    assert_eq!(call["outcome"], json!({"status": "completed"}));
}

#[tokio::test]
async fn an_unnamed_kernel_reflect_writes_no_row() {
    // C88: python's UNNAMED `kernel` reflect fast path (`_agent.py:846`) — a
    // pure identity read with no ledger entry, no call row, no task-table
    // row, and the outcome's pid is None.
    let (kernel, _worker) = c36_kernel();
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0);
    assert!(
        out.pid.is_none(),
        "the fast path never touches the task table: {out:?}"
    );
    assert_eq!(out.status, Status::Completed);
    assert_eq!(out.result["id"], json!("kernel_state"));
    let root_rows = crate::test_support::rows_about(&kernel, &AgentId::from("kernel_state"));
    assert!(
        root_rows.is_empty(),
        "no call row on the root: {root_rows:?}"
    );
}

#[tokio::test]
async fn a_named_kernel_reflect_still_opens_the_ledger() {
    // Contrast: an `id` in the payload (named or joining) still opens the
    // ledger (`_agent.py:816`) — ONLY the unnamed path skips it.
    let (kernel, _worker) = c36_kernel();
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect"}),
        )
        .with_id("named-reflect")])
        .await
        .remove(0);
    assert!(
        out.pid.is_some(),
        "a named reflect still mints an id: {out:?}"
    );
    let root_rows = crate::test_support::rows_about(&kernel, &AgentId::from("kernel_state"));
    let calls: Vec<_> = root_rows
        .iter()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("call"))
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "a named reflect still writes a call row: {root_rows:?}"
    );
}

#[tokio::test]
async fn an_errored_call_row_stamps_the_python_error_and_reason() {
    let (kernel, worker) = c36_kernel();
    kernel
        .send(vec![Item::new(worker.id.clone(), json!({"type": "nope"}))])
        .await;
    let call = crate::test_support::rows_about(&kernel, &worker.id).remove(0);
    let stamp = &call["outcome"];
    assert_eq!(stamp["status"], json!("errored"));
    assert_eq!(stamp["reason"], json!("not_found"));
    // 200 CODE POINTS of `str(error)`, not 200 bytes: the text is cyrillic.
    assert_eq!(stamp["error"].as_str().unwrap().chars().count(), 200);
    assert_eq!(
        stamp.as_object().unwrap().len(),
        3,
        "nothing else is copied"
    );
}

#[tokio::test]
async fn a_falsy_non_string_error_settles_completed() {
    // C89: python `ERRORED if isinstance(reply, dict) and reply.get("error")
    // else COMPLETED` (`_agent.py:1051-1052`) — FULL truthiness, not "the key
    // is present". `0`, `0.0`, `[]`, `{}` and `""` are all falsy.
    let (kernel, worker) = c36_kernel();
    for falsy in [
        json!(0),
        json!(0.0),
        json!([]),
        json!({}),
        json!(""),
        json!(false),
        Value::Null,
    ] {
        let out = kernel
            .send(vec![Item::new(
                worker.id.clone(),
                json!({"type": "echo_error", "error": falsy}),
            )])
            .await
            .remove(0);
        assert_eq!(
            out.status,
            Status::Completed,
            "a falsy error must settle completed: {falsy:?}"
        );
    }
    // The contrast case: a TRUTHY non-string/non-bool error still errors.
    for truthy in [json!(1), json!(-1), json!([1]), json!({"k": 1})] {
        let out = kernel
            .send(vec![Item::new(
                worker.id.clone(),
                json!({"type": "echo_error", "error": truthy}),
            )])
            .await
            .remove(0);
        assert_eq!(
            out.status,
            Status::Errored,
            "a truthy error must still settle errored: {truthy:?}"
        );
    }
}

/// THE SETTLE ROW keeps what `stored_outcome` keeps — an UNNAMED call's
/// `{status, bytes}` (its body went to its caller; python's growth fix, §3) —
/// and the final attempts. The live call row keeps the python STAMP.
#[tokio::test]
async fn the_settle_row_keeps_what_stored_outcome_keeps() {
    let (kernel, worker) = c36_kernel();
    kernel
        .send(vec![Item::new(worker.id.clone(), json!({"type": "work"}))])
        .await;
    let settles: Vec<_> = crate::test_support::rows_about(&kernel, &AgentId::from("kernel_state"))
        .into_iter()
        .filter(|r| r.get("kind") == Some(&json!("settle")))
        .collect();
    assert_eq!(settles.len(), 1);
    let s = &settles[0];
    assert_eq!(s["settled"], json!("w"), "the agent the call was on");
    assert_eq!(s["outcome"], json!({"status": "completed", "bytes": 12}));
    assert_eq!(s["attempts"], json!(1));
    assert_eq!(
        s.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["kind", "parent", "settled", "outcome", "attempts", "id", "agent", "ts"],
        "python's key order"
    );
    let o = s["outcome"].as_object().unwrap();
    assert!(
        !o.contains_key("pid"),
        "the pid is the parent id, not a second copy: {o:?}"
    );
    // The LIVE call row keeps the digest STAMP, which `messages` and the
    // `ledger` verb read off it.
    let call = kernel
        .rows
        .get_one(s["parent"].as_i64().unwrap())
        .expect("the call row");
    assert_eq!(call["outcome"], json!({"status": "completed"}));
}

#[tokio::test]
async fn a_refusal_that_opened_no_call_row_appends_no_settle() {
    // `pid is None` ⟺ nothing reached the task table; python's `_stamp_call`
    // returns on the same test, so no ending is written for a beginning that
    // was never written down.
    let (kernel, _worker) = c36_kernel();
    kernel
        .send(vec![Item::new(
            AgentId::from("ghost"),
            json!({"type": "work"}),
        )])
        .await;
    assert!(
        crate::test_support::rows_about(&kernel, &AgentId::from("kernel_state"))
            .iter()
            .all(|r| r.get("kind") != Some(&json!("settle")))
    );
}

#[tokio::test]
async fn a_nested_send_names_its_caller_as_parent() {
    let (kernel, worker) = c36_kernel();
    let inner = kernel.clone();
    let wid = worker.id.clone();
    let outer = kernel
        .send(vec![Item::new(wid.clone(), json!({"type": "work"}))])
        .await
        .remove(0);
    // A send issued INSIDE that dispatch's task would carry its pid; here we
    // assert the top-level case reads null and the id space is one counter.
    assert!(outer.pid.is_some());
    let rows = crate::test_support::rows_about(&inner, &wid);
    assert_eq!(rows[0]["parent"], Value::Null);
}

// ─── C28 / C29 / C76 / C77 / C78 / C86 — the fanout contract ─────────

#[tokio::test]
async fn a_registered_agent_without_a_reader_cannot_be_a_watcher() {
    // C28/C29: being in the registry is not being a READER. `watch` refuses,
    // with python's text, hint, `watcher` and `src` — it does not invent a
    // consumer on the caller's behalf.
    let kernel = Arc::new(Kernel::new());
    kernel.register(make_agent("src"));
    kernel.register(make_agent("ghost"));
    let denial = kernel
        .watch(&AgentId::from("src"), AgentId::from("ghost"))
        .await
        .expect("a bare registered agent is not a reader");
    assert_eq!(
        denial["error"],
        json!(
            "watch: 'ghost' has no registered inbox, so nothing would read \
             what is mirrored there"
        )
    );
    assert_eq!(denial["reason"], json!("no_consumer"));
    assert_eq!(
        denial["hint"],
        json!("register the reader first — ctx.ensure_inbox('ghost') — then watch")
    );
    assert_eq!(denial["watcher"], json!("ghost"));
    assert_eq!(denial["src"], json!("src"));
    assert!(
        !kernel
            .agents
            .get(&AgentId::from("src"))
            .expect("src")
            .watcher_ids
            .read()
            .await
            .contains(&AgentId::from("ghost")),
        "a refused watch registered the mirror anyway"
    );
}

/// A bundle that reports what its own inbox already held when it ran.
struct PeekInbox;
#[async_trait::async_trait]
impl crate::bundle::Bundle for PeekInbox {
    fn name(&self) -> &str {
        "peek"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        _payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        let mut rx = kernel.ensure_inbox(agent_id);
        let mut seen = Vec::new();
        while let Some(item) = rx.try_recv() {
            seen.push(item.payload);
        }
        Ok(Some(json!({ "inbox_at_entry": seen })))
    }
}

fn peek_kernel() -> (Arc<Kernel>, Arc<Agent>) {
    let mut kernel = Kernel::new();
    let mut reg = crate::bundle::BundleRegistry::new();
    reg.register("peek.tools", PeekInbox);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let agent = Agent::new(
        AgentId::from("peeker"),
        Some("peek.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("peeker"),
        true,
    );
    kernel.register(Arc::clone(&agent));
    (kernel, agent)
}

#[tokio::test]
async fn send_fans_out_before_the_handler_runs() {
    // C76: the mirror happens at DISPATCH START, not after the reply — so a
    // handler already sees the request that woke it, and a watcher's echo
    // precedes every event the handler goes on to emit.
    let (kernel, agent) = peek_kernel();
    let _rx = kernel.ensure_inbox(&agent.id);
    let out = kernel
        .send(vec![Item::new(
            agent.id.clone(),
            json!({"type": "work", "n": 7}),
        )])
        .await
        .remove(0);
    assert_eq!(
        out.result["inbox_at_entry"],
        json!([{"type": "work", "n": 7}]),
        "the payload had not reached the inbox by the time the handler ran"
    );
}

#[tokio::test]
async fn send_payload_reaches_the_targets_own_registered_inbox() {
    // C76: the target's OWN inbox, not only its watchers'. A reader that
    // registered one is told what was sent TO it.
    let (kernel, agent) = peek_kernel();
    let mut rx = kernel.ensure_inbox(&agent.id);
    // Drain what the handler's own `ensure_inbox` read is racing for.
    let _ = kernel
        .send(vec![Item::new(agent.id.clone(), json!({"type": "work"}))])
        .await;
    // The handler drained it; send again with no handler read in between by
    // pointing at a bare agent (no bundle, `unhandled` reply — the fanout still
    // runs, because it runs BEFORE verb resolution).
    let bare = make_agent("bare");
    kernel.register(Arc::clone(&bare));
    let mut bare_rx = kernel.ensure_inbox(&bare.id);
    let _ = kernel
        .send(vec![Item::new(bare.id.clone(), json!({"type": "nope"}))])
        .await;
    assert_eq!(
        bare_rx.try_recv().expect("the target's own inbox").payload,
        json!({"type": "nope"}),
        "a refused verb still mirrors — the fanout precedes resolution"
    );
    assert!(rx.try_recv().is_none(), "the handler drained its own queue");
}

#[tokio::test]
async fn emit_to_a_non_agent_id_is_a_noop() {
    // C78: a synthetic id (a browser client `_ws_<hex>`) is not an agent. No
    // inbox put, no state event — python resolves the agent first and returns.
    let kernel = Arc::new(Kernel::new());
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let sink = Arc::clone(&seen);
    let _t = kernel.add_state_subscriber(Arc::new(move |ev: &Value, _b| {
        sink.lock().expect("sink poisoned").push(ev.clone());
    }));
    // A REGISTERED READER that is not an agent — the exact shape the WS proxy
    // holds for a browser tab.
    let mut rx = kernel.ensure_inbox(&AgentId::from("_ws_deadbeef"));
    kernel
        .emit(&AgentId::from("_ws_deadbeef"), json!({"type": "hi"}))
        .await;
    assert!(rx.try_recv().is_none(), "an emit to a non-agent id landed");
    assert!(
        seen.lock().expect("sink poisoned").is_empty(),
        "an emit to a non-agent id published a state event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_is_rowed_before_the_dispatch_settles_and_its_body_runs_under_it() {
    // WHAT THE KERNEL GUARANTEES about a fork, and only that:
    //
    // - `spawn_task` mints the fork's `task` row SYNCHRONOUSLY, inside the
    //   handler — so it precedes the dispatch's `settle` row, always;
    // - the forked BODY runs afterwards, on its own, and its rows follow the
    //   `task` row that names it.
    //
    // WHAT IT DOES NOT GUARANTEE, and this test therefore no longer asserts:
    // that the body's FIRST row lands before `settle`. This test used to (as
    // `a_forked_body_mints_before_the_dispatch_settles`), on the grounds that
    // python's FIFO ready queue runs the body before the settle resumes. Rust
    // has no such ordering: the body is a `tokio::spawn` and the dispatch
    // settles without waiting on it. On an idle runtime the body usually ran
    // first — tokio parks a fresh spawn in the spawning worker's LIFO slot and
    // runs it at that worker's next yield — which is why 50/50 passed alone.
    // Under load it did not (measured 2026-09-30: 8 of 40 full-crate runs,
    // mostly "the body had not run at all when `send` returned", once "body row
    // 4 after settle row 3"). Making python's order a GUARANTEE is a kernel
    // change (the owner's decision; see the `ai_paths` id-order note below),
    // not something a test can assume.
    //
    // The ai_paths id-order divergence measured on 2026-09-21 (`deliver_reply`
    // rows 2 ids late, `call send` rows 1 early) is a deeper chain (a spawned
    // body that itself spawns) and is open on the owner — a single
    // `yield_now()` at python's await point in `settle` fixed the
    // `deliver_reply` class and made the `call send` class worse (ai_to_ai
    // id/parent mismatches 71 -> 166), and one worker thread was worse still.
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    use tokio::sync::Notify;
    struct Forker {
        // the body says it ran — the test waits on THAT, never on time
        done: Arc<Notify>,
    }
    #[async_trait::async_trait]
    impl Bundle for Forker {
        fn name(&self) -> &str {
            "forker"
        }
        async fn handle(
            &self,
            a: &AgentId,
            _p: &Value,
            k: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            let k2 = Arc::clone(k);
            let a2 = a.clone();
            let done = Arc::clone(&self.done);
            let id = k.spawn_task(a, "body", async move {
                let mut row = Map::new();
                row.insert("kind".to_string(), Value::from("note"));
                let _ = k2.rows_put(&a2, row);
                // a stored permit: the wake is not lost if nobody waits yet
                done.notify_one();
            });
            Ok(Some(json!({ "task": id.0 })))
        }
    }
    let mine = AgentId::from("f");
    for _ in 0..50 {
        let done = Arc::new(Notify::new());
        let mut kernel = Kernel::new();
        let mut reg = BundleRegistry::new();
        reg.register(
            "forker.tools",
            Forker {
                done: Arc::clone(&done),
            },
        );
        kernel.bundles = reg;
        let kernel = Arc::new(kernel);
        let root = make_agent("kernel_state");
        kernel.register(Arc::clone(&root));
        kernel.set_root(Arc::clone(&root));
        kernel.register(Agent::new(
            mine.clone(),
            Some("forker.tools".to_string()),
            None,
            Map::new(),
            Path::new("/tmp/nowhere").join("f"),
            true,
        ));
        kernel
            .send(vec![Item::new(mine.clone(), json!({"type": "go"}))])
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(10), done.notified())
            .await
            .expect("the forked body ran");
        let find = |owner: &AgentId, kind: &str| -> Option<i64> {
            crate::test_support::rows_about(&kernel, owner)
                .into_iter()
                .find(|r| r.get("kind").and_then(Value::as_str) == Some(kind))
                .and_then(|r| r.get("id").and_then(Value::as_i64))
        };
        let task = find(&mine, "task").expect("the fork minted its task row");
        let settle = find(&root.id, "settle").expect("the dispatch wrote a settle row");
        let note = find(&mine, "note").expect("the forked body minted its row");
        assert!(
            task < settle,
            "the fork's task row {task} must precede the settle row {settle}"
        );
        assert!(
            task < note,
            "the body's row {note} must follow the task row {task} that names it"
        );
    }
}

#[tokio::test]
async fn a_watched_write_stream_reaches_the_watcher_with_its_body() {
    // C86: the binary channel's payload rides the inbox WHOLE. The watcher gets
    // the head with `bytes` nulled AND the raw body — which is what the WS drain
    // re-encodes into the binary frame python sends.
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    struct Sink;
    #[async_trait::async_trait]
    impl Bundle for Sink {
        fn name(&self) -> &str {
            "sink"
        }
        async fn handle(
            &self,
            _a: &AgentId,
            _p: &Value,
            _k: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            Ok(Some(json!({"ok": true})))
        }
        async fn handle_binary(
            &self,
            _a: &AgentId,
            _h: Value,
            body: Option<crate::outcome::Blob>,
            _k: &Arc<Kernel>,
        ) -> Result<(Reply, Option<crate::outcome::Blob>), BundleError> {
            let blob: Vec<u8> = body.map(|b| b.bytes).unwrap_or_default();
            Ok((Some(json!({"written": blob.len()})), None))
        }
    }
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("sink.tools", Sink);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let store = Agent::new(
        AgentId::from("store"),
        Some("sink.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("store"),
        true,
    );
    kernel.register(Arc::clone(&store));
    kernel.register(make_agent("tap"));
    let mut rx = kernel.ensure_inbox(&AgentId::from("tap"));
    assert!(kernel
        .watch(&store.id, AgentId::from("tap"))
        .await
        .is_none());

    let (reply, _body) = send_bin(
        &kernel,
        &store.id,
        json!({"type": "write_stream", "path": "a.bin"}),
        vec![1, 2, 3, 4, 5],
    )
    .await;
    assert_eq!(reply["written"], 5);

    let got = rx.try_recv().expect("the watcher was mirrored");
    assert_eq!(got.payload["type"], "write_stream");
    assert_eq!(got.payload["path"], "a.bin");
    assert_eq!(
        got.payload["bytes"],
        Value::Null,
        "the head carries null where the body was lifted out"
    );
    assert_eq!(got.body, Some(Blob::key("bytes", vec![1, 2, 3, 4, 5])));
}

/// A BODY RIDES AT ITS PATH of the payload — where python's dict holds the
/// bytes (`data` for `paste_image`, `bytes` for `write_stream`). The handler gets
/// it at that path and a header with NOTHING there; a watcher gets it at that
/// path with `null` in python's position; the call row records `{len}` there.
/// A path that names no slot of the payload is refused before anything runs.
#[tokio::test]
async fn a_binary_body_rides_at_its_payload_path() {
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    struct Echo;
    #[async_trait::async_trait]
    impl Bundle for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        async fn handle(
            &self,
            _a: &AgentId,
            _p: &Value,
            _k: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            Ok(Some(json!({"channel": "text"})))
        }
        async fn handle_binary(
            &self,
            _a: &AgentId,
            header: Value,
            body: Option<crate::outcome::Blob>,
            _k: &Arc<Kernel>,
        ) -> Result<(Reply, Option<crate::outcome::Blob>), BundleError> {
            let data = crate::outcome::body_at(body.as_ref(), "data").map(<[u8]>::len);
            let bytes = crate::outcome::body_at(body.as_ref(), "bytes").map(<[u8]>::len);
            Ok((
                Some(json!({"header": header, "data": data, "bytes": bytes})),
                None,
            ))
        }
    }
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("echo.tools", Echo);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let echo = Agent::new(
        AgentId::from("echo"),
        Some("echo.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("echo"),
        true,
    );
    kernel.register(Arc::clone(&echo));
    kernel.register(make_agent("tap"));
    let mut rx = kernel.ensure_inbox(&AgentId::from("tap"));
    assert!(kernel.watch(&echo.id, AgentId::from("tap")).await.is_none());

    // the caller names the slot (`data: null`) to keep python's key order
    let out = kernel
        .send(vec![Item::new(
            echo.id.clone(),
            json!({"type": "paste_image", "job_id": 1, "data": null, "mime": "image/png"}),
        )
        .with_binary_key("data", vec![9, 8, 7])])
        .await
        .remove(0);
    assert_eq!(
        out.result,
        json!({"header": {"type": "paste_image", "job_id": 1, "mime": "image/png"},
               "data": 3, "bytes": null}),
        "the handler reads its body at `data`, and nothing is there in its header"
    );
    let got = rx.try_recv().expect("the watcher was mirrored");
    assert_eq!(
        serde_json::to_string(&got.payload).unwrap(),
        r#"{"type":"paste_image","job_id":1,"data":null,"mime":"image/png"}"#
    );
    assert_eq!(got.body, Some(Blob::key("data", vec![9, 8, 7])));
    let row = kernel
        .rows
        .all()
        .into_iter()
        .find(|r| r.get("verb") == Some(&json!("paste_image")))
        .expect("the call row");
    assert_eq!(
        serde_json::to_string(&row["payload"]).unwrap(),
        r#"{"type":"paste_image","job_id":1,"data":{"len":3},"mime":"image/png"}"#
    );

    // a NESTED path: the slot inside the payload, the leaf gone from the header
    let out = kernel
        .send(vec![Item::new(
            echo.id.clone(),
            json!({"type": "take", "meta": {"k": 1}}),
        )
        .with_binary(
            vec![Step::from("meta"), Step::from("blob")],
            vec![1],
        )])
        .await
        .remove(0);
    assert_eq!(
        out.result["header"],
        json!({"type": "take", "meta": {"k": 1}})
    );
    assert_eq!(out.result["data"], Value::Null);
    let got = rx.try_recv().expect("mirrored");
    assert_eq!(got.payload["meta"], json!({"k": 1, "blob": null}));
    assert_eq!(
        got.body,
        Some(Blob::new(
            vec![Step::from("meta"), Step::from("blob")],
            vec![1]
        ))
    );

    // a path that names no slot of the payload is refused, nothing runs
    let out = kernel
        .send(vec![Item::new(echo.id.clone(), json!({"type": "take"}))
            .with_binary(
                vec![Step::from("nope"), Step::from("deep")],
                vec![1],
            )])
        .await
        .remove(0);
    assert_eq!(out.pid, None);
    assert_eq!(out.result["reason"], json!("bad_arg"), "{}", out.result);
    assert_eq!(out.result["arg"], json!("binary"));
    assert!(rx.try_recv().is_none(), "a refused item mirrors nothing");
}

/// A body bound to ONE LITERAL KEY (`Item::with_binary_key` — the REST octet
/// leg's `X-Fantastic-Bytes-Field`, python's `payload[field] = body`): `a.b` is
/// the key `"a.b"`, never `payload["a"]["b"]`. The handler gets it at that key
/// with a nested `a.b` left as sent; the watcher and the call row hold the key
/// where python's dict does; an envelope with no `a` is no refusal.
#[tokio::test]
async fn a_literal_key_body_is_one_top_level_key() {
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    struct Echo;
    #[async_trait::async_trait]
    impl Bundle for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        async fn handle(
            &self,
            _a: &AgentId,
            _p: &Value,
            _k: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            Ok(Some(json!({"channel": "text"})))
        }
        async fn handle_binary(
            &self,
            _a: &AgentId,
            header: Value,
            body: Option<crate::outcome::Blob>,
            _k: &Arc<Kernel>,
        ) -> Result<(Reply, Option<crate::outcome::Blob>), BundleError> {
            let at = crate::outcome::body_at(body.as_ref(), "a.b").map(<[u8]>::len);
            Ok((Some(json!({"header": header, "a.b": at})), None))
        }
    }
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("echo.tools", Echo);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let echo = Agent::new(
        AgentId::from("echo"),
        Some("echo.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("echo"),
        true,
    );
    kernel.register(Arc::clone(&echo));
    kernel.register(make_agent("tap"));
    let mut rx = kernel.ensure_inbox(&AgentId::from("tap"));
    assert!(kernel.watch(&echo.id, AgentId::from("tap")).await.is_none());

    let out = kernel
        .send(vec![Item::new(
            echo.id.clone(),
            json!({"type": "take", "a": {"b": "kept"}}),
        )
        .with_binary_key("a.b", vec![1, 2])])
        .await
        .remove(0);
    assert_eq!(
        out.result,
        json!({"header": {"type": "take", "a": {"b": "kept"}}, "a.b": 2})
    );
    let got = rx.try_recv().expect("mirrored");
    assert_eq!(
        serde_json::to_string(&got.payload).unwrap(),
        r#"{"type":"take","a":{"b":"kept"},"a.b":null}"#
    );
    // the watcher's body names the ONE key, never the nested `a` → `b`
    assert_eq!(got.body, Some(Blob::key("a.b", vec![1, 2])));
    assert_eq!(
        got.body
            .as_ref()
            .map(|b| fantastic_bundle::path::steps_value(&b.path)),
        Some(json!(["a.b"]))
    );
    let row = kernel
        .rows
        .all()
        .into_iter()
        .find(|r| r.get("verb") == Some(&json!("take")))
        .expect("the call row");
    assert_eq!(
        serde_json::to_string(&row["payload"]).unwrap(),
        r#"{"type":"take","a":{"b":"kept"},"a.b":{"len":2}}"#
    );

    // no `a` in the payload: the key is literal, so nothing is missing
    let out = kernel
        .send(vec![
            Item::new(echo.id.clone(), json!({"type": "take"})).with_binary_key("a.b", vec![1])
        ])
        .await
        .remove(0);
    assert_eq!(out.status, Status::Completed, "{}", out.result);
    assert_eq!(out.result, json!({"header": {"type": "take"}, "a.b": 1}));
}

#[tokio::test]
async fn binary_dispatch_runs_the_same_pre_bundle_path_as_text() {
    // python's `handle` is ONE path: the root answers `reflect` with the
    // substrate identity whether the payload carried bytes or not. The binary
    // channel used to go straight to the bundle, so an octet `reflect` on the
    // ROOT described the loader instead of the kernel.
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    struct Loader;
    #[async_trait::async_trait]
    impl Bundle for Loader {
        fn name(&self) -> &str {
            "loader"
        }
        async fn handle(
            &self,
            _a: &AgentId,
            _p: &Value,
            _k: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            Ok(Some(
                json!({"sentence": "Disk persistence/hydration root."}),
            ))
        }
    }
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("loader.tools", Loader);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some("loader.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("kernel_state"),
        true,
    );
    kernel.register(Arc::clone(&root));

    let (reply, _) = send_bin(&kernel, &root.id, json!({"type": "reflect"}), vec![b'x']).await;
    assert_eq!(
        reply["sentence"],
        "Fantastic kernel. Everything is reachable by sending messages to agents.",
        "the root's own identity, not the bundle's: {reply}"
    );
    // A bundle verb still routes to the bundle on this channel.
    let (reply, _) = send_bin(&kernel, &root.id, json!({"type": "load_tree"}), vec![]).await;
    assert_eq!(reply["sentence"], "Disk persistence/hydration root.");
}

#[tokio::test]
async fn state_summary_is_python_json_dumps() {
    // C77: `json.dumps(shrunk, ensure_ascii=False)` — `, `/`: ` separators, raw
    // non-ASCII, bytes as `<bytes:N>`, never truncated.
    let kernel = Arc::new(Kernel::new());
    kernel.register(make_agent("tgt"));
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let sink = Arc::clone(&seen);
    let _t = kernel.add_state_subscriber(Arc::new(move |ev: &Value, _b| {
        sink.lock().expect("sink poisoned").push(ev.clone());
    }));
    kernel
        .emit(
            &AgentId::from("tgt"),
            json!({"type": "say", "text": "héllo"}),
        )
        .await;
    let events = seen.lock().expect("sink poisoned").clone();
    let emit = events
        .iter()
        .find(|e| e["kind"] == json!("emit"))
        .expect("an emit event");
    assert_eq!(
        emit["summary"],
        json!(r#"{"type": "say", "text": "héllo"}"#),
        "python separators and raw non-ASCII"
    );
}

#[tokio::test]
async fn a_bytes_payload_summarizes_as_bytes_n() {
    // C77/C86: the summary shrinks a body to `<bytes:N>` at the path it rode.
    let s = summarize_payload(
        &json!({"type": "write_stream", "bytes": null}),
        Some(&Blob::key("bytes", vec![0u8; 12])),
    );
    assert_eq!(s, r#"{"type": "write_stream", "bytes": "<bytes:12>"}"#);
}

/// The marker sits at the body's LITERAL key, as python's `_shrink` writes it in
/// the dict: `a.b` is the key `"a.b"`, its nested twin `a` → `b` untouched. A
/// nested and a list slot are their own steps.
#[tokio::test]
async fn a_literal_dotted_key_summarizes_at_that_key() {
    let s = summarize_payload(
        &json!({"type": "take", "a": {"b": "kept"}, "a.b": null}),
        Some(&Blob::key("a.b", vec![0u8; 3])),
    );
    assert_eq!(
        s,
        r#"{"type": "take", "a": {"b": "kept"}, "a.b": "<bytes:3>"}"#
    );
    let s = summarize_payload(
        &json!({"type": "take", "items": [{"n": 0}, {"data": null}]}),
        Some(&Blob::new(
            vec![Step::from("items"), Step::Index(1), Step::from("data")],
            vec![0u8; 2],
        )),
    );
    assert_eq!(
        s,
        r#"{"type": "take", "items": [{"n": 0}, {"data": "<bytes:2>"}]}"#
    );
}

// ─── C2 + C4 — the kernel_verbs map is python's `_SYSTEM_VERB_DOCS` ─────────

/// python `_SYSTEM_VERB_DOCS`, in declaration order, verbatim
/// (`P/kernel/_agent.py`).
const PY_KERNEL_VERB_DOCS: [(&str, &str); 12] = [
    ("create_agent", "args: handler_module:str, parent_id:str REQUIRED, id:str?, **meta. Creates a CHILD OF `parent_id` — any agent you name (your own id grows your own subtree); 'kernel_state' for top-level. No default parent."),
    ("update_agent", "args: id:str, **meta. EVERY FIELD YOU NAME IS REPLACED, NOT MERGED (a field you do not name is untouched), so a dict field must be sent WHOLE — `can_call`/`callable_by` included: restate every key you want to keep, the baseline `$self` among them, or the keys you omit are GONE. Granting one edge reads like a PATCH and behaves like a PUT. Whoever holds update_agent on the root updates any agent, itself included."),
    ("delete_agent", "args: id:str. CASCADES — deletes that agent AND ITS WHOLE SUBTREE, deepest first; refuses if any descendant carries `delete_lock`. Whoever holds delete_agent on the root deletes any agent."),
    ("list_agents", "No args. Every agent's record, flat. Paged."),
    ("ps", "No args. The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, verb, started_at}]}, one row per in-flight dispatch. Paged."),
    ("ledger", "No args. EVERY execution and what it became — named under its id, unnamed under its minted pid — {executions:[{id, target, status, attempts, retries, named, outcome?, acked?}]}, paged, scoped to what YOU originated; read from the kernel's memory store plus what is not yet flushed. `outcome` keeps the RESULT only for a named call; a dead letter keeps its error; anything else is {status, bytes} — the reply itself went to its caller. Failed/timed_out/cancelled rows are the dead-letter queue (announced by the `on_deadletter` event): inspect, retry with a NEW id if you choose, then `ack_deadletter` it — nothing is retried silently."),
    ("ack_deadletter", "args: id:int|str (req — an execution id, or its name). Acknowledges a DEAD LETTER (failed/timed_out/cancelled) you originated: recorded in the kernel's store, it may then be pruned. Returns {acked:true, id}."),
    ("kernel_start", "No args. THE START SEQUENCE — the root must name `memory_store_id` (fail loud otherwise): boots the root, then that store, starts flushing the kernel's execution rows into it, then boots every other agent in registration order. Idempotent. Returns {started, store, booted:[ids]}."),
    ("cancel", "args: task_id:str. Kills that task AND ITS SUBTREE — the id names its own agent. Returns {cancelled:bool, killed:[…]}; the blocked caller's batch item settles as status:'cancelled'. Unknown id ⇒ {cancelled:false}."),
    ("shutdown_agents", "No args. Stops the AGENTS, not the process: drains (new roots refused), sends `shutdown` to every agent advertising it, waits for in-flight work, cancels the rest. Returns {drained, shut:[ids], cancelled:[ids]}."),
    ("kernel_stop", "No args. Stops the kernel PROCESS gracefully — drain, every call settles (done, or cancelled into the dead letters), the last rows flush, agents shut down, exit. Under env:'container', the container."),
    ("docs", "args: of:str (req — an agent id (`<guid>|<agent id>` on a connected kernel), a bundle, or 'kernel'), tier:'oneliners'|'detailed'?, verb:str?. HOW TO CALL an agent — its bundle's verbs: `oneliners` (default) is one signature line per verb, `detailed` the full text (no `verb`: plus emits, cancel and notes). Pull once per bundle; `detailed` for a verb you are about to use. Gated like every kernel verb: a `can_call` grant of `docs` on the root. Returns {of, bundle, tier, verbs, emits?, cancel?, notes?}."),
];

/// python `kernel/_docs.signature` of each `_SYSTEM_VERB_DOCS` entry.
const PY_SIGNATURES: [(&str, &str); 12] = [
    ("create_agent", "args: handler_module:str, parent_id:str REQUIRED, id:str?, **meta"),
    ("update_agent", "args: id:str, **meta"),
    ("delete_agent", "args: id:str"),
    ("list_agents", "No args"),
    ("ps", "No args"),
    ("ledger", "No args"),
    ("ack_deadletter", "args: id:int|str (req — an execution id, or its name)"),
    ("kernel_start", "No args"),
    ("cancel", "args: task_id:str"),
    ("shutdown_agents", "No args"),
    ("kernel_stop", "No args"),
    ("docs", "args: of:str (req — an agent id (`<guid>|<agent id>` on a connected kernel), a bundle, or 'kernel'), tier:'oneliners'|'detailed'?, verb:str?"),
];

#[test]
fn kernel_verb_docs_match_python_verbatim() {
    let docs = kernel_verb_docs();
    let obj = docs.as_object().expect("an object");
    assert_eq!(obj.len(), PY_KERNEL_VERB_DOCS.len(), "{docs}");
    for (verb, text) in PY_KERNEL_VERB_DOCS {
        assert_eq!(obj[verb], json!(text), "{verb}");
    }
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    let want: Vec<&str> = PY_KERNEL_VERB_DOCS.iter().map(|(v, _)| *v).collect();
    assert_eq!(keys, want, "python declaration order");
}

#[test]
fn docs_signatures_are_pythons() {
    for ((verb, text), (sverb, sig)) in PY_KERNEL_VERB_DOCS.iter().zip(PY_SIGNATURES.iter()) {
        assert_eq!(verb, sverb);
        assert_eq!(crate::docs::signature(text), *sig, "{verb}");
    }
    assert_eq!(
        crate::docs::signature("args: q:str (req — e.g. a word), n:int? (default 5). Finds q."),
        "args: q:str (req — e.g. a word), n:int? (default 5)"
    );
    assert_eq!(
        crate::docs::signature("Start the loop unless `watch=false`\n(a loader). More.\n\nX."),
        "Start the loop unless `watch=false` (a loader)"
    );
    assert_eq!(crate::docs::signature("No args."), "No args");
}

// ─── STEP-4 PINS (2026-10-01): one dispatch per channel ──────────────────────
//
// The text and binary dispatch paths carry the same prologue (kernel-verb
// root-only check, root reflect, bare-agent answers, not-installed refusal,
// bundle error → raise). These pin what the BINARY leg answers at every branch,
// against the TEXT leg's answer for the same payload, so merging the two
// dispatches cannot change either.

/// A root (`kernel_state`, bare) and one bare child under it.
fn root_and_child() -> Arc<Kernel> {
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = Agent::new(
        AgentId::from("kid"),
        None,
        Some(AgentId::from("kernel_state")),
        Map::new(),
        Path::new("/tmp/nowhere").join("kid"),
        true,
    );
    kernel.register(child);
    kernel
}

async fn send_text(kernel: &Arc<Kernel>, target: &str, payload: Value) -> crate::outcome::Outcome {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

async fn send_binary(
    kernel: &Arc<Kernel>,
    target: &str,
    payload: Value,
    blob: Vec<u8>,
) -> crate::outcome::Outcome {
    kernel
        .send(vec![
            Item::new(AgentId::from(target), payload).with_binary_key("bytes", blob)
        ])
        .await
        .remove(0)
}

#[tokio::test]
async fn a_binary_kernel_verb_to_a_child_is_not_here_exactly_as_text() {
    let kernel = root_and_child();
    for verb in ["create_agent", "list_agents", "ps", "ledger"] {
        let bin = send_binary(&kernel, "kid", json!({"type": verb}), vec![1, 2]).await;
        let txt = send_text(&kernel, "kid", json!({"type": verb})).await;
        let want = format!(
            "{{\"error\":\"{verb} is a kernel verb — kid does not answer it\",\
             \"reason\":\"not_here\",\
             \"hint\":\"address the kernel root ('kernel'); name the agent you mean in \
             `parent_id` / `id`\",\"verb\":\"{verb}\"}}"
        );
        assert_eq!(serde_json::to_string(&bin.result).unwrap(), want, "{verb}");
        assert_eq!(bin.result, txt.result, "{verb}: binary == text");
        assert_eq!(bin.status, txt.status, "{verb}");
        assert_eq!(bin.body, None, "{verb}: a refusal carries no body");
    }
}

#[tokio::test]
async fn a_binary_kernel_verb_on_the_root_is_answered_as_text() {
    let kernel = root_and_child();
    let bin = send_binary(
        &kernel,
        "kernel_state",
        json!({"type": "list_agents"}),
        vec![9],
    )
    .await;
    let txt = send_text(&kernel, "kernel_state", json!({"type": "list_agents"})).await;
    assert_eq!(
        bin.status,
        crate::outcome::Status::Completed,
        "{}",
        bin.result
    );
    assert_eq!(
        bin.result, txt.result,
        "the root answers the kernel verb on both legs"
    );
    let ids: Vec<&str> = bin.result["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["kernel_state", "kid"]);
    assert_eq!(bin.body, None, "a kernel verb returns no chunk");
    // the `kernel` alias resolves to the root on the binary leg too
    let alias = send_binary(&kernel, "kernel", json!({"type": "list_agents"}), vec![9]).await;
    assert_eq!(alias.result, txt.result);
}

#[tokio::test]
async fn a_binary_reflect_or_lifecycle_verb_on_a_bare_agent_answers_as_text() {
    let kernel = root_and_child();
    let bin = send_binary(&kernel, "kid", json!({"type": "reflect"}), vec![1]).await;
    let txt = send_text(&kernel, "kid", json!({"type": "reflect"})).await;
    assert_eq!(bin.status, crate::outcome::Status::Completed);
    assert_eq!(bin.result, txt.result, "bare-agent reflect: binary == text");
    assert_eq!(bin.result["id"], "kid");
    assert_eq!(bin.body, None);
    for verb in ["boot", "shutdown"] {
        let bin = send_binary(&kernel, "kid", json!({"type": verb}), vec![1]).await;
        let txt = send_text(&kernel, "kid", json!({"type": verb})).await;
        assert_eq!(bin.result, Value::Null, "{verb} on a bare agent is null");
        assert_eq!(txt.result, Value::Null, "{verb}");
        assert_eq!(bin.status, crate::outcome::Status::Completed, "{verb}");
        assert_eq!(bin.body, None, "{verb}");
    }
}

/// A bundle whose binary handler answers `Ok((None, body))` and whose text
/// handler answers `Ok(None)` or raises, by verb.
struct NoReply;
#[async_trait::async_trait]
impl crate::bundle::Bundle for NoReply {
    fn name(&self) -> &str {
        "no_reply"
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        Ok(None)
    }
    async fn handle_binary(
        &self,
        _agent_id: &AgentId,
        header: Value,
        body: Option<crate::outcome::Blob>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(crate::bundle::Reply, Option<crate::outcome::Blob>), crate::bundle::BundleError>
    {
        let blob: Vec<u8> = body.map(|b| b.bytes).unwrap_or_default();
        match verb_of(&header) {
            "raise" => Err(crate::bundle::BundleError::from("kaboom")),
            "empty" => Ok((None, None)),
            _ => Ok((None, Some(crate::outcome::Blob::key("bytes", blob)))),
        }
    }
}

fn kernel_with_no_reply() -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    let mut reg = crate::bundle::BundleRegistry::new();
    reg.register("no_reply.tools", NoReply);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    kernel.register(Agent::new(
        AgentId::from("nr"),
        Some("no_reply.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("nr"),
        true,
    ));
    kernel
}

#[tokio::test]
async fn a_binary_handler_with_no_reply_settles_null_and_keeps_its_body() {
    let kernel = kernel_with_no_reply();
    let out = send_binary(&kernel, "nr", json!({"type": "echo"}), vec![4, 5, 6]).await;
    assert_eq!(out.status, crate::outcome::Status::Completed);
    assert_eq!(out.result, Value::Null, "Ok(None) is a null reply");
    assert_eq!(
        out.body,
        Some(Blob::key("bytes", vec![4, 5, 6])),
        "the body still rides back"
    );
    let out = send_binary(&kernel, "nr", json!({"type": "empty"}), vec![4]).await;
    assert_eq!(out.result, Value::Null);
    assert_eq!(out.body, None, "no chunk at all stays no chunk");
    let txt = send_text(&kernel, "nr", json!({"type": "echo"})).await;
    assert_eq!(
        txt.result,
        Value::Null,
        "the text leg's Ok(None) is null too"
    );
    assert_eq!(txt.status, crate::outcome::Status::Completed);
}

#[tokio::test]
async fn a_bundle_error_raises_one_shape_with_or_without_a_request_body() {
    // ONE dispatch (`handle_binary`) — a call with a body and a plain call reach
    // the same handler and settle the same raise.
    let kernel = kernel_with_no_reply();
    let bin = send_binary(&kernel, "nr", json!({"type": "raise"}), vec![1]).await;
    assert_eq!(bin.status, crate::outcome::Status::Failed);
    assert_eq!(
        serde_json::to_string(&bin.result).unwrap(),
        r#"{"error":"kaboom","raised":true}"#
    );
    assert_eq!(bin.body, None);
    let txt = send_text(&kernel, "nr", json!({"type": "raise"})).await;
    assert_eq!(txt.status, crate::outcome::Status::Failed);
    assert_eq!(
        serde_json::to_string(&txt.result).unwrap(),
        r#"{"error":"kaboom","raised":true}"#
    );
    assert_eq!(txt.body, None);
}

#[tokio::test]
async fn binary_refusals_for_bare_and_uninstalled_agents_are_byte_identical_to_text() {
    let kernel = Arc::new(Kernel::new());
    kernel.register(make_agent("bare2"));
    kernel.register(Agent::new(
        AgentId::from("orphan2"),
        Some("nope_state.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("orphan2"),
        true,
    ));
    for (id, want) in [
        (
            "bare2",
            r#"{"error":"agent 'bare2' has no handler_module; cannot answer verb 'ingest'","reason":"unhandled","verb":"ingest"}"#,
        ),
        (
            "orphan2",
            r#"{"error":"agent 'orphan2': handler_module 'nope_state.tools' cannot be imported in this runtime: ModuleNotFoundError: No module named 'nope_state'","reason":"unhandled","verb":"ingest"}"#,
        ),
    ] {
        let bin = send_binary(&kernel, id, json!({"type": "ingest"}), vec![1]).await;
        let txt = send_text(&kernel, id, json!({"type": "ingest"})).await;
        assert_eq!(serde_json::to_string(&bin.result).unwrap(), want, "{id}");
        assert_eq!(bin.result, txt.result, "{id}");
        assert_eq!(bin.status, txt.status, "{id}");
    }
}

// ─── STEP-4 PINS: the call context at every hop (CallCtx) ───────────────────

/// Records, per handler run, what the call context says, and forwards along
/// `next` — every forward the handler's own send, so gated as that agent.
#[derive(Default)]
struct HopLog(std::sync::Mutex<Vec<Value>>);
struct Hop(Arc<HopLog>);
#[async_trait::async_trait]
impl crate::bundle::Bundle for Hop {
    fn name(&self) -> &str {
        "hop"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        if verb_of(payload) != "hop" {
            return Ok(None);
        }
        self.0 .0.lock().unwrap().push(json!({
            "at": agent_id.0,
            "sender": current_sender().map(|a| a.0),
            "origin": current_origin().map(|a| a.0),
            "in_task": current_task().is_some(),
        }));
        let mut rest: Vec<Value> = payload
            .get("next")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if rest.is_empty() {
            return Ok(Some(json!({"at": agent_id.0})));
        }
        let next = rest.remove(0);
        let item = Item::new(
            AgentId::from(next.as_str().unwrap()),
            json!({"type": "hop", "next": rest}),
        );
        let out = kernel.send(vec![item]).await;
        let o = out.into_iter().next().unwrap();
        Ok(Some(
            json!({"via": agent_id.0, "status": format!("{:?}", o.status), "got": o.result}),
        ))
    }
}

/// `ids` registered under `hop.tools`, each with the given explicit grants.
fn hop_kernel(agents: &[(&str, Value, Value)]) -> (Arc<Kernel>, Arc<HopLog>) {
    let log = Arc::new(HopLog::default());
    let mut kernel = Kernel::new();
    let mut reg = crate::bundle::BundleRegistry::new();
    reg.register("hop.tools", Hop(Arc::clone(&log)));
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    for (id, can_call, callable_by) in agents {
        let mut meta = Map::new();
        meta.insert("can_call".into(), can_call.clone());
        meta.insert("callable_by".into(), callable_by.clone());
        kernel.register(Agent::new(
            AgentId::from(*id),
            Some("hop.tools".to_string()),
            None,
            meta,
            Path::new("/tmp/nowhere").join(id),
            true,
        ));
    }
    (kernel, log)
}

fn hop_chain() -> (Arc<Kernel>, Arc<HopLog>) {
    hop_kernel(&[
        ("h_a", json!({"h_b": "*"}), json!({})),
        ("h_b", json!({"h_c": "*"}), json!({"h_a": "*"})),
        ("h_c", json!({}), json!({"h_b": "*"})),
        ("h_d", json!({}), json!({})),
    ])
}

#[tokio::test]
async fn sender_is_the_callee_and_origin_the_caller_at_every_hop() {
    let (kernel, log) = hop_chain();
    let out = send_text(
        &kernel,
        "h_a",
        json!({"type": "hop", "next": ["h_b", "h_c"]}),
    )
    .await;
    assert_eq!(
        out.status,
        crate::outcome::Status::Completed,
        "{}",
        out.result
    );
    assert_eq!(
        out.result,
        json!({"via": "h_a", "status": "Completed", "got":
               {"via": "h_b", "status": "Completed", "got": {"at": "h_c"}}})
    );
    let seen = log.0.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            json!({"at": "h_a", "sender": "h_a", "origin": null, "in_task": true}),
            json!({"at": "h_b", "sender": "h_b", "origin": "h_a", "in_task": true}),
            json!({"at": "h_c", "sender": "h_c", "origin": "h_b", "in_task": true}),
        ]
    );
    // Outside every dispatch the context is empty again.
    assert_eq!(current_sender(), None);
    assert_eq!(current_origin(), None);
    assert_eq!(current_task(), None);
}

#[tokio::test]
async fn a_handler_cannot_send_as_anyone_but_itself() {
    // IDENTITY COMES FROM DISPATCH: h_a holds NO grant to h_d and h_d accepts
    // nobody — a nested send from h_a's handler is h_a's, and refused. There is
    // no switch to make it anyone else's.
    let (kernel, log) = hop_chain();
    let out = send_text(&kernel, "h_a", json!({"type": "hop", "next": ["h_d"]})).await;
    assert_eq!(out.result["status"], "Failed", "{}", out.result);
    assert_eq!(
        out.result["got"]["reason"], "unauthorized",
        "{}",
        out.result
    );
    assert_eq!(log.0.lock().unwrap().len(), 1, "h_d's handler never ran");
}

#[tokio::test]
async fn kernel_bookkeeping_runs_the_handler_as_the_agent_with_no_origin() {
    // The kernel's own call (`bookkeeping`) is not a send: no gate, no call row,
    // no task row. The handler runs AS ITS AGENT — so its own sends are gated as
    // that agent — and sees no origin: the kernel asked.
    let (kernel, log) = hop_chain();
    let h_d = kernel.resolve_target("h_d").unwrap();
    let rows_before = kernel.rows.all().len();
    let out = kernel
        .bookkeeping(&h_d, json!({"type": "hop", "next": []}), None)
        .await;
    assert_eq!(
        out.status,
        crate::outcome::Status::Completed,
        "{}",
        out.result
    );
    assert!(out.pid.is_none());
    assert_eq!(
        log.0.lock().unwrap().clone(),
        vec![json!({"at": "h_d", "sender": "h_d", "origin": null, "in_task": false})]
    );
    assert_eq!(kernel.rows.all().len(), rows_before, "no call row");
    // its nested send is h_d's own — h_d holds no grant to h_a: refused
    log.0.lock().unwrap().clear();
    let out = kernel
        .bookkeeping(&h_d, json!({"type": "hop", "next": ["h_a"]}), None)
        .await;
    assert_eq!(
        out.result["got"]["reason"], "unauthorized",
        "{}",
        out.result
    );
}

#[tokio::test]
async fn a_peer_address_is_an_outbound_key_only() {
    // A caller is always local — a call a bridge leg serves is the leg's — so a
    // `<guid>|<agent>` key in `callable_by` could never match: refused at the
    // write. In `can_call` it is an outbound peer address, accepted.
    let (kernel, _log) = hop_chain();
    let refused = send_text(
        &kernel,
        "kernel",
        json!({"type": "update_agent", "id": "h_d", "callable_by": {"g2|scout": "*"}}),
    )
    .await;
    assert_eq!(
        refused.result["reason"], "invalid_meta",
        "{}",
        refused.result
    );
    assert_eq!(
        refused.result["error"],
        "callable_by['g2|scout'] is not a grant key — legal keys: an agent id, $self, \
         $parent, $subtree"
    );
    let ok = send_text(
        &kernel,
        "kernel",
        json!({"type": "update_agent", "id": "h_d", "can_call": {"g2|scout": "*", "g3|*": "*"}}),
    )
    .await;
    assert_eq!(ok.result["updated"], true, "{}", ok.result);
}

#[tokio::test]
async fn while_draining_shutdown_traffic_and_live_work_pass_and_new_roots_do_not() {
    let (kernel, log) = hop_chain();
    let _ = kernel.shutdown_agents().await;
    assert!(kernel.draining());
    // a new root — operator or agent — is refused, coded, with python's text
    for sender in [None, Some("h_a")] {
        let fut = kernel.send(vec![Item::new(
            AgentId::from("h_b"),
            json!({"type": "hop"}),
        )]);
        let out = match sender {
            None => fut.await,
            Some(s) => as_agent(AgentId::from(s), fut).await,
        }
        .remove(0);
        assert_eq!(out.status, crate::outcome::Status::Failed);
        assert_eq!(
            serde_json::to_string(&out.result).unwrap(),
            r#"{"error":"send: the kernel is draining for shutdown — no new work is accepted; in-flight work may finish","reason":"draining"}"#,
            "{sender:?}"
        );
        assert!(out.pid.is_none());
    }
    assert!(log.0.lock().unwrap().is_empty(), "no handler ran");
    // the kernel's own shutdown traffic passes, AND a send from inside that
    // live work (h_a → h_b → h_c) passes too
    let out = with_shutdown(kernel.send(vec![Item::new(
        AgentId::from("h_a"),
        json!({"type": "hop", "next": ["h_b", "h_c"]}),
    )]))
    .await
    .remove(0);
    assert_eq!(
        out.status,
        crate::outcome::Status::Completed,
        "{}",
        out.result
    );
    assert_eq!(
        out.result["got"]["got"],
        json!({"at": "h_c"}),
        "every nested hop ran: {}",
        out.result
    );
    // the root's control surface stays open
    let out = send_text(&kernel, "kernel", json!({"type": "list_agents"})).await;
    assert_eq!(
        out.status,
        crate::outcome::Status::Completed,
        "{}",
        out.result
    );
    let out = send_text(&kernel, "kernel", json!({"type": "reflect"})).await;
    assert_eq!(out.status, crate::outcome::Status::Completed);
    // outside the scope the flag is clear again
    assert!(!in_shutdown());
}

// ─── STEP-4 PINS: `ack_deadletter` (kernel_verbs' `as_id` / `raised` users) ──

#[tokio::test]
async fn ack_deadletter_refusals_and_acks_are_byte_exact() {
    let kernel = kernel_with_no_reply();
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    let ack = |id: Value| {
        let kernel = Arc::clone(&kernel);
        async move {
            let r = send_text(
                &kernel,
                "kernel",
                json!({"type": "ack_deadletter", "id": id}),
            )
            .await;
            serde_json::to_string(&r.result).unwrap()
        }
    };
    // a FAILED execution (a raise) and a COMPLETED one, both named
    let failed = kernel
        .send(vec![Item::new(
            AgentId::from("nr"),
            json!({"type": "raise"}),
        )
        .with_id("dl_job")])
        .await
        .remove(0);
    assert_eq!(failed.status, crate::outcome::Status::Failed);
    let fid = failed.pid.expect("a failed execution has an id").0;
    let done = kernel
        .send(vec![Item::new(
            AgentId::from("nr"),
            json!({"type": "echo"}),
        )
        .with_id("ok_job")])
        .await
        .remove(0);
    let did = done.pid.expect("an id").0;

    assert_eq!(
        ack(Value::Null).await,
        r#"{"error":"ack_deadletter: id required","reason":"missing_arg","arg":"id"}"#
    );
    assert_eq!(
        ack(json!("")).await,
        r#"{"error":"ack_deadletter: id required","reason":"missing_arg","arg":"id"}"#
    );
    assert_eq!(
        ack(json!("nobody")).await,
        r#"{"error":"ack_deadletter: no execution 'nobody' of yours in the ledger","reason":"not_found","arg":"id"}"#
    );
    assert_eq!(
        ack(json!(987_654_321)).await,
        r#"{"error":"ack_deadletter: no execution 987654321 of yours in the ledger","reason":"not_found","arg":"id"}"#
    );
    assert_eq!(
        ack(json!(did)).await,
        format!(
            r#"{{"error":"ack_deadletter: execution {did} is 'completed', not a dead letter — only failed / timed_out / cancelled work is acknowledged","reason":"bad_arg","arg":"id"}}"#
        )
    );
    // by NAME, then again by its decimal-string id: acked, then already acked
    assert_eq!(
        ack(json!("dl_job")).await,
        format!(r#"{{"acked":true,"id":{fid}}}"#)
    );
    assert_eq!(
        ack(json!(fid.to_string())).await,
        format!(r#"{{"acked":true,"id":{fid},"already":true}}"#)
    );
}

/// SECRET MARKS (`crate::secret`): the handler gets the payload as sent and reads
/// the marks; the call row and every state event carry the value as `[secret]`.
#[tokio::test]
async fn a_marked_value_is_shown_never_kept() {
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    struct Probe;
    #[async_trait::async_trait]
    impl Bundle for Probe {
        fn name(&self) -> &str {
            "secret_probe"
        }
        async fn handle(
            &self,
            _agent_id: &AgentId,
            payload: &Value,
            _kernel: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            Ok(Some(
                json!({"got": payload["fields"]["api_key"], "marks": crate::secret::current_secret()}),
            ))
        }
    }
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("secret_probe.tools", Probe);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    kernel.register(Agent::new(
        AgentId::from("sp"),
        Some("secret_probe.tools".to_string()),
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join("sp"),
        true,
    ));
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let sink = Arc::clone(&seen);
    let _t = kernel.add_state_subscriber(Arc::new(move |ev: &Value, _b| {
        sink.lock().expect("sink poisoned").push(ev.clone());
    }));
    let payload = json!({"type": "set", "ref": "r", "fields": {"api_key": "sk-REAL"}});
    let out = kernel
        .send(vec![
            Item::new("sp", payload).with_secret(["fields.api_key"])
        ])
        .await
        .remove(0);
    assert_eq!(
        out.result,
        json!({"got": "sk-REAL", "marks": ["fields.api_key"]})
    );
    let rows = crate::test_support::rows_about(&kernel, &AgentId::from("sp"));
    let call = rows
        .iter()
        .find(|r| r.get("kind") == Some(&json!("call")))
        .expect("a call row");
    assert_eq!(
        call.get("payload").map(|p| p["fields"]["api_key"].clone()),
        Some(json!("[secret]"))
    );
    assert_eq!(call.get("secret"), Some(&json!(["fields.api_key"])));
    let events = format!("{:?}", seen.lock().expect("sink poisoned"));
    assert!(
        !events.contains("sk-REAL"),
        "the state stream carried the value"
    );
    assert!(events.contains("[secret]"));
    // malformed marks are refused before anything runs
    for bad in [
        json!("fields.api_key"),
        json!([""]),
        json!(["a..b"]),
        json!([1]),
        json!(["type"]),
    ] {
        let out = kernel
            .send(vec![
                Item::new("sp", json!({"type": "set"})).with_raw_secret(bad.clone())
            ])
            .await
            .remove(0);
        assert_eq!(out.result["arg"], json!("secret"), "{bad}");
    }
}

/// While draining, a FORK starting a send is new work: refused. A keepalive fork
/// (a persister) still sends (python `drain_admits`; audit 2026-10-14).
#[tokio::test]
async fn while_draining_a_fork_is_refused_and_a_keepalive_fork_is_not() {
    let (kernel, _log) = hop_chain();
    let _ = kernel.shutdown_agents().await;
    assert!(kernel.draining());
    for keepalive in [false, true] {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let k = Arc::clone(&kernel);
        let work = async move {
            let out = k
                .send(vec![Item::new(
                    AgentId::from("h_c"),
                    json!({"type": "hop"}),
                )])
                .await
                .remove(0);
            let _ = tx.send(out);
        };
        if keepalive {
            kernel.spawn_keepalive_task(&AgentId::from("h_a"), "loop", work);
        } else {
            kernel.spawn_task(&AgentId::from("h_a"), "loop", work);
        }
        let out = rx.await.expect("the fork ran");
        if keepalive {
            assert_eq!(
                out.status,
                crate::outcome::Status::Completed,
                "{}",
                out.result
            );
        } else {
            assert_eq!(out.result["reason"], "draining", "{}", out.result);
        }
    }
}
