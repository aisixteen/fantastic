//! The `user` bundle against a real in-memory kernel (the real gate, the real
//! secret marks). The fakes: a page loader holding records in RAM, and an
//! echo agent that answers with what it was given.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    bundle::{BundleError, Handler},
    AgentId, BundleRegistry, Item, Kernel, Reply,
};
use fantastic_user::{UserBundle, HANDLER_MODULE};
use serde_json::{json, Value};

const G: &str = "g0";

fantastic_kernel::stand_in_verbs!(loader_verbs {
    LoadTree "load_tree" {},
    PersistRecord "persist_record" { record: serde_json::Value },
    ForgetRecord "forget_record" { id: serde_json::Value },
});

/// Page records in RAM: load_tree / persist_record / forget_record.
#[derive(Default)]
struct Loader(Mutex<Vec<Value>>);

#[async_trait]
impl Handler for Loader {
    type Call = loader_verbs::Call;
    fn name(&self) -> &str {
        "loader"
    }
    fn man(&self) -> &'static str {
        "A test stand-in that holds page records in RAM."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        loader_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        loader_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }
    async fn handle(
        &self,
        id: &AgentId,
        call: Self::Call,
        _k: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let mut recs = self.0.lock().unwrap();
        Ok(Some(match call {
            loader_verbs::Call::LoadTree(_) => json!({"records": *recs}),
            loader_verbs::Call::PersistRecord(p) => {
                let r = p.record;
                recs.retain(|x| x["id"] != r["id"]);
                recs.push(r);
                json!({"ok": true})
            }
            loader_verbs::Call::ForgetRecord(p) => {
                recs.retain(|x| x["id"] != p.id);
                json!({"ok": true})
            }
            loader_verbs::Call::Reflect(_) => json!({"id": id.as_str(), "sentence": "a loader"}),
        }))
    }
}

fantastic_kernel::stand_in_verbs!(echo_verbs { Ping "ping" {}, Wait "wait" { wait_s: f64 } });

/// Echo: the payload it was called with. Only `user` may call it.
struct Echo;

#[async_trait]
impl Handler for Echo {
    type Call = echo_verbs::Call;
    fn name(&self) -> &str {
        "echo"
    }
    fn man(&self) -> &'static str {
        "A test stand-in that answers with what it was given."
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
        _id: &AgentId,
        call: Self::Call,
        _k: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(Some(json!({"echo": call.payload()})))
    }
}

fantastic_kernel::stand_in_verbs!(relay_verbs { Relay "relay" { to: String, inner: serde_json::Value } });

/// A LEG: what arrives at it is sent onward from its own handler, as the leg —
/// the way a wire call reaches the host (a call to the leg, which sends on).
struct Relay;

#[async_trait]
impl Handler for Relay {
    type Call = relay_verbs::Call;
    fn name(&self) -> &str {
        "relay"
    }
    fn man(&self) -> &'static str {
        "A test stand-in leg that sends what it is given onward."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        relay_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        relay_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }
    async fn handle(
        &self,
        id: &AgentId,
        call: Self::Call,
        k: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let relay_verbs::Call::Relay(r) = call else {
            return Ok(Some(json!({"id": id.as_str(), "sentence": "a leg"})));
        };
        let item = Item::new(AgentId::from(r.to.as_str()), r.inner);
        Ok(Some(Arc::clone(k).send(vec![item]).await.remove(0).result))
    }
}

/// The shell: a send from outside every agent (ungated).
async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

/// What a TAB does: its call reaches the leg, which sends it on to `user`.
async fn tab(k: &Arc<Kernel>, payload: Value) -> Value {
    op(
        k,
        "leg",
        json!({"type": "relay", "to": "user", "inner": payload}),
    )
    .await
}

/// What an AI cell does: `agent` sends to `user` from its own handler.
async fn as_agent(k: &Arc<Kernel>, agent: &str, payload: Value) -> Value {
    op(
        k,
        agent,
        json!({"type": "relay", "to": "user", "inner": payload}),
    )
    .await
}

