//! The `user` bundle against a real in-memory kernel (the real gate, the real
//! secret marks). The fakes: a page loader holding records in RAM, and an
//! echo agent that answers with what it was given.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_user::{grants, UserBundle, HANDLER_MODULE};
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    bundle::BundleError,
    AgentId, Bundle, BundleRegistry, Item, Kernel, Reply,
};
use serde_json::{json, Value};

const G: &str = "g0";

/// Page records in RAM: load_tree / persist_record / forget_record.
#[derive(Default)]
struct Loader(Mutex<Vec<Value>>);

#[async_trait]
impl Bundle for Loader {
    fn name(&self) -> &str {
        "loader"
    }
    async fn handle(&self, _id: &AgentId, p: &Value, _k: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let mut recs = self.0.lock().unwrap();
        Ok(Some(match p["type"].as_str() {
            Some("load_tree") => json!({"records": *recs}),
            Some("persist_record") => {
                let r = p["record"].clone();
                recs.retain(|x| x["id"] != r["id"]);
                recs.push(r);
                json!({"ok": true})
            }
            Some("forget_record") => {
                recs.retain(|x| x["id"] != p["id"]);
                json!({"ok": true})
            }
            _ => Value::Null,
        }))
    }
}

/// Echo: the payload it was called with. Only `user` may call it.
struct Echo;

#[async_trait]
impl Bundle for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    async fn handle(&self, _id: &AgentId, p: &Value, _k: &Arc<Kernel>) -> Result<Reply, BundleError> {
        Ok(Some(json!({"echo": p})))
    }
}

/// A LEG: what arrives at it is sent onward from its own handler, as the leg —
/// the way a wire call reaches the host (a call to the leg, which sends on).
struct Relay;

#[async_trait]
impl Bundle for Relay {
    fn name(&self) -> &str {
        "relay"
    }
    async fn handle(&self, _id: &AgentId, p: &Value, k: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let item = Item::new(AgentId::from(p["to"].as_str().unwrap_or("")), p["inner"].clone());
        Ok(Some(Arc::clone(k).send(vec![item]).await.remove(0).result))
    }
}

/// The shell: a send from outside every agent (ungated).
async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k).send(vec![Item::new(AgentId::from(target), payload)]).await.remove(0).result
}

/// What a TAB does: its call reaches the leg, which sends it on to `user`.
async fn tab(k: &Arc<Kernel>, payload: Value) -> Value {
    op(k, "leg", json!({"type": "relay", "to": "user", "inner": payload})).await
}

/// What an AI cell does: `agent` sends to `user` from its own handler.
async fn as_agent(k: &Arc<Kernel>, agent: &str, payload: Value) -> Value {
    op(k, agent, json!({"type": "relay", "to": "user", "inner": payload})).await
}

fn rec(id: &str, parent: &str) -> Value {
    json!({"id": id, "parent_id": parent, "handler_module": "x.ts", "x": 0, "y": 0, "width": 100, "height": 50})
}

/// root ─ leg · echo · user {loader}; the page tree canvas {view_root {cv1 {c1}, cv2}}.
async fn setup() -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, UserBundle::default());
    let loader = Loader::default();
    {
        let mut r = loader.0.lock().unwrap();
        let mut root = rec("canvas", "loader");
        root["guid"] = json!(G);
        r.extend([root, rec("view_root", "canvas"), rec("cv1", "view_root"), rec("c1", "cv1"), rec("cv2", "view_root")]);
    }
    reg.register("loader.tools", loader);
    reg.register("echo.tools", Echo);
    reg.register("plain.tools", Echo);
    reg.register("relay.tools", Relay);
    let k = Arc::clone(&bootstrap(reg, BootstrapOptions::in_memory()).expect("boots").kernel);
    let root = k.root().unwrap().id.0.clone();
    let mk = |id: &str, parent: &str, hm: &str, extra: Value| {
        let mut p = json!({"type": "create_agent", "parent_id": parent, "id": id, "handler_module": hm});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        p
    };
    for p in [
        mk("leg", &root, "relay.tools", json!({"can_call": {"user": "*", "*|*": ["*"]}})),
        mk("echo", &root, "echo.tools", json!({"callable_by": {"user": "*"}})),
        mk("user", &root, HANDLER_MODULE, json!({"page_loader": "loader", "callable_by": {"leg": "*"},
            "can_call": {"echo": "*", "loader": "*", "*|*": ["*"]}})),
        // the page loader sits under `user` (the tree): its record verbs serve its parent
        mk("loader", "user", "loader.tools", json!({"callable_by": {"user": "*"}})),
    ] {
        let r = op(&k, &root, p.clone()).await;
        assert!(r.get("error").is_none(), "{p}: {r}");
    }
    k
}

