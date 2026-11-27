//! CROSS-KERNEL ADDRESSING: an agent on a connected peer
//! kernel is addressed `<guid>|<agent id>`, called with an ordinary `send`, and
//! discovered through reflect. The KERNEL routes it — a transport only registers
//! the peer it is connected to (`register_peer`) and carries the frames.
//!
//! The twin of python `tests/kernel/test_peer_addressing.py`, driving the kernel
//! against a fake route; the web ws leg's real route is covered in
//! `fantastic-web`'s `tests_peer_calls.rs`.

use async_trait::async_trait;
use fantastic_kernel::{
    Agent, AgentId, BundleRegistry, Handler, Item, Kernel, Outcome, PeerRoute, Reply,
};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::sync::oneshot;

const HOST: &str = "h0"; // this kernel's guid, set by hand: the root is not booted here
const PAGE: &str = "g1";

/// A local agent that answers every verb with its own id.
struct Echo;

fantastic_kernel::stand_in_verbs!(echo_verbs { AroundV "around" { x: Option<i64>, y: Option<i64>, radius: Option<i64> }, ClearV "clear" {  } });
#[async_trait]
impl Handler for Echo {
    type Call = echo_verbs::Call;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "echo"
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        echo_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        echo_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }
    async fn handle(
        &self,
        id: &AgentId,
        _call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Some(json!({"id": id.0})))
    }
}

fn kernel(tmp: &TempDir, guid: Option<&str>) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    let mut reg = BundleRegistry::new();
    reg.register("echo.tools", Echo);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let mut meta = Map::new();
    if let Some(g) = guid {
        meta.insert("guid".to_string(), json!(g));
    }
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        meta,
        tmp.path().join(".fantastic"),
        true,
    );
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
    kernel.set_root(root);
    kernel
}

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// A fake route that records every call and answers `reply` (or `{ok: agent}`).
fn route(via: &str, calls: &Calls, reply: Option<Value>) -> Arc<PeerRoute> {
    let calls = Arc::clone(calls);
    PeerRoute::new(
        via,
        move |agent_id, payload, _body, answer: fantastic_kernel::PeerAnswer| {
            calls.lock().unwrap().push((agent_id.clone(), payload));
            let r = reply.clone().unwrap_or_else(|| json!({"ok": agent_id}));
            answer.answer((r, None), Vec::new());
            Box::pin(async {})
        },
        |_, _, _| Box::pin(async {}),
    )
}

fn connect(k: &Arc<Kernel>, calls: &Calls, reply: Option<Value>) -> Arc<PeerRoute> {
    let r = route("leg", calls, reply);
    k.register_peer(PAGE, &r);
    r
}

async fn send(k: &Arc<Kernel>, target: &str, payload: Value, as_: Option<&str>) -> Outcome {
    let item = vec![Item::new(AgentId::from(target), payload)];
    match as_ {
        None => k.operator_send(item).await.remove(0),
        Some(s) => fantastic_kernel::test_support::as_agent(AgentId::from(s), k.operator_send(item))
            .await
            .remove(0),
    }
}

async fn create(k: &Arc<Kernel>, rec: Value) {
    let r = send(k, "kernel", rec, None).await;
    assert!(r.result.get("error").is_none(), "{:?}", r.result);
}

#[tokio::test]
async fn an_operator_send_reaches_the_peer_agent() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    connect(&k, &calls, Some(json!({"cells": [1, 2]})));
    let payload = json!({"type": "around", "x": 1, "y": 2, "radius": 3});
    let out = send(&k, &format!("{PAGE}|canvas"), payload.clone(), None).await;
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(out.result, json!({"cells": [1, 2]}));
    assert_eq!(
        *calls.lock().unwrap(),
        vec![("canvas".to_string(), payload)]
    );
}

