//! The users gateway against a real in-memory kernel (the real gate, the real
//! secret marks). The fakes: a page loader holding records in RAM, and an
//! echo agent that answers with what it was given.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_canvas_users::{grants, CanvasUserBundle, CanvasUsersBundle, HANDLER_MODULE, USER_MODULE};
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    bundle::BundleError,
    send::with_operator,
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

/// Echo: the payload, and the marks it was called with. WHO called it shows
/// through the gate: `user` may call any verb, `ed` only `ping`.
struct Echo;

#[async_trait]
impl Bundle for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    async fn handle(&self, _id: &AgentId, p: &Value, _k: &Arc<Kernel>) -> Result<Reply, BundleError> {
        Ok(Some(json!({"echo": p, "marks": fantastic_kernel::current_secret()})))
    }
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    let k2 = Arc::clone(k);
    let item = Item::new(AgentId::from(target), payload);
    with_operator(async move { k2.send(vec![item]).await }).await.remove(0).result
}

/// What a TAB does: call the gateway as the leg `leg`.
async fn tab(k: &Arc<Kernel>, payload: Value, secret: Option<Value>) -> Value {
    let k2 = Arc::clone(k);
    let mut item = Item::new(AgentId::from("users"), payload);
    if let Some(s) = secret {
        item = item.with_secret(s);
    }
    fantastic_kernel::with_sender(AgentId::from("leg"), async move { k2.send(vec![item]).await }).await.remove(0).result
}

fn rec(id: &str, parent: &str) -> Value {
    json!({"id": id, "parent_id": parent, "handler_module": "x.ts", "x": 0, "y": 0, "width": 100, "height": 50})
}

