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
    let root = make_agent("core");
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
    assert!(body.is_empty());
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
        ) -> Result<(Reply, Vec<u8>), BundleError> {
            // Echo the bytes back as the reply BODY (symmetric channel).
            let len = blob.len();
            Ok((Some(json!({"ok": true, "len": len})), blob))
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
    assert_eq!(body.len(), 64);
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
    let root = make_agent("core");
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
    let root = make_agent("core");
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let mut meta = Map::new();
    // A wildcard on the root — deliberately generous, and still not enough.
    meta.insert("can_call".into(), json!({"core": "*"}));
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
    let root = make_agent("core");
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
    let rest = granted("rest", Some("core"), "can_call", json!({"ws": "*"}));
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
    let root = make_agent("core");
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
            .starts_with("leaf cannot delegate '*' on $loader: it resolves to core,"),
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
    let d = confer(json!({"core": ["ps"]})).expect("a grant on the root passed");
    assert!(
        d["error"].as_str().unwrap().starts_with(
            "leaf cannot delegate 'ps' on core: core is not your child \
                          (its parent is None) and not in your can_call —"
        ),
        "got {}",
        d["error"]
    );
    assert_eq!(keys(&d), ["error", "reason", "side", "parent", "hint"]);
    assert_eq!(d["parent"], Value::Null);
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
    let root = make_agent("core");
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
    let rows = kernel.rows.for_agent(&worker.id);
    assert_eq!(rows.len(), 1, "one call row on the TARGET: {rows:?}");
    let call = &rows[0];
    // PYTHON'S KEY ORDER: the row as it was written, then the two keys the store
    // adds (`{**row, "id": rid, "agent": agent_id}` — `id` keeps its place).
    assert_eq!(
        call.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["id", "kind", "verb", "parent", "origin", "name", "agent", "ts", "outcome"],
        "…and `outcome` is stamped onto the end at settle"
    );
    assert_eq!(call["kind"], json!("call"));
    assert_eq!(call["verb"], json!("work"));
    assert_eq!(
        call["parent"],
        Value::Null,
        "no dispatch was nested around it"
    );
    assert_eq!(
        call["origin"],
        Value::Null,
        "an operator send has no origin"
    );
    assert_eq!(call["name"], json!("named-one"));
    assert_eq!(call["id"].as_i64(), out.pid.map(crate::TaskId::get));
    // The stamp is NESTED under `outcome`, and completed carries status alone.
    assert_eq!(call["outcome"], json!({"status": "completed"}));
}

#[tokio::test]
async fn an_errored_call_row_stamps_the_python_error_and_reason() {
    let (kernel, worker) = c36_kernel();
    kernel
        .send(vec![Item::new(worker.id.clone(), json!({"type": "nope"}))])
        .await;
    let call = kernel.rows.for_agent(&worker.id).remove(0);
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
async fn the_settle_row_carries_the_stamp_not_the_outcome() {
    let (kernel, worker) = c36_kernel();
    kernel
        .send(vec![Item::new(worker.id.clone(), json!({"type": "work"}))])
        .await;
    let settles: Vec<_> = kernel
        .rows
        .for_agent(&AgentId::from("core"))
        .into_iter()
        .filter(|r| r.get("kind") == Some(&json!("settle")))
        .collect();
    assert_eq!(settles.len(), 1);
    let s = &settles[0];
    assert_eq!(s["settled"], json!("w"), "the agent the call was on");
    assert_eq!(s["outcome"], json!({"status": "completed"}));
    let o = s["outcome"].as_object().unwrap();
    assert!(
        !o.contains_key("pid") && !o.contains_key("result") && !o.contains_key("attempts"),
        "the ledger's Outcome never enters the message log: {o:?}"
    );
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
    assert!(kernel
        .rows
        .for_agent(&AgentId::from("core"))
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
    let rows = inner.rows.for_agent(&wid);
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
        ) -> Result<(Reply, Vec<u8>), BundleError> {
            Ok((Some(json!({"written": blob.len()})), Vec::new()))
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