#[tokio::test]
#[ignore = "peers parked: the follow-up issue to #754 designs peer lines"]
async fn an_agent_needs_the_qualified_target_in_its_can() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    connect(&k, &calls, None);
    let canvas = format!("{PAGE}|canvas");
    create(
        &k,
        json!({"type": "create_agent", "parent_id": "kernel", "id": "plain",
               "handler_module": "echo.tools"}),
    )
    .await;
    create(
        &k,
        json!({"type": "create_agent", "parent_id": "kernel", "id": "named",
               "handler_module": "echo.tools", "can": {&canvas: {"call": ["around"]}}}),
    )
    .await;
    create(
        &k,
        json!({"type": "create_agent", "parent_id": "kernel", "id": "wild",
               "handler_module": "echo.tools",
               "can": {format!("{PAGE}|*"): {"call": ["around"]}}}),
    )
    .await;
    let refused = send(&k, &canvas, json!({"type": "around"}), Some("plain")).await;
    let r = &refused.result;
    assert_eq!(r["reason"], json!("unknown_agent"), "{r}");
    // a literal grant and the whole-kernel `<guid>|*` both reach it
    for sender in ["named", "wild"] {
        let ok = send(&k, &canvas, json!({"type": "around"}), Some(sender)).await;
        assert_eq!(ok.status.as_str(), "completed", "{sender}: {:?}", ok.result);
    }
    assert_eq!(calls.lock().unwrap().len(), 2);
    // a verb the grant does not name is refused at egress too
    let denied = send(&k, &canvas, json!({"type": "clear"}), Some("named")).await;
    assert_eq!(denied.result["reason"], "unknown_agent", "{:?}", denied.result);
}

#[tokio::test]
async fn a_guid_nobody_is_connected_as_is_an_unknown_agent() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let out = send(&k, "nobody|canvas", json!({"type": "reflect"}), None).await;
    assert_eq!(out.result["reason"], "unknown_agent", "{:?}", out.result);
}

#[tokio::test]
async fn this_kernels_own_guid_names_a_local_agent() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    create(
        &k,
        json!({"type": "create_agent", "parent_id": "kernel", "id": "cli",
               "handler_module": "echo.tools"}),
    )
    .await;
    let out = send(&k, &format!("{HOST}|cli"), json!({"type": "reflect"}), None).await;
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(out.result["id"], "cli", "{:?}", out.result);
    let root = send(
        &k,
        &format!("{HOST}|kernel"),
        json!({"type": "reflect", "tree": "none"}),
        None,
    )
    .await;
    assert_eq!(root.result["id"], "kernel_state", "{:?}", root.result);
}

#[tokio::test]
async fn the_root_reflect_lists_connected_peers() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let r = connect(&k, &Calls::default(), None);
    let reflect = json!({"type": "reflect", "tree": "none"});
    let out = send(&k, "kernel", reflect.clone(), None).await;
    assert_eq!(out.result["peers"], json!([{"guid": PAGE, "via": "leg"}]));
    // after `runtime`, python's key order
    let keys: Vec<&String> = out.result.as_object().unwrap().keys().collect();
    let at = keys.iter().position(|k| *k == "runtime").unwrap();
    assert_eq!(keys[at + 1], "peers", "{keys:?}");
    k.unregister_peer(PAGE, &r);
    let out = send(&k, "kernel", reflect, None).await;
    assert_eq!(out.result["peers"], json!([]));
}

/// The tabs of one identity are ONE principal: a second
/// route for a live guid is ADDED, and a disconnect removes only its own.
#[tokio::test]
async fn one_guid_holds_a_set_of_live_routes() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    let first = connect(&k, &calls, None);
    let other = route("leg2", &calls, None);
    k.register_peer(PAGE, &other);
    k.register_peer(PAGE, &other); // the same route twice stays one
    assert_eq!(
        k.peer_rows(),
        json!([{"guid": PAGE, "via": "leg"}, {"guid": PAGE, "via": "leg2"}])
    );
    k.unregister_peer(PAGE, &other);
    let left = k.peer_routes(PAGE);
    assert!(left.len() == 1 && Arc::ptr_eq(&left[0], &first));
    k.unregister_peer(PAGE, &first);
    assert!(k.peer_routes(PAGE).is_empty());
    assert_eq!(k.peer_rows(), json!([]));
}

/// One TAB of the identity PAGE: records the call as `<name>:<agent>`, answers it
/// when `answer` does (#795: the route's future ends once the frame is out), and sets
/// `late` when its answer came after another tab's had settled the call.
fn tab(
    k: &Arc<Kernel>,
    name: &str,
    calls: &Calls,
    answer: oneshot::Receiver<Value>,
    late: &Arc<AtomicBool>,
) -> Arc<PeerRoute> {
    let calls = Arc::clone(calls);
    let answer = Arc::new(Mutex::new(Some(answer)));
    let late = Arc::clone(late);
    let name = name.to_string();
    let r = PeerRoute::new(
        format!("leg_{name}"),
        move |agent_id, payload, _b, peer_answer: fantastic_kernel::PeerAnswer| {
            calls
                .lock()
                .unwrap()
                .push((format!("{name}:{agent_id}"), payload));
            let rx = answer.lock().unwrap().take().expect("one call per tab");
            let late = Arc::clone(&late);
            tokio::spawn(async move {
                if let Ok(v) = rx.await {
                    if !peer_answer.answer((v, None), Vec::new()) {
                        late.store(true, Ordering::SeqCst);
                    }
                }
            });
            Box::pin(async {})
        },
        |_, _, _| Box::pin(async {}),
    );
    k.register_peer(PAGE, &r);
    r
}