fn ids(r: &Value) -> Vec<String> {
    let mut v: Vec<String> = r["records"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn no_token_set_is_full_access_a_token_set_must_be_given() {
    let k = setup().await;
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}})).await;
    assert_eq!(r["echo"]["type"], "ping", "only `user` may call echo: {r}");
    // a token SET on `user`: a call without it is refused
    op(&k, "kernel", json!({"type": "update_agent", "id": "user", "token_sha256": grants::hash("t-admin")})).await;
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert!(r["error"].as_str().unwrap().contains("a token is required"), "{r}");
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}, "token": "nope"})).await;
    assert!(r["error"].as_str().unwrap().contains("unknown token"), "{r}");
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}, "token": "t-admin"})).await;
    assert_eq!(r["echo"]["type"], "ping", "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_loader_under_user_is_reached_whole_and_writable() {
    let k = setup().await;
    let r = tab(&k, json!({"type": "forward", "target": "loader", "payload": {"type": "load_tree"}})).await;
    assert_eq!(ids(&r), vec!["c1", "canvas", "cv1", "cv2", "view_root"], "{r}");
    let r = tab(&k, json!({"type": "forward", "target": "loader",
        "payload": {"type": "persist_record", "record": {"id": "c9", "parent_id": "cv2"}}})).await;
    assert_eq!(r["ok"], true, "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn around_answers_from_the_page_records_by_the_callers_page_grants() {
    let k = setup().await;
    // a tab is judged by the leg's page grants (`*|*`: every page agent): the root's cells, nearest first
    let r = tab(&k, json!({"type": "around", "x": 50, "y": 25, "radius": 10})).await;
    let got: Vec<&str> = r["members"].as_array().unwrap_or_else(|| panic!("{r}")).iter().filter_map(|m| m["id"].as_str()).collect();
    assert_eq!(got, vec!["cv1", "cv2"], "{r}");
    assert_eq!(r["members"][0]["distance"], 0.0);
    let r = tab(&k, json!({"type": "around", "view": "cv1", "x": 0, "y": 0, "radius": 5})).await;
    assert_eq!(r["members"][0]["id"], "c1", "{r}");
    let r = tab(&k, json!({"type": "around", "x": "a", "y": 0, "radius": 5})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    // a cell's AI: judged by its own page grants (cv2 only)
    let root = k.root().unwrap().id.0.clone();
    op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "ai_1", "handler_module": "relay.tools",
        "can_call": {"user": ["around"], format!("{G}|cv2"): ["reflect"]}})).await;
    let mut cb = k.agents.get(&AgentId::from("user")).unwrap().record().meta.get("callable_by").cloned().unwrap();
    cb["ai_1"] = json!(["around"]);
    op(&k, "kernel", json!({"type": "update_agent", "id": "user", "callable_by": cb})).await;
    let r = as_agent(&k, "ai_1", json!({"type": "around", "x": 50, "y": 25, "radius": 10})).await;
    let got: Vec<&str> = r["members"].as_array().unwrap_or_else(|| panic!("{r}")).iter().filter_map(|m| m["id"].as_str()).collect();
    assert_eq!(got, vec!["cv2"], "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shell_needs_no_token_and_other_verbs_are_the_probes() {
    let k = setup().await;
    op(&k, "kernel", json!({"type": "update_agent", "id": "user", "token_sha256": grants::hash("t")})).await;
    // the shell (no origin): no token
    let r = op(&k, "user", json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}})).await;
    assert_eq!(r["echo"]["type"], "ping", "{r}");
    // the probe's own verbs, unchanged
    let r = op(&k, "user", json!({"type": "inquiries"})).await;
    assert!(r.get("inquiries").is_some(), "{r}");
    let r = op(&k, "user", json!({"type": "reflect"})).await;
    assert_eq!(r["handler_module"], HANDLER_MODULE, "{r}");
    assert!(r["verbs"].get("forward").is_some() && r["verbs"].get("inquiry").is_some(), "{r}");
}
