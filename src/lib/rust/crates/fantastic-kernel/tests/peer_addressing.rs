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
    Agent, AgentId, Bundle, BundleRegistry, Item, Kernel, Outcome, PeerRoute, Reply,
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

#[async_trait]
impl Bundle for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    async fn handle(
        &self,
        id: &AgentId,
        _payload: &Value,
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
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    kernel
}

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// A fake route that records every call and answers `reply` (or `{ok: agent}`).
fn route(via: &str, calls: &Calls, reply: Option<Value>) -> Arc<PeerRoute> {
    let calls = Arc::clone(calls);
    PeerRoute::new(
        via,
        move |agent_id, payload, _body| {
            calls.lock().unwrap().push((agent_id.clone(), payload));
            let r = reply.clone().unwrap_or_else(|| json!({"ok": agent_id}));
            Box::pin(async move { (r, None) })
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
        None => k.send(item).await.remove(0),
        Some(s) => fantastic_kernel::test_support::as_agent(AgentId::from(s), k.send(item))
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
async fn an_agent_needs_the_qualified_target_in_its_can_call() {
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
               "handler_module": "echo.tools", "can_call": {&canvas: ["around"]}}),
    )
    .await;
    create(
        &k,
        json!({"type": "create_agent", "parent_id": "kernel", "id": "wild",
               "handler_module": "echo.tools",
               "can_call": {format!("{PAGE}|*"): ["around"]}}),
    )
    .await;
    let refused = send(&k, &canvas, json!({"type": "around"}), Some("plain")).await;
    let r = &refused.result;
    assert_eq!(
        (&r["reason"], &r["side"]),
        (&json!("unauthorized"), &json!("egress")),
        "{r}"
    );
    assert!(
        r["hint"].as_str().unwrap().contains(&format!("{PAGE}|*")),
        "{r}"
    );
    // a literal grant and the whole-kernel `<guid>|*` both reach it
    for sender in ["named", "wild"] {
        let ok = send(&k, &canvas, json!({"type": "around"}), Some(sender)).await;
        assert_eq!(ok.status.as_str(), "completed", "{sender}: {:?}", ok.result);
    }
    assert_eq!(calls.lock().unwrap().len(), 2);
    // a verb the grant does not name is refused at egress too
    let denied = send(&k, &canvas, json!({"type": "clear"}), Some("named")).await;
    assert_eq!(denied.result["side"], "egress", "{:?}", denied.result);
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

/// Set when the future it lives in is dropped — completed or abandoned.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// One TAB of the identity PAGE: records the call as `<name>:<agent>`, answers
/// when `answer` does, and flags `dropped` when its call future goes away.
fn tab(
    k: &Arc<Kernel>,
    name: &str,
    calls: &Calls,
    answer: oneshot::Receiver<Value>,
    dropped: &Arc<AtomicBool>,
) -> Arc<PeerRoute> {
    let calls = Arc::clone(calls);
    let answer = Arc::new(Mutex::new(Some(answer)));
    let dropped = Arc::clone(dropped);
    let name = name.to_string();
    let r = PeerRoute::new(
        format!("leg_{name}"),
        move |agent_id, payload, _b| {
            calls
                .lock()
                .unwrap()
                .push((format!("{name}:{agent_id}"), payload));
            let rx = answer.lock().unwrap().take().expect("one call per tab");
            let flag = DropFlag(Arc::clone(&dropped));
            Box::pin(async move {
                let _flag = flag;
                (rx.await.unwrap_or(Value::Null), None)
            })
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
    // the slower tab's call is dropped, not left waiting
    assert!(a_gone.load(Ordering::SeqCst));
    drop(a_tx);
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
    assert_eq!(out.result["side"], "egress", "{:?}", out.result);
    assert!(calls.lock().unwrap().is_empty());
}

/// `docs of: <guid>|<agent>`: the question goes to the
/// connected kernel's own `docs` and the reply names the qualified id. The asker's
/// grant is `docs` on THIS root, like any `docs`; the peer judges its own leg.
/// Python `test_docs_of_a_peer_agent_asks_that_kernel`.
#[tokio::test]
async fn docs_of_a_peer_agent_asks_that_kernel() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    connect(
        &k,
        &calls,
        Some(
            json!({"of": "view_root", "bundle": "canvas.tools", "tier": "oneliners",
                    "verbs": {"place": "args: x, y"}}),
        ),
    );
    create(
        &k,
        json!({"type": "create_agent", "parent_id": "kernel", "id": "plain",
               "handler_module": "echo.tools"}),
    )
    .await;
    let refused = send(
        &k,
        "kernel",
        json!({"type": "docs", "of": format!("{PAGE}|view_root")}),
        Some("plain"),
    )
    .await;
    assert_eq!(
        refused.result["side"],
        json!("egress"),
        "{:?}",
        refused.result
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "a refused docs never reaches the peer"
    );
    create(
        &k,
        json!({"type": "create_agent", "parent_id": "kernel", "id": "reader",
               "handler_module": "echo.tools", "can_call": {"kernel_state": ["docs"]}}),
    )
    .await;
    create(
        &k,
        json!({"type": "update_agent", "id": "kernel_state",
               "callable_by": {"$self": "*", "reader": ["docs"]}}),
    )
    .await;
    let out = send(
        &k,
        "kernel",
        json!({"type": "docs", "of": format!("{PAGE}|view_root"), "tier": "oneliners"}),
        Some("reader"),
    )
    .await;
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(out.result["of"], json!(format!("{PAGE}|view_root")));
    assert_eq!(out.result["verbs"], json!({"place": "args: x, y"}));
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(
            "kernel".to_string(),
            json!({"type": "docs", "of": "view_root", "tier": "oneliners"})
        )]
    );
}

/// An unknown guid is `bad_arg`; this kernel's own guid is a LOCAL id, answered
/// here with no peer asked. Python `test_docs_of_an_unknown_guid_or_own_guid`.
#[tokio::test]
async fn docs_of_an_unknown_guid_or_own_guid() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    connect(&k, &calls, None);
    let gone = send(
        &k,
        "kernel",
        json!({"type": "docs", "of": "nope|view_root"}),
        None,
    )
    .await;
    assert_eq!(gone.result["reason"], json!("bad_arg"), "{:?}", gone.result);
    assert_eq!(gone.result["arg"], json!("of"));
    let own = send(
        &k,
        "kernel",
        json!({"type": "docs", "of": format!("{HOST}|kernel")}),
        None,
    )
    .await;
    assert_eq!(own.result["bundle"], json!("kernel"), "{:?}", own.result);
    assert!(own.result["verbs"].get("create_agent").is_some());
    assert!(calls.lock().unwrap().is_empty());
}