#[tokio::test]
async fn a_call_reaches_every_tab_and_settles_with_the_first_reply() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    let (a_tx, a_rx) = oneshot::channel();
    let (b_tx, b_rx) = oneshot::channel();
    let (a_gone, b_gone) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    tab(&k, "a", &calls, a_rx, &a_gone);
    tab(&k, "b", &calls, b_rx, &b_gone);
    let k2 = Arc::clone(&k);
    let send = tokio::spawn(async move {
        send(
            &k2,
            &format!("{PAGE}|canvas"),
            json!({"type": "around"}),
            None,
        )
        .await
    });
    while calls.lock().unwrap().len() < 2 {
        tokio::task::yield_now().await;
    }
    let mut seen = calls.lock().unwrap().clone();
    seen.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(
        seen,
        vec![
            ("a:canvas".to_string(), json!({"type": "around"})),
            ("b:canvas".to_string(), json!({"type": "around"})),
        ]
    );
    b_tx.send(json!({"from": "b"})).unwrap();
    let out = tokio::time::timeout(std::time::Duration::from_secs(3), send)
        .await
        .expect("settles")
        .unwrap();
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(out.result, json!({"from": "b"}));
    // the slower tab's answer is late: it settles nothing
    a_tx.send(json!({"from": "a"})).unwrap();
    for _ in 0..200 {
        if a_gone.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(a_gone.load(Ordering::SeqCst), "the second answer was taken");
    assert!(!b_gone.load(Ordering::SeqCst), "the first answer was late");
}

#[tokio::test]
async fn a_disconnected_tab_leaves_the_identity_routable() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    let (_a_tx, a_rx) = oneshot::channel();
    let (b_tx, b_rx) = oneshot::channel();
    let flag = Arc::new(AtomicBool::new(false));
    let gone = tab(&k, "a", &calls, a_rx, &flag);
    tab(&k, "b", &calls, b_rx, &flag);
    b_tx.send(json!({"from": "b"})).unwrap();
    k.unregister_peer(PAGE, &gone);
    let out = send(
        &k,
        &format!("{PAGE}|canvas"),
        json!({"type": "around"}),
        None,
    )
    .await;
    assert_eq!(out.result, json!({"from": "b"}), "{:?}", out.result);
    let who: Vec<String> = calls.lock().unwrap().iter().map(|c| c.0.clone()).collect();
    assert_eq!(who, vec!["b:canvas".to_string()]);
}

/// A call to a peer carries NO caller: the peer judges what arrives as the leg
/// that serves it there, so this kernel's guid is not on the call — and a kernel
/// whose root never minted one reaches the peer the same.
#[tokio::test]
async fn a_peer_call_names_no_caller() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, None);
    let calls = Calls::default();
    connect(&k, &calls, None);
    let out = send(
        &k,
        &format!("{PAGE}|canvas"),
        json!({"type": "around"}),
        None,
    )
    .await;
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![("canvas".to_string(), json!({"type": "around"}))]
    );
}

// ── rust-only pins (python diverges; see the port report) ───────────────

/// A peer's reflect is ITS answer: its kernel composed it over its own tree, and
/// this kernel adds nothing (python decorates it and raises on the remote target).
#[tokio::test]
async fn a_peer_reflect_is_its_own_answer() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    let theirs = json!({"id": "kernel_state", "sentence": "the page kernel"});
    connect(&k, &calls, Some(theirs.clone()));
    let out = send(
        &k,
        &format!("{PAGE}|kernel"),
        json!({"type": "reflect"}),
        None,
    )
    .await;
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(out.result, theirs);
}