/// root ─ leg · echo · loader · user · users {ro, ed}; the page tree
/// canvas {cv1 {c1}, cv2 {c2}} in the loader.
async fn setup() -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, CanvasUsersBundle::default());
    reg.register(USER_MODULE, CanvasUserBundle);
    let loader = Loader::default();
    {
        let mut r = loader.0.lock().unwrap();
        let mut root = rec("canvas", "loader");
        root["guid"] = json!(G);
        r.extend([root, rec("cv1", "canvas"), rec("c1", "cv1"), rec("cv2", "canvas"), rec("c2", "cv2")]);
    }
    reg.register("loader.tools", loader);
    reg.register("echo.tools", Echo);
    reg.register("plain.tools", Echo);
    let k = Arc::clone(&bootstrap(reg, BootstrapOptions::in_memory()).expect("boots").kernel);
    let root = k.root().unwrap().id.0.clone();
    let mk = |id: &str, parent: &str, hm: &str, extra: Value| {
        let mut p = json!({"type": "create_agent", "parent_id": parent, "id": id, "handler_module": hm});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        p
    };
    for p in [
        mk("leg", &root, "plain.tools", json!({"can_call": {"users": "*"}})),
        mk("users", &root, HANDLER_MODULE, json!({"page_loader": "loader", "callable_by": {"leg": "*"}})),
        mk("echo", &root, "echo.tools", json!({"callable_by": {"user": "*", "ed": ["ping", "watch"]}})),
        // nobody holds a grant on the page loader: the gateway calls it as the operator
        mk("loader", "leg", "loader.tools", json!({})),
        mk("user", &root, "plain.tools", json!({"can_call": {"echo": "*", "*|*": ["*"]}})),
        mk("ro", "users", USER_MODULE, json!({"token_sha256": grants::hash("t-ro"), "can_call": {format!("{G}|cv1"): ["reflect"]}})),
        mk("ed", "users", USER_MODULE, json!({"token_sha256": grants::hash("t-ed"), "can_call": {"echo": ["ping", "watch"], format!("{G}|cv1"): ["*"]}})),
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
async fn no_token_is_the_default_user_with_its_full_rights_until_it_carries_a_token() {
    let k = setup().await;
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "pong"}}), None).await;
    assert_eq!(r["echo"]["type"], "pong", "only `user` may pong: {r}");
    let r = tab(&k, json!({"type": "forward", "target": "loader", "payload": {"type": "load_tree"}}), None).await;
    assert_eq!(ids(&r).len(), 5, "the default sees the whole page tree: {r}");
    // token MANDATORY: the default carries a hash → tokenless is refused
    op(&k, "kernel", json!({"type": "update_agent", "id": "user", "token_sha256": grants::hash("t-admin")})).await;
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}}), None).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert!(r["error"].as_str().unwrap().contains("a token is required"), "{r}");
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "pong"}, "token": "t-admin"}), Some(json!(["payload.token"]))).await;
    assert_eq!(r["echo"]["type"], "pong", "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_picks_its_user_and_the_gate_decides_by_that_users_grants() {
    let k = setup().await;
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}, "token": "nope"}), None).await;
    assert!(r["error"].as_str().unwrap().contains("unknown token"), "{r}");
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}, "token": "t-ed"}), None).await;
    assert_eq!(r["echo"]["type"], "ping", "{r}");
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "pong"}, "token": "t-ed"}), None).await;
    assert!(r.get("echo").is_none(), "ed may only ping — it is ed, not user: {r}");
    // ro holds no grant on echo: the KERNEL's refusal comes back verbatim
    let r = tab(&k, json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}, "token": "t-ro"}), None).await;
    assert!(r.get("error").is_some() && r.get("echo").is_none(), "{r}");
    let r = tab(&k, json!({"type": "reflect", "token": "t-ro"}), None).await;
    assert_eq!(r["as"], "ro");
    assert!(r["can_call"].get(format!("{G}|cv1")).is_some(), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_tree_is_filtered_and_written_by_the_users_page_grants() {
    let k = setup().await;
    let load = json!({"type": "forward", "target": "loader", "payload": {"type": "load_tree"}});
    let mut ro = load.clone();
    ro["token"] = json!("t-ro");
    assert_eq!(ids(&tab(&k, ro, None).await), vec!["c1", "canvas", "cv1"], "a grant on cv1 covers it and what is under it, and the path up to the root");
    // ro holds only reflect: no write
    let w = |tok: &str, id: &str, parent: &str| json!({"type": "forward", "target": "loader", "token": tok, "tab": "T1",
        "payload": {"type": "persist_record", "record": {"id": id, "parent_id": parent, "x": 5}}});
    let r = tab(&k, w("t-ro", "c1", "cv1"), None).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    // ed writes under cv1, not under cv2
    let r = tab(&k, w("t-ed", "c1", "cv1"), None).await;
    assert_eq!(r["ok"], true, "{r}");
    let r = tab(&k, w("t-ed", "c2", "cv2"), None).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    // a new cell under cv1: allowed (inherited)
    let r = tab(&k, w("t-ed", "c3", "cv1"), None).await;
    assert_eq!(r["ok"], true, "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_accepted_write_reaches_every_user_that_sees_it_through_the_events_pull() {
    let k = setup().await;
    let r = tab(&k, json!({"type": "events", "token": "t-ro", "tab": "R", "wait": 0.1}), None).await;
    assert_eq!(r["events"], json!([]), "{r}");
    let from = r["next"].as_u64().unwrap();
    // a reader waits; the write lands meanwhile
    let k2 = Arc::clone(&k);
    let waiter = tokio::spawn(async move { tab(&k2, json!({"type": "events", "token": "t-ro", "tab": "R", "after": from, "wait": 5}), None).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let r = tab(&k, json!({"type": "forward", "target": "loader", "token": "t-ed", "tab": "E",
        "payload": {"type": "persist_record", "record": {"id": "c1", "parent_id": "cv1", "x": 9}}}), None).await;
    assert_eq!(r["ok"], true, "{r}");
    let got = waiter.await.unwrap();
    let ev = &got["events"][0];
    assert_eq!(ev["type"], "record_changed", "{got}");
    assert_eq!(ev["id"], "c1");
    assert_eq!(ev["record"]["x"], 9);
    assert_eq!(ev["origin"], "E", "the writing tab skips its own echo by this");
    // a write ro cannot see never reaches it
    let r = tab(&k, json!({"type": "forward", "target": "loader", "tab": "A",
        "payload": {"type": "persist_record", "record": {"id": "c2", "parent_id": "cv2", "x": 1}}}), None).await;
    assert_eq!(r["ok"], true, "the default user writes anywhere: {r}");
    let after = got["next"].as_u64().unwrap();
    let r = tab(&k, json!({"type": "events", "token": "t-ro", "tab": "R", "after": after, "wait": 0.2}), None).await;
    assert_eq!(r["events"], json!([]), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_inner_marks_are_re_rooted_and_the_token_never_travels_on() {
    let k = setup().await;
    let r = tab(&k, json!({"type": "forward", "target": "echo", "token": "t-ed", "payload": {"type": "ping", "k": "v"}}),
        Some(json!(["payload.token", "payload.payload.k"]))).await;
    assert_eq!(r["marks"], json!(["payload.k"]), "{r}");
    assert!(r["echo"].get("token").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watch_is_gated_as_the_user() {
    let k = setup().await;
    let r = tab(&k, json!({"type": "watch_agent", "token": "t-ro", "target": "echo", "tab": "R"}), None).await;
    assert!(r.get("error").is_some(), "ro holds no watch on echo: {r}");
    let r = tab(&k, json!({"type": "watch_agent", "token": "t-ed", "target": "echo", "tab": "E"}), None).await;
    assert_eq!(r["watching"], "echo", "{r}");
    let r = tab(&k, json!({"type": "unwatch_agent", "token": "t-ed", "target": "echo", "tab": "E"}), None).await;
    assert_eq!(r["unwatched"], "echo", "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_agents_shows_only_the_host_agents_a_users_grants_name() {
    let k = setup().await;
    let root = k.root().unwrap().id.0.clone();
    op(&k, "kernel", json!({"type": "update_agent", "id": "ed", "can_call": {"echo": ["ping"], root.as_str(): ["list_agents"]}})).await;
    let mut cb = k.agents.get(&AgentId::from(root.as_str())).unwrap().record().meta.get("callable_by").cloned().unwrap_or(json!({}));
    cb["ed"] = json!(["list_agents"]);
    op(&k, "kernel", json!({"type": "update_agent", "id": root, "callable_by": cb})).await;
    let r = tab(&k, json!({"type": "forward", "target": "kernel", "token": "t-ed", "payload": {"type": "list_agents"}}), None).await;
    let names: Vec<&str> = r["agents"].as_array().unwrap_or_else(|| panic!("{r}")).iter().filter_map(|a| a["id"].as_str()).collect();
    assert_eq!(names, vec![root.as_str(), "echo"], "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn around_answers_from_the_page_records_by_the_callers_page_grants() {
    let k = setup().await;
    // the default sees the root's cells (cv1, cv2 at 0,0 100×50), nearest first
    let r = tab(&k, json!({"type": "around", "x": 50, "y": 25, "radius": 10}), None).await;
    let ids: Vec<&str> = r["members"].as_array().unwrap_or_else(|| panic!("{r}")).iter().filter_map(|m| m["id"].as_str()).collect();
    assert_eq!(ids, vec!["cv1", "cv2"], "{r}");
    assert_eq!(r["members"][0]["distance"], 0.0);
    assert_eq!(r["members"][0]["kind"], "x.ts");
    // far away: nothing
    let r = tab(&k, json!({"type": "around", "x": 1000, "y": 1000, "radius": 10}), None).await;
    assert_eq!(r["members"], json!([]));
    // ro sees only cv1 (and what is under it): on the root, cv1 alone
    let r = tab(&k, json!({"type": "around", "token": "t-ro", "x": 50, "y": 25, "radius": 10}), None).await;
    let ids: Vec<&str> = r["members"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
    assert_eq!(ids, vec!["cv1"], "{r}");
    // inside cv1: its cell
    let r = tab(&k, json!({"type": "around", "token": "t-ro", "canvas": "cv1", "x": 0, "y": 0, "radius": 5}), None).await;
    assert_eq!(r["members"][0]["id"], "c1", "{r}");
    // bad args
    let r = tab(&k, json!({"type": "around", "x": "a", "y": 0, "radius": 5}), None).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agents_own_around_is_judged_by_its_own_page_grants() {
    let k = setup().await;
    let root = k.root().unwrap().id.0.clone();
    // a cell's AI: may call the gateway, sees cv2 only
    op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "ai_1", "handler_module": "plain.tools",
        "can_call": {"users": ["around"], format!("{G}|cv2"): ["reflect"]}})).await;
    let mut cb = k.agents.get(&AgentId::from("users")).unwrap().record().meta.get("callable_by").cloned().unwrap();
    cb["ai_1"] = json!(["around"]);
    op(&k, "kernel", json!({"type": "update_agent", "id": "users", "callable_by": cb})).await;
    let k2 = Arc::clone(&k);
    let item = Item::new(AgentId::from("users"), json!({"type": "around", "x": 50, "y": 25, "radius": 10}));
    let r = fantastic_kernel::with_sender(AgentId::from("ai_1"), async move { k2.send(vec![item]).await }).await.remove(0).result;
    let ids: Vec<&str> = r["members"].as_array().unwrap_or_else(|| panic!("{r}")).iter().filter_map(|m| m["id"].as_str()).collect();
    assert_eq!(ids, vec!["cv2"], "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_state_stream_reaches_a_subscribed_tab_filtered_by_its_grants_and_never_blocks_the_kernel() {
    let k = setup().await;
    let root = k.root().unwrap().id.0.clone();
    // the default may tap the state stream (a root grant, both sides)
    let mut cc = k.agents.get(&AgentId::from("user")).unwrap().record().meta.get("can_call").cloned().unwrap();
    cc[root.as_str()] = json!(["state_subscribe"]);
    op(&k, "kernel", json!({"type": "update_agent", "id": "user", "can_call": cc})).await;
    let mut cb = k.agents.get(&AgentId::from(root.as_str())).unwrap().record().meta.get("callable_by").cloned().unwrap_or(json!({}));
    cb["user"] = json!(["state_subscribe"]);
    op(&k, "kernel", json!({"type": "update_agent", "id": root, "callable_by": cb})).await;
    let r = tab(&k, json!({"type": "state_subscribe", "tab": "S"}), None).await;
    assert_eq!(r["subscribed"], true, "{r}");
    let from = tab(&k, json!({"type": "events", "tab": "S", "wait": 0.1}), None).await["next"].as_u64().unwrap();
    // a change the kernel publishes while it works on its own agents
    for i in 0..20 {
        let r = op(&k, "kernel", json!({"type": "update_agent", "id": "echo", "note": i})).await;
        assert!(r.get("error").is_none(), "{r}");
    }
    let got = tab(&k, json!({"type": "events", "tab": "S", "after": from, "wait": 3}), None).await;
    let evs = got["events"].as_array().unwrap_or_else(|| panic!("{got}"));
    assert!(evs.iter().any(|e| e["type"] == "state_event" && e["payload"]["agent_id"] == "echo"), "{got}");
    // an agent the default's grants do not name (the loader) never shows
    op(&k, "kernel", json!({"type": "update_agent", "id": "loader", "note": 1})).await;
    let got = tab(&k, json!({"type": "events", "tab": "S", "after": got["next"], "wait": 0.5}), None).await;
    assert!(!got["events"].as_array().unwrap().iter().any(|e| e["payload"]["agent_id"] == "loader"), "{got}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_pull_starts_at_the_head_and_a_reply_carries_at_most_a_batch() {
    let k = setup().await;
    // 250 accepted writes queue 250 changes for the default
    for i in 0..250 {
        let r = tab(&k, json!({"type": "forward", "target": "loader", "tab": "W",
            "payload": {"type": "persist_record", "record": {"id": format!("n{i}"), "parent_id": "canvas"}}}), None).await;
        assert_eq!(r["ok"], true, "{r}");
    }
    // a new tab: no history, the head at once
    let r = tab(&k, json!({"type": "events", "tab": "N"}), None).await;
    assert_eq!(r["events"], json!([]), "{r}");
    assert_eq!(r["next"], 250);
    // an old cursor: at most a batch
    let r = tab(&k, json!({"type": "events", "tab": "O", "after": 0, "wait": 0}), None).await;
    assert_eq!(r["events"].as_array().unwrap().len(), fantastic_canvas_users::BATCH, "{}", r["next"]);
}
