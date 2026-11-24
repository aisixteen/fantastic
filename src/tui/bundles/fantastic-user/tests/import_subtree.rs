//! `import_subtree` against a real in-memory kernel: export, then import.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    bundle::{BundleError, Handler},
    AgentId, BundleRegistry, Item, Kernel, Reply,
};
use fantastic_user::export::strip_runtime;
use fantastic_user::{verbs::VERBS, UserBundle, HANDLER_MODULE};
use serde_json::{json, Map, Value};

fantastic_kernel::stand_in_verbs!(echo_verbs { Ping "ping" {}, Run "run" {} });

/// Answers with what it was given.
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

/// The shell: a send from outside every agent.
async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .operator_send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

/// A send made by `agent`'s own code: the gate judges it.
async fn from(k: &Arc<Kernel>, agent: &str, target: &str) -> Value {
    call(k, agent, target, "ping").await
}

/// `agent` calls `verb` on `target`.
async fn call(k: &Arc<Kernel>, agent: &str, target: &str, verb: &str) -> Value {
    fantastic_kernel::test_support::as_agent(agent, op(k, target, json!({"type": verb}))).await
}

fn meta(k: &Kernel, id: &str) -> Map<String, Value> {
    k.resolve_target(id)
        .unwrap_or_else(|| panic!("no agent {id}"))
        .record()
        .meta
}

fn parent(k: &Kernel, id: &str) -> Option<String> {
    k.resolve_target(id)
        .and_then(|a| a.record().parent_id)
}