/// `create_agent` sent to a peer places its child THERE: a local `parent_id` in
/// the payload does not stand in for the egress grant on the peer.
#[tokio::test]
#[ignore = "peers parked: the follow-up issue to #754 designs peer lines"]
async fn a_create_on_a_peer_is_gated_on_the_peer_target() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    connect(&k, &calls, None);
    create(
        &k,
        json!({"type": "create_agent", "parent_id": "kernel", "id": "plain",
               "handler_module": "echo.tools"}),
    )
    .await;
    let out = send(
        &k,
        &format!("{PAGE}|kernel"),
        json!({"type": "create_agent", "parent_id": "plain", "handler_module": "x.tools"}),
        Some("plain"),
    )
    .await;
    assert_eq!(out.result["reason"], "unknown_agent", "{:?}", out.result);
    assert!(calls.lock().unwrap().is_empty());
}

/// The verb docs of `<guid>|<agent>`: the question goes to the connected kernel's
/// own `reflect` of that agent, with `tier`, `verb` and `part`; the peer judges its
/// own leg. An agent sender reaches no remote target until peer lines exist.
/// Python `test_docs_of_a_peer_agent_asks_that_kernel`.
#[tokio::test]
#[ignore = "peers parked: the follow-up issue to #754 designs peer lines"]
async fn verb_docs_of_a_peer_agent_ask_that_kernel() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    connect(
        &k,
        &calls,
        Some(json!({"id": "view_root", "tier": "oneliners", "verbs": {"place": "args: x, y"}})),
    );
    create(
        &k,
        json!({"type": "create_agent", "parent_id": "kernel", "id": "plain",
               "handler_module": "echo.tools"}),
    )
    .await;
    let qualified = format!("{PAGE}|view_root");
    let refused = send(
        &k,
        &qualified,
        json!({"type": "reflect", "tier": "oneliners"}),
        Some("plain"),
    )
    .await;
    assert_eq!(
        refused.result,
        fantastic_kernel::unknown_agent(&qualified),
        "{:?}",
        refused.result
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "a refused reflect never reaches the peer"
    );
    let out = send(
        &k,
        &qualified,
        json!({"type": "reflect", "tier": "oneliners"}),
        None,
    )
    .await;
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(out.result["verbs"], json!({"place": "args: x, y"}));
    assert_eq!(
        calls.lock().unwrap().last().cloned(),
        Some((
            "view_root".to_string(),
            json!({"type": "reflect", "tier": "oneliners"})
        ))
    );
}

/// An unknown guid is the unknown answer; this kernel's own guid is a LOCAL id,
/// answered here with no peer asked. Python `test_docs_of_an_unknown_guid_or_own_guid`.
#[tokio::test]
#[ignore = "peers parked: the follow-up issue to #754 designs peer lines"]
async fn verb_docs_of_an_unknown_guid_or_own_guid() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    connect(&k, &calls, None);
    let gone = send(
        &k,
        "nope|view_root",
        json!({"type": "reflect", "part": "docs"}),
        None,
    )
    .await;
    assert_eq!(
        gone.result,
        fantastic_kernel::unknown_agent("nope|view_root"),
        "{:?}",
        gone.result
    );
    let own = send(
        &k,
        &format!("{HOST}|kernel"),
        json!({"type": "reflect", "tree": "none", "part": "docs"}),
        None,
    )
    .await;
    assert!(own.result["verbs"].get("create_agent").is_some(), "{:?}", own.result);
    assert!(calls.lock().unwrap().is_empty());
}

/// `part`: `schema` answers each verb's JSON schema and no text, `docs` the text
/// and no schemas, and left out (with `verb` or `tier` given) it is `all`, both (#719).
#[tokio::test]
#[ignore = "peers parked: the follow-up issue to #754 designs peer lines"]
async fn verb_docs_part_picks_text_schemas_or_both() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let ask = |part: Option<&str>| {
        let mut p = json!({"type": "reflect", "tree": "none", "verb": "ps"});
        if let Some(part) = part {
            p["part"] = json!(part);
        }
        p
    };
    let schema = send(&k, "kernel", ask(Some("schema")), None).await.result;
    assert_eq!(
        schema["schemas"]["ps"]["properties"]["limit"]["type"],
        json!(["integer", "null"]),
        "{schema}"
    );
    assert!(schema.get("verbs").is_none(), "{schema}");
    let text = send(&k, "kernel", ask(Some("docs")), None).await.result;
    assert!(text["verbs"].get("ps").is_some(), "{text}");
    assert!(text.get("schemas").is_none(), "{text}");
    let all = send(&k, "kernel", ask(None), None).await.result;
    assert!(all["verbs"].get("ps").is_some() && all["schemas"].get("ps").is_some(), "{all}");
    let bad = send(&k, "kernel", ask(Some("most")), None).await.result;
    assert_eq!(bad["reason"], json!("bad_arg"), "{bad}");
}