fn rec(id: &str, parent: &str) -> Value {
    json!({"id": id, "parent_id": parent, "handler_module": "x.ts"})
}

/// The page's free-canvas layout under `view`: each cell's rect (never on the
/// cell's record). One per page kernel, as the page keeps it (`layout.ts`).
fn layout(view: &str, cells: &[&str]) -> Value {
    let rects: serde_json::Map<String, Value> = cells
        .iter()
        .map(|c| {
            (
                (*c).to_string(),
                json!({"x": 0, "y": 0, "width": 100, "height": 50}),
            )
        })
        .collect();
    json!({"id": "layout_canvas", "parent_id": view, "handler_module": "layout.ts", "rects": rects})
}

/// root ─ leg · echo · user {loader}; the page tree canvas {view_root {cv1 {c1}, cv2}},
/// the root view's rects in its `layout_canvas`.
async fn setup() -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, UserBundle::default());
    let loader = Loader::default();
    {
        let mut r = loader.0.lock().unwrap();
        let mut root = rec("canvas", "loader");
        root["guid"] = json!(G);
        r.extend([
            root,
            rec("view_root", "canvas"),
            rec("cv1", "view_root"),
            rec("c1", "cv1"),
            rec("cv2", "view_root"),
            layout("view_root", &["cv1", "cv2"]),
        ]);
    }
    reg.register("loader.tools", loader);
    reg.register("echo.tools", Echo);
    reg.register("plain.tools", Echo);
    reg.register("relay.tools", Relay);
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let mk = |id: &str, parent: &str, hm: &str, extra: Value| {
        let mut p =
            json!({"type": "create_agent", "parent_id": parent, "id": id, "handler_module": hm});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        p
    };
    for p in [
        mk(
            "leg",
            &root,
            "relay.tools",
            json!({"can_call": {"user": "*", "*|*": ["*"]}}),
        ),
        mk(
            "echo",
            &root,
            "echo.tools",
            json!({"callable_by": {"user": "*"}}),
        ),
        mk(
            "user",
            &root,
            HANDLER_MODULE,
            json!({"page_loader": "loader", "callable_by": {"leg": "*"},
            "can_call": {"echo": "*", "loader": "*", "*|*": ["*"]}}),
        ),
        // the page loader sits under `user` (the tree): its record verbs serve its parent
        mk(
            "loader",
            "user",
            "loader.tools",
            json!({"callable_by": {"user": "*"}}),
        ),
    ] {
        let r = op(&k, &root, p.clone()).await;
        assert!(r.get("error").is_none(), "{p}: {r}");
    }
    k
}

