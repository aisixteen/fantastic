//! CROSS-KERNEL ADDRESSING (owner, 2026-09-30): an agent on a connected peer
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
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

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

type Calls = Arc<Mutex<Vec<(String, Value, String)>>>;

/// The `secret` marks each call handed the route, in call order.
type Marks = Arc<Mutex<Vec<Vec<String>>>>;

/// A fake route that records every call and answers `reply` (or `{ok: agent}`).
fn route(via: &str, calls: &Calls, reply: Option<Value>) -> Arc<PeerRoute> {
    route_marked(via, calls, reply, &Marks::default())
}

/// [`route`], also recording the `secret` marks of every call.
fn route_marked(via: &str, calls: &Calls, reply: Option<Value>, marks: &Marks) -> Arc<PeerRoute> {
    let calls = Arc::clone(calls);
    let marks = Arc::clone(marks);
    PeerRoute::new(via, move |agent_id, payload, caller, secret, _body| {
        calls
            .lock()
            .unwrap()
            .push((agent_id.clone(), payload, caller));
        marks.lock().unwrap().push(secret);
        let r = reply.clone().unwrap_or_else(|| json!({"ok": agent_id}));
        Box::pin(async move { (r, None) })
    })
}

fn connect(k: &Arc<Kernel>, calls: &Calls, reply: Option<Value>) -> Arc<PeerRoute> {
    let r = route("leg", calls, reply);
    assert!(k.register_peer(PAGE, &r).is_none());
    r
}

async fn send(k: &Arc<Kernel>, target: &str, payload: Value, as_: Option<&str>) -> Outcome {
    let item = vec![Item::new(AgentId::from(target), payload)];
    match as_ {
        None => k.send(item).await.remove(0),
        Some(s) => fantastic_kernel::send::with_sender(AgentId::from(s), k.send(item))
            .await
            .remove(0),
    }
}

async fn create(k: &Arc<Kernel>, rec: Value) {
    let r = send(k, "kernel", rec, None).await;
    assert!(r.result.get("error").is_none(), "{:?}", r.result);
}

#[tokio::test]
async fn an_operator_send_reaches_the_peer_agent_as_this_kernel() {
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
        vec![("canvas".to_string(), payload, format!("{HOST}|kernel"))]
    );
}

/// The route gets the item's `secret` paths verbatim (rooted at the item, as the
/// peer's item is), so the PEER masks its records; this kernel masks its own.
#[tokio::test]
async fn the_items_secret_marks_ride_to_the_peer() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    let marks = Marks::default();
    let r = route_marked("leg", &calls, Some(json!({"ok": true})), &marks);
    assert!(k.register_peer(PAGE, &r).is_none());
    let key = "sk-peer-0123456789";
    let target = format!("{PAGE}|creds");
    let out = k
        .send(vec![Item::new(
            AgentId::from(target.as_str()),
            json!({"type": "set", "fields": {"api_key": key}}),
        )
        .with_secret(json!(["payload.fields", "result"]))])
        .await
        .remove(0);
    assert_eq!(out.status.as_str(), "completed", "{:?}", out.result);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![(
            "creds".to_string(),
            json!({"type": "set", "fields": {"api_key": key}}),
            format!("{HOST}|kernel")
        )]
    );
    assert_eq!(
        *marks.lock().unwrap(),
        vec![vec!["payload.fields".to_string(), "result".to_string()]]
    );
    let row = k
        .rows
        .get_one(out.pid.unwrap().get())
        .expect("the call row");
    assert_eq!(row["payload"]["fields"], json!("<secret>"), "{row:?}");
    // an unmarked call hands the route no marks
    send(&k, &target, json!({"type": "reflect"}), None).await;
    assert_eq!(marks.lock().unwrap().last(), Some(&Vec::new()));
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
    let callers: Vec<String> = calls.lock().unwrap().iter().map(|c| c.2.clone()).collect();
    assert_eq!(
        callers,
        vec![format!("{HOST}|named"), format!("{HOST}|wild")]
    );
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

#[tokio::test]
async fn one_guid_one_live_route() {
    let tmp = TempDir::new().unwrap();
    let k = kernel(&tmp, Some(HOST));
    let calls = Calls::default();
    let first = connect(&k, &calls, None);
    let other = route("leg2", &calls, None);
    let refused = k.register_peer(PAGE, &other).expect("refused");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("already connected via leg"),
        "{refused}"
    );
    assert_eq!(refused["arg"], "guid");
    // only the registered route may unregister it
    k.unregister_peer(PAGE, &other);
    assert!(k.peer(PAGE).is_some());
    k.unregister_peer(PAGE, &first);
    assert!(k.peer(PAGE).is_none());
}

#[tokio::test]
async fn a_kernel_with_no_guid_cannot_say_who_is_calling() {
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
    assert!(
        out.result["error"]
            .as_str()
            .unwrap()
            .contains("no guid yet"),
        "{:?}",
        out.result
    );
    assert!(calls.lock().unwrap().is_empty());
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