/// A call to an agent on a CONNECTED kernel is typed by THAT kernel's declaration:
/// the sender asks the peer's `reflect verb=<verb> part=schema` first, then reads the
/// text by it — `x` written as `100` reaches the page as the number 100 (#719).
#[tokio::test]
async fn a_call_to_a_peer_agent_is_typed_by_the_peers_schema() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    connect(
        &k,
        &calls,
        Some(json!({"id": "view_root", "schemas": {"place": {
            "type": "object",
            "properties": {"x": {"type": "number"}, "kind": {"type": "string"}},
            "required": ["x"]}}})),
    );
    let target = format!("{PAGE}|view_root");
    assert!(fantastic_kernel::typing::needs_remote_schema(&k, &target, "place"));
    // the ask is a call to the peer (#795): its answer comes to the continuation
    let (tx, rx) = tokio::sync::oneshot::channel();
    fantastic_kernel::typing::remote_schema(&k, &target, "place", move |schema| async move {
        let _ = tx.send(schema);
    })
    .await;
    let schema = rx.await.expect("the peer answered").expect("the peer declares place");
    let mut payload = json!({"type": "place", "x": "100", "kind": "007"})
        .as_object()
        .cloned()
        .unwrap();
    fantastic_kernel::typing::payload(&k, &target, &mut payload, Some(&schema)).unwrap();
    assert_eq!(payload["x"], json!(100));
    assert_eq!(payload["kind"], json!("007"));
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(
            "view_root".to_string(),
            json!({"type": "reflect", "verb": "place", "part": "schema"})
        )]
    );
    let mut wrong = json!({"type": "place", "x": "far"}).as_object().cloned().unwrap();
    let refused = fantastic_kernel::typing::payload(&k, &target, &mut wrong, Some(&schema)).unwrap_err();
    assert_eq!(refused["arg"], json!("x"), "{refused}");
}

/// A schema ask the peer drops unanswered (it disconnected) ends: its continuation is
/// told, with no schema, so the call it was for still goes out (#795).
#[tokio::test]
async fn a_schema_ask_the_peer_drops_ends_with_no_schema() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let dropped = PeerRoute::new(
        "leg",
        |_agent_id, _payload, _body, answer: fantastic_kernel::PeerAnswer| {
            drop(answer);
            Box::pin(async {})
        },
        |_, _, _| Box::pin(async {}),
    );
    k.register_peer(PAGE, &dropped);
    let (tx, rx) = tokio::sync::oneshot::channel();
    fantastic_kernel::typing::remote_schema(&k, &format!("{PAGE}|view_root"), "place", move |schema| async move {
        let _ = tx.send(schema);
    })
    .await;
    assert_eq!(rx.await.expect("the continuation ran"), None);
}

/// A PEER CALL IS ASYNC (#795): the route's future ends once the frame is out, the
/// execution stays open (its `ps` row) with no handler running, and the peer's answer —
/// whenever it comes — settles it, with the reply's own secret marks; a second answer
/// is late and settles nothing.
#[tokio::test]
async fn a_peer_call_settles_from_its_later_answer() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let held: Arc<Mutex<Option<fantastic_kernel::PeerAnswer>>> = Arc::default();
    let slot = Arc::clone(&held);
    let r = PeerRoute::new(
        "leg",
        move |_agent, _payload, _body, answer: fantastic_kernel::PeerAnswer| {
            *slot.lock().unwrap() = Some(answer);
            Box::pin(async {})
        },
        |_, _, _| Box::pin(async {}),
    );
    k.register_peer(PAGE, &r);
    let k2 = Arc::clone(&k);
    let call = tokio::spawn(async move {
        send(&k2, &format!("{PAGE}|canvas"), json!({"type": "around"}), None).await
    });
    let answer = loop {
        if let Some(a) = held.lock().unwrap().clone() {
            break a;
        }
        tokio::task::yield_now().await;
    };
    assert!(!call.is_finished(), "the call settled before the peer answered");
    assert!(answer.is_open());
    assert!(answer.answer((json!({"key": "v", "ok": 1}), None), vec!["key".to_string()]));
    let out = tokio::time::timeout(std::time::Duration::from_secs(3), call)
        .await
        .expect("settles")
        .unwrap();
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(out.result, json!({"key": "v", "ok": 1}));
    assert_eq!(out.secret, vec!["key".to_string()], "the reply's marks ride the settle");
    assert!(!answer.answer((json!({"late": true}), None), Vec::new()), "a second answer was taken");
}