fn ids(r: &Value) -> Vec<String> {
    let mut v: Vec<String> = r["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_path_verb_names_the_running_fantastic_as_fantastic() {
    // #216: the menu tile for `fantastic` must not depend on PATH. Put a DIFFERENT
    // executable named `fantastic` first on PATH; `executables.fantastic` must still
    // be the running binary (`self`).
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("fgw_fake_fantastic_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("fantastic");
    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = std::ffi::OsString::from(&dir);
    new_path.push(":");
    new_path.push(&old_path);
    std::env::set_var("PATH", &new_path);

    let k = setup().await;
    let r = op(&k, "user", json!({"type": "path", "rescan": true})).await;

    std::env::set_var("PATH", &old_path);
    let _ = std::fs::remove_dir_all(&dir);

    let me = r["self"].as_str().unwrap_or_default().to_string();
    assert!(!me.is_empty(), "path reply names self: {r}");
    assert_ne!(me, fake.to_string_lossy(), "self must be the running binary: {r}");
    assert_eq!(
        r["executables"]["fantastic"].as_str(),
        Some(me.as_str()),
        "executables.fantastic must be the running binary, not the PATH copy: {r}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tab_forwards_through_user() {
    let k = setup().await;
    let r = tab(
        &k,
        json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}}),
    )
    .await;
    assert_eq!(r["echo"]["type"], "ping", "only `user` may call echo: {r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forwarded_call_written_as_text_takes_its_target_verbs_types() {
    let k = setup().await;
    let r = tab(
        &k,
        json!({"type": "forward", "target": "echo", "payload": {"type": "wait", "wait_s": "30"}}),
    )
    .await;
    assert_eq!(r["echo"]["wait_s"].as_f64(), Some(30.0), "the text `30` is the number 30 here: {r}");
    let r = tab(
        &k,
        json!({"type": "forward", "target": "echo", "payload": {"type": "wait", "wait_s": "soon"}}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "a value that is not a number is refused by name: {r}");
    assert!(r.to_string().contains("wait_s"), "the refusal names the argument: {r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_page_loader_under_user_is_reached_whole_and_writable() {
    let k = setup().await;
    let r = tab(
        &k,
        json!({"type": "forward", "target": "loader", "payload": {"type": "load_tree"}}),
    )
    .await;
    assert_eq!(
        ids(&r),
        vec!["c1", "canvas", "cv1", "cv2", "layout_canvas", "view_root"],
        "{r}"
    );
    let r = tab(
        &k,
        json!({"type": "forward", "target": "loader",
        "payload": {"type": "persist_record", "record": {"id": "c9", "parent_id": "cv2"}}}),
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn around_answers_from_the_page_records_by_the_callers_page_grants() {
    let k = setup().await;
    // a tab is judged by the leg's page grants (`*|*`: every page agent): the root's cells, nearest first
    let r = tab(
        &k,
        json!({"type": "around", "x": 50, "y": 25, "radius": 10}),
    )
    .await;
    let got: Vec<&str> = r["members"]
        .as_array()
        .unwrap_or_else(|| panic!("{r}"))
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(got, vec!["cv1", "cv2"], "{r}");
    assert_eq!(r["members"][0]["distance"], 0.0);
    // a cell with no rect in the layout is not placed there: none found
    let r = tab(
        &k,
        json!({"type": "around", "view": "cv1", "x": 0, "y": 0, "radius": 5}),
    )
    .await;
    assert_eq!(r["members"], json!([]), "{r}");
    let r = tab(
        &k,
        json!({"type": "around", "theme": "mobile", "x": 50, "y": 25, "radius": 10}),
    )
    .await;
    assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("theme")), "an unknown argument is refused: {r}");
    let r = tab(&k, json!({"type": "around", "x": "a", "y": 0, "radius": 5})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    // a cell's AI: judged by its own page grants (cv2 only)
    let root = k.root().unwrap().id.0.clone();
    op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "ai_1", "handler_module": "relay.tools",
        "can_call": {"user": ["around"], format!("{G}|cv2"): ["reflect"]}})).await;
    let mut cb = k
        .agents
        .get(&AgentId::from("user"))
        .unwrap()
        .record()
        .meta
        .get("callable_by")
        .cloned()
        .unwrap();
    cb["ai_1"] = json!(["around"]);
    op(
        &k,
        "kernel",
        json!({"type": "update_agent", "id": "user", "callable_by": cb}),
    )
    .await;
    let r = as_agent(
        &k,
        "ai_1",
        json!({"type": "around", "x": 50, "y": 25, "radius": 10}),
    )
    .await;
    let got: Vec<&str> = r["members"]
        .as_array()
        .unwrap_or_else(|| panic!("{r}"))
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(got, vec!["cv2"], "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shell_forwards_and_user_answers_its_own_verbs() {
    let k = setup().await;
    // the shell (no origin)
    let r = op(
        &k,
        "user",
        json!({"type": "forward", "target": "echo", "payload": {"type": "ping"}}),
    )
    .await;
    assert_eq!(r["echo"]["type"], "ping", "{r}");
    // its inquiries and its reflect
    let r = op(&k, "user", json!({"type": "inquiries"})).await;
    assert!(r.get("inquiries").is_some(), "{r}");
    let r = op(&k, "user", json!({"type": "reflect"})).await;
    assert_eq!(r["handler_module"], HANDLER_MODULE, "{r}");
    assert!(r["verbs"].get("forward").is_some(), "a reflect renders the declared verbs: {r}");
    // HOW TO CALL it: the kernel's `docs`, rendered from the declared verbs
    let d = op(&k, "kernel", json!({"type": "docs", "of": "user"})).await;
    assert!(
        d["verbs"].get("forward").is_some()
            && d["verbs"].get("around").is_some()
            && d["verbs"].get("inquiry").is_some(),
        "{d}"
    );
}

fn why(r: &Value) -> (Option<&str>, Option<&str>) {
    (r["reason"].as_str(), r["arg"].as_str())
}

#[tokio::test(flavor = "multi_thread")]
async fn every_verb_of_user_is_checked_by_its_declaration() {
    let k = setup().await;
    let r = op(&k, "user", json!({"type": "zzz_nope"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    for v in fantastic_user::verbs::VERBS.iter().map(|v| v.name).chain(["reflect"]) {
        assert!(r["error"].as_str().unwrap().contains(v), "{v} in {r}");
    }
    for v in fantastic_user::verbs::VERBS {
        let r = op(&k, "user", json!({"type": v.name, "zzz_unknown": 1})).await;
        assert_eq!(why(&r), (Some("bad_arg"), Some("zzz_unknown")), "{}: {r}", v.name);
    }
    // a verb's required argument missing is refused by its name
    for v in fantastic_user::verbs::VERBS {
        let required: Vec<String> = fantastic_kernel::verbs::args(v.schema())
            .into_iter()
            .filter(|a| a.required)
            .map(|a| a.name)
            .collect();
        if required.is_empty() {
            continue;
        }
        let r = op(&k, "user", json!({"type": v.name})).await;
        assert_eq!(r["reason"], "missing_arg", "{}: {r}", v.name);
        assert!(required.iter().any(|n| r["arg"] == json!(n)), "{}: {r}", v.name);
    }
    // wrongly typed, never coerced
    for (p, arg) in [
        (json!({"type": "forward", "target": "echo", "payload": "x"}), "payload"),
        (json!({"type": "forward", "target": "echo", "payload": {}, "timeout": "5"}), "timeout"),
        (json!({"type": "around", "x": 1, "y": 2, "radius": 0}), "radius"),
        (json!({"type": "around", "x": 1, "y": 2, "radius": 5, "view": 3}), "view"),
        (json!({"type": "path", "rescan": "yes"}), "rescan"),
        (json!({"type": "inquiry", "text": "q", "kind": "poll"}), "kind"),
        (json!({"type": "inquiries", "asker": 3}), "asker"),
        (json!({"type": "answer_inquiry", "re": 1, "answer": 3}), "answer"),
        (json!({"type": "answer_inquiry", "re": 1, "answer": ""}), "answer"),
        (json!({"type": "import_subtree", "path": "x", "parent_id": "p", "approve": "true"}), "approve"),
    ] {
        let r = op(&k, "user", p.clone()).await;
        assert_eq!(why(&r), (Some("bad_arg"), Some(arg)), "{p}: {r}");
    }
    // the record declares `page_loader` and no other field
    let root = k.root().unwrap().id.0.clone();
    for p in [
        json!({"type": "update_agent", "id": "user", "page_loader": ""}),
        json!({"type": "update_agent", "id": "user", "stray": 1}),
    ] {
        let r = op(&k, &root, p.clone()).await;
        assert_eq!(r["reason"], "bad_arg", "{p}: {r}");
    }
    // an answer to a question nobody asked is not found
    let r = op(&k, "user", json!({"type": "answer_inquiry", "re": 99, "answer": "yes"})).await;
    assert_eq!(r["reason"], "not_found", "{r}");
}
