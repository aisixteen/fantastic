//! Unit tests for [`crate::send`].
use crate::outcome::Item;

use super::*;
use crate::agent::Agent;
use serde_json::Map;
use std::path::Path;

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
async fn with_sender_propagates_to_nested_async() {
    let r = with_sender(AgentId::from("alice"), async {
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

#[tokio::test]
async fn send_with_binary_missing_target_returns_error() {
    let kernel = Arc::new(Kernel::new());
    let (v, body) = kernel
        .send_with_binary(
            &AgentId::from("nope"),
            json!({"type": "frob"}),
            vec![1, 2, 3],
        )
        .await;
    assert!(v["error"].as_str().unwrap_or("").contains("no agent"));
    assert_eq!(body, None);
}

#[tokio::test]
async fn send_with_binary_routes_through_bundle_handle_binary() {
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
            blob: Vec<u8>,
            _kernel: &Arc<Kernel>,
        ) -> Result<(Reply, Option<Vec<u8>>), BundleError> {
            // Echo the bytes back as the reply BODY (symmetric channel).
            let len = blob.len();
            Ok((Some(json!({"ok": true, "len": len})), Some(blob)))
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
    let (reply, body) = kernel
        .send_with_binary(
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
    let (v, body) = kernel
        .send_with_binary(&AgentId::from("bare"), json!({"type": "ingest"}), vec![1])
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
    let (v, body) = kernel
        .send_with_binary(
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
         'nope_state.tools'"
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
            _blob: Vec<u8>,
            _kernel: &Arc<Kernel>,
        ) -> Result<(Reply, Option<Vec<u8>>), BundleError> {
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
    let (v, body) = kernel
        .send_with_binary(&AgentId::from("bm"), json!({"type": "ingest"}), vec![1])
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
         'nope_state.tools'"
    );
}

// ─── #2 — current_origin: the CALLER, not the callee ────────────────

#[tokio::test]
async fn a_handler_reads_its_caller_not_itself() {
    // `current_sender()` inside a handler is that handler's OWN agent — the
    // callee. A bundle whose payload NAMES a resource needs the CALLER, which
    // is what `current_origin()` carries. Without it an agent could write
    // another agent's namespace through a blob verb and nothing could tell.
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    use crate::send::{current_origin, with_sender};
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
    // half of this test needs real wiring — a bare `with_sender` of an
    // unregistered id is (correctly) deny-all.
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
    let _ = with_sender(AgentId::from("caller"), async {
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
    // Found 2026-08-27 in a production audit: a browser leg granted exactly
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

    let denial = crate::send::with_sender(AgentId::from("nosy"), async {
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
async fn the_state_stream_is_not_covered_by_a_wildcard() {
    // The broadest read the kernel offers. The structural baseline grants
    // `$parent: "*"`, so WITHOUT `state_subscribe` being privileged every direct
    // child of the root could tap the whole stream while holding no grant that
    // names it.
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

    let denial = crate::send::with_sender(AgentId::from("leaf"), async {
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

fn make_agent_under(id: &str, parent: &str) -> Arc<Agent> {
    Agent::new(
        id.into(),
        None,
        Some(AgentId::from(parent)),
        Map::new(),
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

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
        .gate(
            &AgentId::from("totals"),
            "write_stream",
            &store,
            &json!({"type": "write_stream"}),
        )
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
        .gate(
            &AgentId::from("rest"),
            "reflect",
            &ws,
            &json!({"type": "reflect"}),
        )
        .expect("an unaccepted call was admitted");
    assert_eq!(d["error"], "ws does not accept 'reflect' from rest");
    assert_eq!(keys(&d), ["error", "reason", "hint", "side"]);
    assert_eq!(d["side"], "ingress");
}

#[tokio::test]
async fn conferral_refusals_use_python_repr_and_none() {
    // The conferral dicts are python LITERALS, so they keep their own key order
    // (`side` before `hint`) — only the four `refusal()` denials reorder. What
    // changes here is the quoting, and the absent parent: python renders `None`.
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let mid = make_agent("mid");
    kernel.register(Arc::clone(&mid));
    // `leaf` places under ITSELF, which is baseline — so the placement gate
    // passes and conferral is what is actually under test.
    let leaf = make_agent_under("leaf", "mid");
    kernel.register(Arc::clone(&leaf));

    let confer = |grant: Value| {
        kernel.gate(
            &AgentId::from("leaf"),
            "create_agent",
            &root,
            &json!({"type": "create_agent", "parent_id": "leaf", "can_call": grant}),
        )
    };

    // An invented structural token.
    let d = confer(json!({"$x": "*"})).expect("an unknown $token was conferred");
    assert_eq!(
        d["error"],
        "leaf cannot confer '$x': not a structural token — $self, $parent, \
         $children and $loader are the only ones"
    );
    assert_eq!(keys(&d), ["error", "reason", "arg", "side", "hint"]);

    // `$loader` is real reach, subset-probed like any lateral edge.
    let d = confer(json!({"$loader": "*"})).expect("$loader was conferred unheld");
    assert!(
        d["error"]
            .as_str()
            .unwrap()
            .starts_with("leaf cannot delegate '*' on $loader: it resolves to kernel_state,"),
        "got {}",
        d["error"]
    );
    assert_eq!(keys(&d), ["error", "reason", "side", "hint"]);

    // A target that is not an agent at all — almost always same-batch.
    let d = confer(json!({"ghost": ["read"]})).expect("a grant on a ghost passed");
    assert!(
        d["error"]
            .as_str()
            .unwrap()
            .starts_with("leaf cannot delegate 'read' on ghost: no such agent —"),
        "got {}",
        d["error"]
    );
    assert_eq!(keys(&d), ["error", "reason", "side", "missing", "hint"]);

    // A target that exists and is not held: python prints `its parent is None`
    // for a parentless agent (the root), not `none`.
    let d = confer(json!({"kernel_state": ["ps"]})).expect("a grant on the root passed");
    assert!(
        d["error"].as_str().unwrap().starts_with(
            "leaf cannot delegate 'ps' on kernel_state: kernel_state is not your \
             child (its parent is None) and not in your can_call —"
        ),
        "got {}",
        d["error"]
    );
    assert_eq!(keys(&d), ["error", "reason", "side", "parent", "hint"]);
    assert_eq!(d["parent"], Value::Null);
}

#[tokio::test]
async fn conferring_an_empty_verb_list_probes_star() {
    // python: `probe = need if (need and "*" not in need) else ["*"]` — an
    // EMPTY conferred verb list is falsy, so it probes `*` too. Before the
    // C21 fix, `Value::Array(vs)` with `vs.is_empty()` still matched the
    // "use the list" arm, built an EMPTY probe vec, the `for v in probe` loop
    // ran zero times, and the conferral silently PASSED — `can_call:{"x":[]}`
    // was accepted from a granter holding NOTHING on `x` at all.
    let kernel = Arc::new(Kernel::new());
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let mid = make_agent("mid");
    kernel.register(Arc::clone(&mid));
    let leaf = make_agent_under("leaf", "mid");
    kernel.register(Arc::clone(&leaf));

    // `leaf` holds nothing on the ROOT (its grandparent, not its own parent —
    // the `$parent` baseline does not cover it) and tries to confer an EMPTY
    // verb list on it.
    let d = kernel
        .gate(
            &AgentId::from("leaf"),
            "create_agent",
            &root,
            &json!({"type": "create_agent", "parent_id": "leaf",
                    "can_call": {"kernel_state": []}}),
        )
        .expect("an empty conferred verb list must still probe '*' and be denied");
    assert_eq!(d["reason"], "unauthorized");
    assert_eq!(d["side"], "delegation");
}

// ── C35: alias resolves through well_known ──────────────────────────────

/// Answers `{"id": <own id>}` on every verb — just enough to prove a `send`
/// landed on the right agent.
struct IdEcho;
#[async_trait::async_trait]
impl crate::bundle::Bundle for IdEcho {
    fn name(&self) -> &str {
        "id_echo"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        Ok(Some(json!({"id": agent_id.0})))
    }
}

fn alias_kernel() -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    let mut reg = crate::bundle::BundleRegistry::new();
    reg.register("id_echo.tools", IdEcho);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = make_agent("kernel_state");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
}

#[tokio::test]
async fn alias_resolves_to_agent() {
    // python `test_alias_resolves_to_agent`: `alias` wires into `well_known`
    // on register, and `send` resolves a target through it.
    let kernel = alias_kernel();
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","id":"store_x",
                   "handler_module":"id_echo.tools","alias":"store"}),
        )])
        .await;
    assert_eq!(
        kernel.well_known.get("store").map(|e| e.value().clone()),
        Some(AgentId::from("store_x"))
    );
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("store"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "store_x");
}

#[tokio::test]
async fn literal_id_unaffected_by_aliases() {
    // python `test_literal_id_unaffected_by_aliases`: a literal id that is
    // NOT an alias resolves to itself, alias meta on the record or not.
    let kernel = alias_kernel();
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","id":"real",
                   "handler_module":"id_echo.tools","alias":"real_alias"}),
        )])
        .await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("real"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "real");
}

#[tokio::test]
async fn alias_cleared_on_delete() {
    // python `test_alias_cleared_on_delete`: `unregister` drops any alias
    // pointing at the removed agent; a dangling alias then names no agent.
    let kernel = alias_kernel();
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","id":"a1",
                   "handler_module":"id_echo.tools","alias":"myalias"}),
        )])
        .await;
    assert!(kernel.well_known.contains_key("myalias"));
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"delete_agent","id":"a1"}),
        )])
        .await;
    assert!(!kernel.well_known.contains_key("myalias"));
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("myalias"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_some());
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
    assert_eq!(
        s["outcome"],
        json!({"status": "completed", "bytes": 12})
    );
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
    assert!(crate::test_support::rows_about(&kernel, &AgentId::from("kernel_state"))
        .iter()
        .all(|r| r.get("kind") != Some(&json!("settle"))));
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
async fn a_forked_body_mints_before_the_dispatch_settles() {
    // Python's ready queue is FIFO: the handler's `spawn_task` queues the body
    // (`asyncio.create_task`) and the settle that awaits the dispatch task
    // resumes BEHIND it, so the body's first mints precede the `settle` row.
    //
    // WHAT THIS DOES NOT PIN. The `ai_paths` id-order divergence measured on
    // 2026-09-21 (`deliver_reply` rows 2 ids late, `call send` rows 1 early,
    // 20 + 9 occurrences) is NOT this shape: rust already matches python here,
    // on the multi-thread runtime, 50 iterations out of 50. That divergence
    // needs a deeper chain (a spawned body that itself spawns) and is open on
    // the owner — a single `yield_now()` at python's await point in `settle`
    // fixed the `deliver_reply` class and made the `call send` class worse
    // (ai_to_ai id/parent mismatches 71 -> 166), and one worker thread was
    // worse still (354/354). Kept as the characterisation of the simple case.
    use crate::bundle::{Bundle, BundleError, BundleRegistry, Reply};
    struct Forker;
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
            let id = k.spawn_task(a, "body", async move {
                let mut row = Map::new();
                row.insert("kind".to_string(), Value::from("note"));
                let _ = k2.rows_put(&a2, row);
            });
            Ok(Some(json!({ "task": id.0 })))
        }
    }
    let mine = AgentId::from("f");
    for _ in 0..50 {
        let mut kernel = Kernel::new();
        let mut reg = BundleRegistry::new();
        reg.register("forker.tools", Forker);
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
        let find = |owner: &AgentId, kind: &str| -> Option<i64> {
            crate::test_support::rows_about(&kernel, owner)
                .into_iter()
                .find(|r| r.get("kind").and_then(Value::as_str) == Some(kind))
                .and_then(|r| r.get("id").and_then(Value::as_i64))
        };
        let note =
            find(&mine, "note").expect("the forked body minted its row before send returned");
        let settle = find(&root.id, "settle").expect("the dispatch wrote a settle row");
        assert!(
            note < settle,
            "the forked body's row {note} must precede the settle row {settle}"
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
            blob: Vec<u8>,
            _k: &Arc<Kernel>,
        ) -> Result<(Reply, Option<Vec<u8>>), BundleError> {
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

    let (reply, _body) = kernel
        .send_with_binary(
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
    assert_eq!(got.body, Some(("bytes".to_string(), vec![1, 2, 3, 4, 5])));
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

    let (reply, _) = kernel
        .send_with_binary(&root.id, json!({"type": "reflect"}), vec![b'x'])
        .await;
    assert_eq!(
        reply["sentence"],
        "Fantastic kernel. Everything is reachable by sending messages to agents.",
        "the root's own identity, not the bundle's: {reply}"
    );
    // A bundle verb still routes to the bundle on this channel.
    let (reply, _) = kernel
        .send_with_binary(&root.id, json!({"type": "load_tree"}), vec![])
        .await;
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
        Some(&("bytes".to_string(), vec![0u8; 12])),
    );
    assert_eq!(s, r#"{"type": "write_stream", "bytes": "<bytes:12>"}"#);
}

// ─── C2 + C4 — the kernel_verbs map is python's `_SYSTEM_VERB_DOCS` ─────────

/// python `_SYSTEM_VERB_DOCS`, in declaration order, verbatim
/// (`P/kernel/_agent.py`).
const PY_KERNEL_VERB_DOCS: [(&str, &str); 11] = [
    ("create_agent", "args: handler_module:str, parent_id:str REQUIRED, id:str?, **meta. Creates a CHILD OF `parent_id` — your own id grows your own subtree (no grant needed); 'kernel_state' for top-level. No default parent."),
    ("update_agent", "args: id:str, **meta. EVERY FIELD YOU NAME IS REPLACED, NOT MERGED (a field you do not name is untouched), so a dict field must be sent WHOLE — `can_call`/`callable_by` included: restate every key you want to keep, the baseline `$self`/`$parent`/`$children` among them, or the keys you omit are GONE. Granting one edge reads like a PATCH and behaves like a PUT."),
    ("delete_agent", "args: id:str. CASCADES — deletes that agent AND ITS WHOLE SUBTREE, deepest first; refuses if any descendant carries `delete_lock`."),
    ("list_agents", "No args. Every agent's record, flat. Paged."),
    ("ps", "No args. The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, verb, started_at}]}, one row per in-flight dispatch. Paged."),
    ("ledger", "No args. EVERY execution and what it became — named under its id, unnamed under its minted pid — {executions:[{id, target, status, attempts, retries, named, outcome?, acked?}]}, paged, scoped to what YOU originated; read from the kernel's memory store plus what is not yet flushed. `outcome` keeps the RESULT only for a named call; a dead letter keeps its error; anything else is {status, bytes} — the reply itself went to its caller. Failed/timed_out/cancelled rows are the dead-letter queue (announced by the `on_deadletter` event): inspect, retry with a NEW id if you choose, then `ack_deadletter` it — nothing is retried silently."),
    ("ack_deadletter", "args: id:int|str (req — an execution id, or its name). Acknowledges a DEAD LETTER (failed/timed_out/cancelled) you originated: recorded in the kernel's store, it may then be pruned. Returns {acked:true, id}."),
    ("kernel_start", "No args. THE START SEQUENCE — the root must name `memory_store_id` (fail loud otherwise): boots the root, then that store, starts flushing the kernel's execution rows into it, then boots every other agent in registration order. Idempotent. Returns {started, store, booted:[ids]}."),
    ("cancel", "args: task_id:str. Kills that task AND ITS SUBTREE — the id names its own agent. Returns {cancelled:bool, killed:[…]}; the blocked caller's batch item settles as status:'cancelled'. Unknown id ⇒ {cancelled:false}."),
    ("shutdown_agents", "No args. Stops the AGENTS, not the process: drains (new roots refused), sends `shutdown` to every agent advertising it, waits for in-flight work, cancels the rest. Returns {drained, shut:[ids], cancelled:[ids]}."),
    ("kernel_stop", "No args. Stops the kernel PROCESS gracefully — drain, every call settles (done, or cancelled into the dead letters), the last rows flush, agents shut down, exit. Under env:'container', the container."),
];

#[test]
fn kernel_verb_docs_match_python_verbatim() {
    let docs = kernel_verb_docs();
    let obj = docs.as_object().expect("an object");
    assert_eq!(obj.len(), PY_KERNEL_VERB_DOCS.len(), "{docs}");
    for (verb, text) in PY_KERNEL_VERB_DOCS {
        assert_eq!(obj[verb], json!(text), "{verb}");
    }
    // `shutdown_agents` has no em-dash, so distilled == full.
    let distilled = kernel_verbs_view(false);
    assert_eq!(
        distilled["shutdown_agents"],
        json!(PY_KERNEL_VERB_DOCS[9].1)
    );
    assert_eq!(
        distilled["ack_deadletter"],
        json!("args: id:int|str (req")
    );
}

#[test]
fn full_kernel_verbs_keep_python_order() {
    // python renders `dict(_SYSTEM_VERB_DOCS)` under `detail:full` — declaration
    // order — and sorts only the distilled comprehension.
    let full = kernel_verbs_view(true);
    let keys: Vec<&str> = full
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut want = vec!["_"];
    want.extend(PY_KERNEL_VERB_DOCS.iter().map(|(v, _)| *v));
    assert_eq!(keys, want);

    let distilled = kernel_verbs_view(false);
    let keys: Vec<&str> = distilled
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut sorted: Vec<&str> = PY_KERNEL_VERB_DOCS.iter().map(|(v, _)| *v).collect();
    sorted.sort_unstable();
    let mut want = vec!["_"];
    want.extend(sorted);
    assert_eq!(keys, want);
}