/// root ─ user {memory, memory2 (stores), exec, other} · a {b {c}}: a and c
/// reach memory, b reaches exec; each line is in its holder's record. Every
/// ancestor's lines cap the lines below it: `a` holds the exec line its child `b`
/// uses, `b` the store line its child `c` uses, and `other`, where the imports
/// go, the bound ports' lines.
async fn setup() -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, UserBundle::default());
    reg.register("sqlite_store.tools", Echo);
    reg.register("exec_bridge.tools", Echo);
    reg.register("echo.tools", Echo);
    // A disk kernel: an export or import names a path under its work folder.
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let work = std::env::temp_dir().join(format!(
        "fu-setup-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    reg.register(
        fantastic_core::HANDLER_MODULE,
        fantastic_core::KernelStateBundle::default(),
    );
    let k = Arc::clone(
        &bootstrap(
            reg,
            BootstrapOptions::one_shot(work.join(".fantastic"), &work),
        )
        .expect("boots")
        .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    for (id, parent, hm, extra) in [
        ("user", root.as_str(), HANDLER_MODULE, json!({})),
        (
            "memory",
            "user",
            "sqlite_store.tools",
            json!({}),
        ),
        ("memory2", "user", "sqlite_store.tools", json!({})),
        (
            "exec",
            "user",
            "exec_bridge.tools",
            json!({}),
        ),
        (
            "other",
            "user",
            "echo.tools",
            json!({"can": {"memory2": {"call": ["*"]}, "exec": {"call": ["run"]}}}),
        ),
        (
            "a",
            "user",
            "echo.tools",
            json!({"can": {"memory": {"call": ["*"]}, "b": {"call": ["*"]}, "exec": {"call": ["run"]}}, "memory_store_id": "memory",
            "credentials": {"store": "memory", "ref": "r1"}, "notes": "top"}),
        ),
        (
            "b",
            "a",
            "echo.tools",
            json!({"can": {"exec": {"call": ["run"]}, "memory": {"call": ["*"]}}, "cwd": "."}),
        ),
        (
            "c",
            "b",
            "echo.tools",
            json!({"can": {"memory": {"call": ["*"]}}, "notes": "plain"}),
        ),
    ] {
        let mut p =
            json!({"type": "create_agent", "parent_id": parent, "id": id, "handler_module": hm});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let r = op(&k, &root, p.clone()).await;
        assert!(r.get("error").is_none(), "{p}: {r}");
    }
    k
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fu-import-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Export `a` into a fresh folder.
async fn exported(k: &Arc<Kernel>, tag: &str) -> PathBuf {
    let d = dir(tag);
    let r = op(
        k,
        "user",
        json!({"type": "export_subtree", "agent_id": "a", "path": d.to_str().unwrap()}),
    )
    .await;
    assert_eq!(r["exported"], 3, "{r}");
    assert_eq!(r["ports"], json!(["@exec", "@memory"]), "{r}");
    d
}

/// A hand-written blueprint folder.
fn written(tag: &str, yaml: &str, files: &[(&str, &str)]) -> PathBuf {
    let d = dir(tag);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("blueprint.yaml"), yaml).unwrap();
    for (f, t) in files {
        let p = d.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    }
    d
}

async fn import(k: &Arc<Kernel>, d: &std::path::Path, extra: Value) -> Value {
    let mut p = json!({"type": "import_subtree", "path": d.to_str().unwrap(),
        "parent_id": "other", "bind": {"@memory": "memory2", "@exec": "exec"}});
    p.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    op(k, "user", p).await
}

/// `v` with every line target and id field renamed by `ids`.
fn renamed(m: &Map<String, Value>, ids: &[(&str, &str)]) -> Map<String, Value> {
    let r = |s: &str| {
        ids.iter()
            .find(|(a, _)| *a == s)
            .map_or_else(|| s.to_string(), |(_, b)| (*b).to_string())
    };
    let mut out = m.clone();
    if let Some(Value::Object(o)) = m.get("can") {
        out.insert(
            "can".into(),
            Value::Object(o.iter().map(|(k, v)| (r(k), v.clone())).collect()),
        );
    }
    if let Some(Value::String(t)) = m.get("memory_store_id") {
        out.insert("memory_store_id".into(), json!(r(t)));
    }
    if let Some(Value::String(t)) = m.get("credentials").and_then(|c| c.get("store")) {
        out["credentials"]["store"] = json!(r(t));
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn import_without_approve_creates_nothing_and_shows_reaches() {
    let k = setup().await;
    let d = exported(&k, "preview").await;
    let n = k.agent_ids().len();
    let r = import(&k, &d, json!({})).await;
    assert_eq!(r["approval_required"], true, "{r}");
    assert_eq!(r["agents"], 3, "{r}");
    assert_eq!(
        r["new_ids"],
        json!({"a": "a_1", "b": "b_1", "c": "c_1"}),
        "{r}"
    );
    assert_eq!(
        r["ports"],
        json!({"@exec": "exec", "@memory": "memory2"}),
        "{r}"
    );
    assert_eq!(r["code"], json!([]), "{r}");
    let reaches = r["reaches"].as_array().unwrap();
    assert_eq!(reaches.len(), 3);
    assert_eq!(reaches[1]["id"], "b_1");
    assert_eq!(reaches[1]["ports"], json!({"@exec": "exec", "@memory": "memory2"}), "{r}");
    assert_eq!(k.agent_ids().len(), n, "nothing created");
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test(flavor = "multi_thread")]
async fn export_then_import_round_trips_with_fresh_ids() {
    let k = setup().await;
    let d = exported(&k, "round").await;
    let r = import(&k, &d, json!({"approve": true})).await;
    assert_eq!(r["imported"], 3, "{r}");
    assert_eq!(r["parent_id"], "other");
    assert_eq!(parent(&k, "a_1").as_deref(), Some("other"));
    assert_eq!(parent(&k, "b_1").as_deref(), Some("a_1"));
    assert_eq!(parent(&k, "c_1").as_deref(), Some("b_1"));
    let ids = [
        ("a", "a_1"),
        ("b", "b_1"),
        ("c", "c_1"),
        ("memory", "memory2"),
    ];
    for (old, new) in &ids[..3] {
        assert_eq!(
            strip_runtime(&meta(&k, new)),
            renamed(&strip_runtime(&meta(&k, old)), &ids),
            "{old} → {new}"
        );
    }
    // the new agents hold their lines on the bound agents; the bound agents keep no list
    assert_eq!(meta(&k, "a_1")["can"]["memory2"], json!({"call": ["*"]}));
    assert_eq!(meta(&k, "b_1")["can"]["exec"], json!({"call": ["run"]}));
    assert!(meta(&k, "memory2").get("can").is_none());
    // the originals are as they were
    assert_eq!(meta(&k, "a")["can"]["b"], json!({"call": ["*"]}));
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test(flavor = "multi_thread")]
async fn import_twice_under_same_parent_gives_two_independent_copies() {
    let k = setup().await;
    let d = exported(&k, "twice").await;
    let one = import(&k, &d, json!({"approve": true})).await;
    let two = import(&k, &d, json!({"approve": true})).await;
    assert_eq!(one["ids"]["a"], "a_1", "{one}");
    assert_eq!(
        two["ids"],
        json!({"a": "a_2", "b": "b_2", "c": "c_2"}),
        "{two}"
    );
    for id in ["a_2", "b_2", "c_2"] {
        let m = meta(&k, id);
        let text = Value::Object(m).to_string();
        for first in ["a_1", "b_1", "c_1"] {
            assert!(!text.contains(first), "{id} names {first}: {text}");
        }
    }
    assert_eq!(meta(&k, "a_2")["can"]["b_2"], json!({"call": ["*"]}));
    // change one copy, delete it: the other stays as it was
    let root = k.root().unwrap().id.0.clone();
    let r = op(
        &k,
        &root,
        json!({"type": "update_agent", "id": "c_1", "notes": "changed"}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(meta(&k, "c_2")["notes"], "plain");
    let r = op(&k, &root, json!({"type": "delete_agent", "id": "a_1"})).await;
    assert_eq!(r["deleted"], true, "{r}");
    for id in ["a_1", "b_1", "c_1"] {
        assert!(parent(&k, id).is_none(), "{id} gone");
    }
    assert_eq!(parent(&k, "c_2").as_deref(), Some("b_2"));
    assert_eq!(
        from(&k, "c_2", "memory2").await["echo"]["type"],
        "ping",
        "copy 2 still reaches its store"
    );
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test(flavor = "multi_thread")]
async fn import_refuses_missing_binding_secret_and_code_with_messages_that_name_the_fix() {
    let k = setup().await;
    let n = k.agent_ids().len();
    let d = exported(&k, "refuse").await;
    let r = op(
        &k,
        "user",
        json!({"type": "import_subtree", "path": d.to_str().unwrap(), "parent_id": "other",
            "bind": {"@exec": "exec"}, "approve": true}),
    )
    .await;
    assert_eq!(r["reason"], "unbound_ports", "{r}");
    let msg = r["error"].as_str().unwrap();
    assert!(
        msg.contains("@memory (memory)") && msg.contains("pass bind"),
        "{msg}"
    );

    let key = "sk-planted0123456789";
    let s = written(
        "secret",
        &format!("agents:\n  - id: a\n    handler_module: echo.tools\n    api_key: {key}\n"),
        &[],
    );
    let r = import(&k, &s, json!({"approve": true, "bind": {}})).await;
    assert_eq!(r["reason"], "secret_found", "{r}");
    let msg = r["error"].as_str().unwrap();
    assert!(
        msg.contains("agent a: a api_key key") && msg.contains("export again"),
        "{msg}"
    );
    assert!(!msg.contains(key), "{msg}");

    let c = written(
        "code",
        "agents:\n  - id: s\n    handler_module: py_ab12.tools\n",
        &[("s/bundle.yaml", "name: s\n"), ("s/tools.py", "print(1)\n")],
    );
    let r = import(&k, &c, json!({"approve": true, "bind": {}})).await;
    assert_eq!(r["reason"], "code_not_allowed", "{r}");
    let msg = r["error"].as_str().unwrap();
    assert!(
        msg.contains("py_ab12.tools")
            && msg.contains("s/tools.py")
            && msg.contains("allow_code: true"),
        "{msg}"
    );
    assert_eq!(k.agent_ids().len(), n, "nothing created");
    // allowed, a handler this kernel does not run is still refused when the agent is
    // made (`create_agent` needs an installed bundle), and nothing is kept
    let r = import(
        &k,
        &c,
        json!({"approve": true, "bind": {}, "allow_code": true}),
    )
    .await;
    assert_eq!(r["reason"], "failed", "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("handler_module 'py_ab12.tools' is not installed"),
        "{r}"
    );
    assert_eq!(k.agent_ids().len(), n, "nothing kept");
    // allowed, with the handler installed, the code comes in
    let ok = written(
        "code-ok",
        "agents:\n  - id: s\n    handler_module: echo.tools\n",
        &[("s/bundle.yaml", "name: s\n"), ("s/tools.py", "print(1)\n")],
    );
    let r = import(
        &k,
        &ok,
        json!({"approve": true, "bind": {}, "allow_code": true}),
    )
    .await;
    assert_eq!(r["imported"], 1, "{r}");
    for x in [d, s, c, ok] {
        let _ = std::fs::remove_dir_all(x);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn imported_agents_call_only_what_they_were_granted_and_what_ports_were_bound() {
    let k = setup().await;
    let d = exported(&k, "gate").await;
    let r = import(&k, &d, json!({"approve": true})).await;
    assert_eq!(r["imported"], 3, "{r}");
    // the bound store answers
    assert_eq!(from(&k, "c_1", "memory2").await["echo"]["type"], "ping");
    assert_eq!(from(&k, "a_1", "memory2").await["echo"]["type"], "ping");
    assert_eq!(call(&k, "b_1", "exec", "run").await["echo"]["type"], "run");
    // b was granted only `run` on exec
    let r = from(&k, "b_1", "exec").await;
    assert!(r.get("error").is_some(), "{r}");
    // the original store was not bound: refused
    let r = from(&k, "c_1", "memory").await;
    assert!(r.get("error").is_some(), "{r}");
    // c was never granted exec, b only `run`
    let r = from(&k, "c_1", "exec").await;
    assert!(r.get("error").is_some(), "{r}");
    // the original agents are not reachable from the copies
    let r = from(&k, "a_1", "b").await;
    assert!(r.get("error").is_some(), "{r}");
    assert_eq!(from(&k, "a_1", "b_1").await["echo"]["type"], "ping");
    let _ = std::fs::remove_dir_all(d);
}

/// THE ASKER'S CREATES (Q39): `leg` holds `import_subtree` on `user` and a `create`
/// line on `other` only. Each create of its import is checked as `leg`, never as
/// `user`: aimed at `other` it lands; aimed anywhere else (a helper beside it, its
/// own ancestor) every create gets the unknown answer and nothing is kept.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_is_checked_as_the_asker_and_refused_outside_its_create_line() {
    let k = setup().await;
    let root = k.root().unwrap().id.0.clone();
    let made = op(
        &k,
        &root,
        json!({"type": "create_agent", "parent_id": "user", "id": "leg", "handler_module": "echo.tools",
            "can": {"user": {"call": ["import_subtree"]}, "other": {"create": {"who": ["*"], "what": "*"}}}}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    let d = written("asker", "agents:\n  - id: x\n    handler_module: echo.tools\n", &[]);
    let by_leg = |parent: &'static str| {
        let k = Arc::clone(&k);
        let path = d.to_str().unwrap().to_string();
        async move {
            fantastic_kernel::test_support::as_agent(
                "leg",
                op(&k, "user", json!({"type": "import_subtree", "path": path, "parent_id": parent, "approve": true})),
            )
            .await
        }
    };
    let n = k.agent_ids().len();
    for outside in ["exec", "user"] {
        let r = by_leg(outside).await;
        let msg = r["error"].as_str().unwrap_or_default();
        assert_eq!(r["reason"], "failed", "{outside}: {r}");
        assert!(
            msg.contains("was not created") && msg.contains(&format!("no agent '{outside}'")) && msg.contains("nothing was kept"),
            "{outside}: {r}"
        );
        assert!(r.get("imported").is_none(), "{outside}: {r}");
        assert_eq!(k.agent_ids().len(), n, "{outside}: nothing was created");
    }
    let r = by_leg("other").await;
    assert_eq!(r["imported"], 1, "{r}");
    assert_eq!(parent(&k, "x_1").as_deref(), Some("other"));
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test(flavor = "multi_thread")]
async fn import_failure_rolls_back() {
    let k = setup().await;
    let n = k.agent_ids().len();
    let d = written(
        "rollback",
        "agents:
  - id: a
    handler_module: echo.tools
    agents:
      - id: b
        handler_module: echo.tools
        agents:
          - id: c
            handler_module: echo.tools
            can: {$parent: {call: 5}}
",
        &[],
    );
    let r = import(&k, &d, json!({"approve": true, "bind": {}})).await;
    assert_eq!(r["reason"], "failed", "{r}");
    assert!(
        r["error"].as_str().unwrap().contains("agent c (c_1)"),
        "{r}"
    );
    assert_eq!(k.agent_ids().len(), n, "no half team stays");
    assert!(parent(&k, "a_1").is_none());
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test(flavor = "multi_thread")]
async fn import_takes_only_a_boolean_for_approve_and_allow_code() {
    let k = setup().await;
    let n = k.agent_ids().len();
    let d = exported(&k, "literal").await;
    let r = import(&k, &d, json!({"approve": "true"})).await;
    assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("approve")), "{r}");
    assert_eq!(k.agent_ids().len(), n);
    let c = written(
        "literal-code",
        "agents:\n  - id: s\n    handler_module: py_ab12.tools\n",
        &[],
    );
    let r = import(
        &k,
        &c,
        json!({"approve": true, "bind": {}, "allow_code": "true"}),
    )
    .await;
    assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("allow_code")), "{r}");
    let r = import(&k, &c, json!({"approve": true, "bind": {}})).await;
    assert_eq!(r["reason"], "code_not_allowed", "{r}");
    assert_eq!(k.agent_ids().len(), n);
    for x in [d, c] {
        let _ = std::fs::remove_dir_all(x);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn import_subtree_args_and_docs() {
    assert!(VERBS.iter().any(|v| v.name == "import_subtree"));
    let k = setup().await;
    for (p, reason) in [
        (json!({"type": "import_subtree", "parent_id": "other"}), "missing_arg"),
        (json!({"type": "import_subtree", "path": "x"}), "missing_arg"),
        (json!({"type": "import_subtree", "path": "x/../y", "parent_id": "other"}), "bad_arg"),
        (json!({"type": "import_subtree", "path": "x", "parent_id": "other", "bind": []}), "bad_arg"),
    ] {
        let r = op(&k, "user", p.clone()).await;
        assert_eq!(r["reason"], reason, "{p}: {r}");
    }
    let r = op(
        &k,
        "user",
        json!({"type": "import_subtree", "path": "x", "parent_id": "nobody"}),
    )
    .await;
    assert_eq!(r["reason"], "not_found", "{r}");
    let d = op(&k, "user", json!({"type": "reflect", "tier": "oneliners"})).await;
    assert!(d["verbs"].get("import_subtree").is_some(), "{d}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_bind_target_is_refused_by_the_shared_engine() {
    let k = setup().await;
    let n = k.agent_ids().len();
    for bind in [json!({"@memory": ""}), json!({"@memory": 3})] {
        let p = json!({"type": "import_subtree", "path": "x", "parent_id": "other", "bind": bind});
        let r = op(&k, "user", p.clone()).await;
        assert_eq!(r["reason"], "bad_arg", "{p}: {r}");
        assert!(r["error"].as_str().is_some_and(|e| e.contains("bind")), "{r}");
    }
    assert_eq!(k.agent_ids().len(), n, "nothing created");
}

#[tokio::test(flavor = "multi_thread")]
async fn import_refuses_bind_to_a_missing_agent_naming_the_port() {
    let k = setup().await;
    let d = exported(&k, "ghost").await;
    let n = k.agent_ids().len();
    for extra in [
        json!({"approve": true, "bind": {"@memory": "ghost", "@exec": "exec"}}),
        json!({"bind": {"@memory": "ghost", "@exec": "exec"}}),
    ] {
        let r = import(&k, &d, extra).await;
        assert_eq!(r["reason"], "bad_binding", "{r}");
        let msg = r["error"].as_str().unwrap();
        assert!(
            msg.contains("@memory") && msg.contains("ghost") && msg.contains("no agent"),
            "{msg}"
        );
        assert_eq!(k.agent_ids().len(), n, "nothing created");
    }
    // a bind key that names no port is refused naming that key
    let r = import(
        &k,
        &d,
        json!({"approve": true, "bind": {"@memory": "memory2", "@exec": "exec", "@nope": "exec"}}),
    )
    .await;
    assert_eq!(r["reason"], "bad_binding", "{r}");
    assert!(r["error"].as_str().unwrap().contains("@nope"), "{r}");
    assert_eq!(k.agent_ids().len(), n, "nothing created");
    let _ = std::fs::remove_dir_all(d);
}
