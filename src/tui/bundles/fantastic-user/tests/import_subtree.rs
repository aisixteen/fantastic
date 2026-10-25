//! `import_subtree` against a real in-memory kernel: export, then import.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    bundle::BundleError,
    AgentId, Bundle, BundleRegistry, Item, Kernel, Reply,
};
use fantastic_user::export::strip_runtime;
use fantastic_user::{UserBundle, HANDLER_MODULE, VERBS};
use serde_json::{json, Map, Value};

/// Answers with what it was given.
struct Echo;

#[async_trait]
impl Bundle for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        p: &Value,
        _k: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(Some(json!({"echo": p})))
    }
}

/// The shell: a send from outside every agent.
async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
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
    k.agents
        .get(&AgentId::from(id))
        .unwrap_or_else(|| panic!("no agent {id}"))
        .value()
        .record()
        .meta
}

fn parent(k: &Kernel, id: &str) -> Option<String> {
    k.agents
        .get(&AgentId::from(id))
        .and_then(|a| a.value().record().parent_id)
}

/// root ─ user {memory, memory2 (stores), exec, other} · a {b {c}}: a and c
/// reach memory, b reaches exec, memory grants a and c back.
async fn setup() -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, UserBundle::default());
    reg.register("sqlite_store.tools", Echo);
    reg.register("exec_bridge.tools", Echo);
    reg.register("echo.tools", Echo);
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
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
            json!({"callable_by": {"a": "*", "c": "*"}}),
        ),
        ("memory2", "user", "sqlite_store.tools", json!({})),
        (
            "exec",
            "user",
            "exec_bridge.tools",
            json!({"callable_by": {"b": ["run"]}}),
        ),
        ("other", "user", "echo.tools", json!({})),
        (
            "a",
            "user",
            "echo.tools",
            json!({"can_call": {"memory": "*", "b": "*"}, "memory_store_id": "memory",
            "credentials": {"store": "memory", "ref": "r1"}, "notes": "top"}),
        ),
        (
            "b",
            "a",
            "echo.tools",
            json!({"can_call": {"exec": ["run"]}, "callable_by": {"a": "*"}, "cwd": "."}),
        ),
        (
            "c",
            "b",
            "echo.tools",
            json!({"can_call": {"memory": "*"}, "notes": "plain"}),
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

/// `v` with every grant key and id field renamed by `ids`.
fn renamed(m: &Map<String, Value>, ids: &[(&str, &str)]) -> Map<String, Value> {
    let r = |s: &str| {
        ids.iter()
            .find(|(a, _)| *a == s)
            .map_or_else(|| s.to_string(), |(_, b)| (*b).to_string())
    };
    let mut out = m.clone();
    for g in ["can_call", "callable_by"] {
        if let Some(Value::Object(o)) = m.get(g) {
            out.insert(
                g.into(),
                Value::Object(o.iter().map(|(k, v)| (r(k), v.clone())).collect()),
            );
        }
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
    let n = k.agents.len();
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
    assert_eq!(reaches[1]["ports"], json!({"@exec": "exec"}), "{r}");
    assert_eq!(k.agents.len(), n, "nothing created");
    assert!(meta(&k, "memory2")
        .get("callable_by")
        .unwrap()
        .get("a_1")
        .is_none());
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
    // the bound agents granted the new ids back
    assert_eq!(meta(&k, "memory2")["callable_by"]["a_1"], "*");
    assert_eq!(meta(&k, "memory2")["callable_by"]["c_1"], "*");
    assert_eq!(meta(&k, "exec")["callable_by"]["b_1"], json!(["run"]));
    // the originals are as they were
    assert_eq!(meta(&k, "a")["can_call"]["b"], "*");
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
    assert_eq!(meta(&k, "a_2")["can_call"]["b_2"], "*");
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
    assert_eq!(meta(&k, "memory2")["callable_by"]["a_2"], "*");
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
    let n = k.agents.len();
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
    assert_eq!(k.agents.len(), n, "nothing created");
    // allowed, the code comes in
    let r = import(
        &k,
        &c,
        json!({"approve": true, "bind": {}, "allow_code": true}),
    )
    .await;
    assert_eq!(r["imported"], 1, "{r}");
    for x in [d, s, c] {
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
    assert_eq!(meta(&k, "memory2")["callable_by"]["c_1"], "*");
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test(flavor = "multi_thread")]
async fn import_failure_rolls_back() {
    let k = setup().await;
    let n = k.agents.len();
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
            can_call: {$self: 5}
",
        &[],
    );
    let r = import(&k, &d, json!({"approve": true, "bind": {}})).await;
    assert_eq!(r["reason"], "failed", "{r}");
    assert!(
        r["error"].as_str().unwrap().contains("agent c (c_1)"),
        "{r}"
    );
    assert_eq!(k.agents.len(), n, "no half team stays");
    assert!(parent(&k, "a_1").is_none());
    let _ = std::fs::remove_dir_all(d);
}

#[tokio::test(flavor = "multi_thread")]
async fn import_requires_literal_true_for_approve_and_allow_code() {
    let k = setup().await;
    let n = k.agents.len();
    let d = exported(&k, "literal").await;
    let r = import(&k, &d, json!({"approve": "true"})).await;
    assert_eq!(r["approval_required"], true, "{r}");
    assert_eq!(k.agents.len(), n);
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
    assert_eq!(r["reason"], "code_not_allowed", "{r}");
    assert_eq!(k.agents.len(), n);
    for x in [d, c] {
        let _ = std::fs::remove_dir_all(x);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn import_subtree_args_and_docs() {
    assert!(VERBS.contains(&"import_subtree"));
    let k = setup().await;
    for p in [
        json!({"type": "import_subtree", "parent_id": "other"}),
        json!({"type": "import_subtree", "path": "x"}),
        json!({"type": "import_subtree", "path": "x/../y", "parent_id": "other"}),
        json!({"type": "import_subtree", "path": "x", "parent_id": "other", "bind": []}),
    ] {
        let r = op(&k, "user", p.clone()).await;
        assert_eq!(r["reason"], "bad_arg", "{p}: {r}");
    }
    let r = op(
        &k,
        "user",
        json!({"type": "import_subtree", "path": "x", "parent_id": "nobody"}),
    )
    .await;
    assert_eq!(r["reason"], "not_found", "{r}");
    let d = op(&k, "kernel", json!({"type": "docs", "of": "user"})).await;
    assert!(d["verbs"].get("import_subtree").is_some(), "{d}");
}

#[tokio::test(flavor = "multi_thread")]
async fn import_refuses_bind_to_a_missing_agent_naming_the_port() {
    let k = setup().await;
    let d = exported(&k, "ghost").await;
    let n = k.agents.len();
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
        assert_eq!(k.agents.len(), n, "nothing created");
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
    assert_eq!(k.agents.len(), n, "nothing created");
    let _ = std::fs::remove_dir_all(d);
}
