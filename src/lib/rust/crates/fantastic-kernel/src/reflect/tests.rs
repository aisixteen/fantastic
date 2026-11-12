//! Unit tests for [`crate::reflect`].
use crate::outcome::Item;

use super::*;
use crate::agent::Agent;
use serde_json::json;
use std::path::Path;

fn mk_agent(id: &str, parent: Option<AgentId>) -> Arc<Agent> {
    Agent::new(
        AgentId::from(id),
        None,
        parent,
        Map::new(),
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

fn mk_agent_meta(id: &str, parent: Option<AgentId>, meta: Map<String, Value>) -> Arc<Agent> {
    Agent::new(
        AgentId::from(id),
        None,
        parent,
        meta,
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

/// The arguments of a `reflect` that names no flag.
fn plain_reflect() -> crate::declared::common::Reflect {
    match crate::declared::common::Call::parse("x", &json!({"type": "reflect"})) {
        Ok(crate::declared::common::Call::Reflect(a)) => a,
        other => panic!("not a reflect: {other:?}"),
    }
}

/// Mirror what `send()` does for a bare agent: identity + flags.
async fn bare_reflect(kernel: &Arc<Kernel>, agent: &Arc<Agent>, payload: &Value) -> Value {
    let mut r = reflect_identity(agent);
    let args = match crate::declared::common::Call::parse(&agent.id.0, payload) {
        Ok(crate::declared::common::Call::Reflect(a)) => a,
        Ok(other) => panic!("not a reflect: {other:?}"),
        Err(refused) => return refused,
    };
    apply_reflect_flags(kernel, agent, &args, &mut r)
        .await
        .expect("no roster node raised");
    r
}

#[test]
fn tree_node_includes_id_and_empty_children() {
    let a = mk_agent("leaf", Some(AgentId::from("p")));
    let v = tree_node(&a);
    assert_eq!(v["id"], "leaf");
    assert_eq!(v["parent_id"], "p");
    assert_eq!(v["children"], json!([]));
}

#[tokio::test]
async fn root_reflect_is_uniform() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = bare_reflect(&kernel, &root, &json!({"type": "reflect"})).await;
    assert_eq!(v["id"], "kernel_state");
    assert!(v["sentence"]
        .as_str()
        .unwrap()
        .starts_with("Fantastic kernel"));
    assert_eq!(v["parent_id"], Value::Null);
    // tree default = all.
    assert_eq!(v["tree"]["id"], "kernel_state");
    // bundles omitted by default.
    assert!(v.get("bundles").is_none());
    // kernel runtime identity + deployment context — root only.
    assert_eq!(v["runtime"], "rust");
    // No FANTASTIC_ENV / FANTASTIC_VERSION in the test process → host defaults.
    assert_eq!(v["env"], "host");
    assert!(v["version"].is_null());
}

#[tokio::test]
async fn root_reflect_carries_a_32hex_guid() {
    let kernel = Arc::new(Kernel::new());
    let mut meta = Map::new();
    meta.insert(
        "guid".to_string(),
        json!("0123456789abcdef0123456789abcdef"),
    );
    let root = mk_agent_meta("kernel_state", None, meta);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = bare_reflect(&kernel, &root, &json!({"type": "reflect"})).await;
    let guid = v["guid"].as_str().expect("root reflect has a guid");
    assert_eq!(guid.len(), 32);
    assert!(guid
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
}

#[tokio::test]
async fn child_reflect_has_no_guid() {
    // guid is persistence-scoped + root-only. A child record never carries a
    // `guid` in its meta (only the root record does), so a child reflect must
    // not surface one — `apply_reflect_flags` only injects it on the root.
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = mk_agent("kid_1", Some(AgentId::from("kernel_state")));
    kernel.register(Arc::clone(&child));
    let v = bare_reflect(&kernel, &child, &json!({"type": "reflect"})).await;
    assert!(v.get("guid").is_none(), "guid must be root-only");
}

#[tokio::test]
async fn child_reflect_is_uniform() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = mk_agent("kid_1", Some(AgentId::from("kernel_state")));
    kernel.register(Arc::clone(&child));
    let v = bare_reflect(&kernel, &child, &json!({"type": "reflect"})).await;
    assert_eq!(v["id"], "kid_1");
    assert_eq!(v["parent_id"], "kernel_state");
    assert_eq!(v["tree"]["id"], "kid_1");
    // runtime + env/version are root-only — a child must not carry them.
    assert!(v.get("runtime").is_none());
    assert!(v.get("env").is_none());
    assert!(v.get("version").is_none());
}

#[tokio::test]
async fn tree_tiers() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = mk_agent("kid_1", Some(AgentId::from("kernel_state")));
    kernel.register(Arc::clone(&child));
    root.add_child(Arc::clone(&child));
    // tree=ids → flat list, self first.
    let ids = bare_reflect(&kernel, &root, &json!({"type": "reflect", "tree": "ids"})).await;
    assert_eq!(ids["tree"], json!(["kernel_state", "kid_1"]));
    // tree=none → omitted.
    let none = bare_reflect(&kernel, &root, &json!({"type": "reflect", "tree": "none"})).await;
    assert!(none.get("tree").is_none());
}

#[tokio::test]
async fn bundles_tiers() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // No bundles registered in this bare kernel → empty list (not absent).
    let all = bare_reflect(
        &kernel,
        &root,
        &json!({"type": "reflect", "bundles": "all"}),
    )
    .await;
    assert!(all["bundles"].is_array());
    let ids = bare_reflect(
        &kernel,
        &root,
        &json!({"type": "reflect", "bundles": "ids"}),
    )
    .await;
    assert!(ids["bundles"].is_array());
}

#[tokio::test]
async fn description_surfaces_top_and_tree() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let mut meta = Map::new();
    meta.insert("description".to_string(), json!("holds my notes"));
    let child = mk_agent_meta("kid_1", Some(AgentId::from("kernel_state")), meta);
    kernel.register(Arc::clone(&child));
    root.add_child(Arc::clone(&child));
    // top-level on the child's own reflect.
    let own = bare_reflect(&kernel, &child, &json!({"type": "reflect"})).await;
    assert_eq!(own["description"], "holds my notes");
    // and in the parent's tree=all node.
    let root_v = bare_reflect(&kernel, &root, &json!({"type": "reflect"})).await;
    let node = root_v["tree"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "kid_1")
        .unwrap();
    assert_eq!(node["description"], "holds my notes");
}

#[tokio::test]
async fn tree_ids_is_paged_and_the_cursor_walks_the_whole_tree() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    for i in 0..5 {
        let c = mk_agent(&format!("kid_{i}"), Some(root.id.clone()));
        kernel.register(Arc::clone(&c));
        root.add_child(Arc::clone(&c));
    }

    // Default is UNCHANGED — everything, no cursor.
    let all = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"reflect","tree":"ids"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(all["tree"].as_array().unwrap().len(), 6); // self + 5
                                                          // No further page ⇒ no cursor key at all (python inserts `next` only
                                                          // when one exists; there is no `tree_next`).
    assert!(all.get("next").is_none(), "{all}");
    assert!(all.get("tree_next").is_none(), "{all}");

    // Bounded, and the cursor names the last id handed over.
    let p1 = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"reflect","tree":"ids","limit":2}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(p1["tree"].as_array().unwrap().len(), 2);
    let cursor = p1["next"].as_str().expect("a further page exists");
    assert_eq!(cursor, p1["tree"][1].as_str().unwrap());

    // Resuming visits each id exactly once.
    let mut seen: Vec<String> = p1["tree"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let mut cur = cursor.to_string();
    loop {
        let p = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type":"reflect","tree":"ids","limit":2,"after":cur}),
            )])
            .await
            .remove(0)
            .result;
        for v in p["tree"].as_array().unwrap() {
            seen.push(v.as_str().unwrap().to_string());
        }
        match p["next"].as_str() {
            Some(c) => cur = c.to_string(),
            None => break,
        }
    }
    assert_eq!(seen.len(), 6);
    let mut uniq = seen.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), 6, "a paged walk must not repeat or skip");
}

#[tokio::test]
async fn an_agent_can_read_its_own_authority() {
    // The gate is VISIBLE: otherwise an agent could only discover its authority
    // by trying, which costs whole passes of empirical probing every run. It also
    // makes `update_agent`'s replace-not-merge warning actionable — you must
    // send the map whole, so you must be able to read it.
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let mut meta = Map::new();
    meta.insert("can_call".into(), json!({"store": ["read"]}));
    meta.insert("callable_by".into(), json!({"cli": "*"}));
    let a = mk_agent_meta("worker", Some(root.id.clone()), meta);
    kernel.register(Arc::clone(&a));
    root.add_child(Arc::clone(&a));

    let own = kernel
        .send(vec![Item::new(a.id.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(own["can_call"]["store"][0], "read");
    assert_eq!(own["callable_by"]["cli"], "*");

    // ...and the same two keys ride every WIRING tree node (`detail:full`), so
    // the whole authorization landscape is readable from ONE root reflect. The
    // distilled roster carries purpose, not posture (python `_PURPOSE_KEYS`).
    let tree = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect", "detail": "full"}),
        )])
        .await
        .remove(0)
        .result;
    let node = tree["tree"]["children"]
        .as_array()
        .and_then(|c| c.iter().find(|n| n["id"] == "worker"))
        .expect("the worker node");
    assert_eq!(node["can_call"]["store"][0], "read");
    assert_eq!(node["callable_by"]["cli"], "*");
}

// ─── Children walk CREATION order ─────────────────────────

#[tokio::test]
async fn tree_children_keep_creation_order() {
    // Python's `_children` is a plain dict, so the tree frame, `tree=ids` and
    // the descendant walk all read insertion order — never sorted by id.
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // Deliberately reverse-alphabetical: a sort would invert this.
    for id in ["zulu", "mike", "alpha"] {
        let c = mk_agent(id, Some(root.id.clone()));
        kernel.register(Arc::clone(&c));
        root.add_child(Arc::clone(&c));
    }
    // A grandchild under the LAST child, to pin the DFS order too.
    let g = mk_agent("gamma", Some(AgentId::from("alpha")));
    kernel.register(Arc::clone(&g));
    kernel
        .agents
        .get(&AgentId::from("alpha"))
        .unwrap()
        .add_child(Arc::clone(&g));

    // `tree` frame.
    let node = tree_node(&root);
    let kids: Vec<&str> = node["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(kids, vec!["zulu", "mike", "alpha"], "{node}");

    // `tree=ids` / descendant walk — DFS, self first, children in creation order.
    assert_eq!(
        descendant_ids(&root),
        vec!["kernel_state", "zulu", "mike", "alpha", "gamma"]
    );

    // `child_ids()` is the one ordered accessor both read through.
    assert_eq!(
        root.child_ids(),
        vec![
            AgentId::from("zulu"),
            AgentId::from("mike"),
            AgentId::from("alpha")
        ]
    );

    // A detach leaves the survivors in place — it does not re-sort.
    root.remove_child(&AgentId::from("mike"));
    assert_eq!(
        root.child_ids(),
        vec![AgentId::from("zulu"), AgentId::from("alpha")]
    );
}

// ─── The reflect flags are python's ─────────────

/// A bundle whose reflect names a sentence and a `mode` (or raises, for the
/// agent named `boom`).
struct RosterBundle;

crate::stand_in_verbs!(rosterbundle_verbs { BoomV "boom" {  }, ModelessV "modeless" {  }, PingV "ping" {  } });
#[async_trait::async_trait]
impl crate::bundle::Handler for RosterBundle {
    type Call = rosterbundle_verbs::Call;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "roster_probe"
    }
    fn verbs(&self) -> &'static [crate::Verb] {
        rosterbundle_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &crate::Agent) -> Vec<crate::DynVerb> {
        rosterbundle_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static crate::RecordDecl {
        crate::fixture_record!()
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        let payload = &call.payload();
        if payload.get("type").and_then(Value::as_str) != Some("reflect") {
            return Ok(Some(json!({"ok": true})));
        }
        if agent_id.as_str() == "boom" {
            return Err("RuntimeError: reflect exploded".into());
        }
        Ok(Some(json!({
            "id": agent_id.as_str(),
            "sentence": format!("{} is a roster probe", agent_id.as_str()),
            "handler_module": "roster_probe.tools",
            "mode": if agent_id.as_str() == "modeless" { Value::Null } else { json!("watch") },
        })))
    }
}

/// A kernel with `roster_probe.tools` INSTALLED and `fixture_probe.tools`
/// registered UNLISTED, and a root `kernel_state`.
fn roster_kernel() -> (Arc<Kernel>, Arc<Agent>) {
    let mut kernel = Kernel::new();
    let mut reg = crate::bundle::BundleRegistry::new();
    reg.register("roster_probe.tools", RosterBundle);
    reg.register_unlisted("fixture_probe.tools", RosterBundle);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let mut meta = Map::new();
    meta.insert(
        "callable_by".into(),
        json!({"$self": "*", "$parent": "*", "$subtree": "*"}),
    );
    let root = mk_agent_meta("kernel_state", None, meta);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    (kernel, root)
}

/// A handler-bearing child of `parent` with `meta`.
fn probe_child(kernel: &Arc<Kernel>, id: &str, parent: &Arc<Agent>, meta: Value) -> Arc<Agent> {
    handler_child(kernel, id, "roster_probe.tools", parent, meta)
}

/// Like [`probe_child`] with the handler named.
fn handler_child(
    kernel: &Arc<Kernel>,
    id: &str,
    handler: &str,
    parent: &Arc<Agent>,
    meta: Value,
) -> Arc<Agent> {
    let Value::Object(meta) = meta else {
        panic!("meta must be an object")
    };
    let a = Agent::new(
        AgentId::from(id),
        Some(handler.to_string()),
        Some(parent.id.clone()),
        meta,
        Path::new("/tmp/nowhere").join(id),
        true,
    );
    kernel.register(Arc::clone(&a));
    parent.add_child(Arc::clone(&a));
    a
}

async fn kernel_reflect(kernel: &Arc<Kernel>, payload: Value) -> crate::outcome::Outcome {
    kernel
        .send(vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
}

#[tokio::test]
async fn reflect_carries_the_agents_view() {
    // `reflect` is where any client learns how an agent is drawn: a BUNDLE
    // agent's record `view` rides its reflect; no `view` -> no key.
    let (kernel, root) = roster_kernel();
    let view = json!({"kind": "html", "bytes": "<b>hi</b>"});
    probe_child(&kernel, "drawn", &root, json!({"view": view}));
    probe_child(&kernel, "plain", &root, json!({}));
    let ask = |id: &str| {
        let kernel = Arc::clone(&kernel);
        let id = id.to_string();
        async move {
            kernel
                .send(vec![Item::new(
                    AgentId::from(id),
                    json!({"type": "reflect"}),
                )])
                .await
                .remove(0)
                .result
        }
    };
    assert_eq!(ask("drawn").await["view"], view);
    assert!(ask("plain").await.get("view").is_none());
}

#[tokio::test]
async fn a_reply_that_built_its_own_view_keeps_it() {
    let (kernel, root) = roster_kernel();
    let a = probe_child(&kernel, "drawn", &root, json!({"view": {"kind": "html"}}));
    let own = json!({"kind": "html", "bytes": "own"});
    let mut reply = json!({"id": "drawn", "view": own});
    apply_reflect_flags(&kernel, &a, &plain_reflect(), &mut reply)
        .await
        .expect("no roster node raised");
    assert_eq!(reply["view"], own);
}

#[tokio::test]
async fn a_refusal_gets_no_view() {
    let (kernel, root) = roster_kernel();
    let a = probe_child(&kernel, "drawn", &root, json!({"view": {"kind": "html"}}));
    let mut reply = json!({"error": "nope"});
    apply_reflect_flags(&kernel, &a, &plain_reflect(), &mut reply)
        .await
        .expect("no roster node raised");
    assert_eq!(reply, json!({"error": "nope"}));
}

#[tokio::test]
async fn reflect_carries_the_declared_verbs_and_no_other_docs() {
    // A reflect says what an agent IS and lists its declared verbs; emits, cancel,
    // `more` and `kernel_verbs` are the `docs` kernel verb.
    let (kernel, root) = roster_kernel();
    let child = probe_child(&kernel, "kid", &root, json!({}));
    let r = kernel_reflect(&kernel, json!({"type": "reflect"}))
        .await
        .result;
    let c = kernel
        .send(vec![Item::new(
            child.id.clone(),
            json!({"type": "reflect", "tree": "none"}),
        )])
        .await
        .remove(0)
        .result;
    for reply in [&r, &c] {
        for key in ["more", "kernel_verbs", "emits", "cancel"] {
            assert!(reply.get(key).is_none(), "{key}: {reply}");
        }
    }
    let shown: Vec<&str> = c["verbs"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        shown,
        ["boom", "modeless", "ping", "reflect"],
        "{c}"
    );
    // Python order: `tree`, `next`, `bundles`.
    let r = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "ids", "limit": 1, "bundles": "ids"}),
    )
    .await
    .result;
    let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
    let tail: Vec<&str> = keys[keys.iter().position(|k| *k == "tree").unwrap()..].to_vec();
    assert_eq!(tail[..3], ["tree", "next", "bundles"], "{r}");
}

#[tokio::test]
async fn distilled_tree_reflect_echoes_to_node_watchers() {
    // The per-node reflect is a REAL dispatch: the node's watchers hear
    // `{type:'reflect'}`, and an `internal` call row is written that nothing
    // settles. Python `_run_dispatch`.
    let (kernel, root) = roster_kernel();
    let node = probe_child(&kernel, "watched", &root, json!({}));
    let watcher = mk_agent("tap", Some(root.id.clone()));
    kernel.register(Arc::clone(&watcher));
    root.add_child(Arc::clone(&watcher));
    let mut rx = kernel.ensure_inbox(&watcher.id);
    assert!(kernel.watch(&node.id, watcher.id.clone()).await.is_none());

    let out = kernel_reflect(&kernel, json!({"type": "reflect"})).await;
    assert_eq!(
        rx.try_recv().expect("the roster reflect echoed").payload,
        json!({"type": "reflect"})
    );
    assert!(rx.try_recv().is_none(), "one echo per reflect");

    let rows = crate::test_support::rows_about(&kernel, &node.id);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(
        row.keys().map(String::as_str).collect::<Vec<_>>(),
        ["id", "kind", "verb", "parent", "caller", "payload", "internal", "agent", "ts"],
        "no outcome stamp: the internal dispatch opens no execution"
    );
    assert_eq!(row["kind"], "call");
    assert_eq!(row["verb"], "reflect");
    assert_eq!(row["internal"], true);
    assert_eq!(row["caller"], Value::Null, "an operator reflect");
    assert_ne!(row["id"].as_i64(), out.pid.map(crate::TaskId::get));
    assert_eq!(
        crate::test_support::rows_about(&kernel, &watcher.id).len(),
        0,
        "a bare node is not dispatched"
    );

    // `detail:full` walks the wiring tree — nothing is dispatched.
    let _ = kernel_reflect(&kernel, json!({"type": "reflect", "detail": "full"})).await;
    assert!(rx.try_recv().is_none());
    assert_eq!(crate::test_support::rows_about(&kernel, &node.id).len(), 1);
}

#[tokio::test]
async fn a_raising_roster_node_fails_the_reflect() {
    // python: the exception propagates out of `_roster` and `_settle_once`
    // records the whole reflect `failed`.
    let (kernel, root) = roster_kernel();
    let _boom = probe_child(&kernel, "boom", &root, json!({}));
    let out = kernel_reflect(&kernel, json!({"type": "reflect"})).await;
    assert_eq!(out.status, Status::Failed);
    assert_eq!(
        out.result,
        json!({"error": "RuntimeError: reflect exploded", "raised": true})
    );
    // The flat index dispatches nothing, so it still answers.
    let ids = kernel_reflect(&kernel, json!({"type": "reflect", "tree": "ids"})).await;
    assert_eq!(ids.status, Status::Completed);
}

#[tokio::test]
async fn tree_none_omits_the_tree_and_a_value_the_declaration_lacks_is_refused() {
    let (kernel, _root) = roster_kernel();
    let absent = kernel_reflect(&kernel, json!({"type": "reflect"}))
        .await
        .result;
    assert!(absent["tree"].is_object(), "absent ⇒ all");
    let r = kernel_reflect(&kernel, json!({"type": "reflect", "tree": "none"}))
        .await
        .result;
    assert!(r.get("tree").is_none(), "{r}");
    assert!(r.get("next").is_none());
    assert!(r.get("more").is_none());
    // every other value is refused by name, never read as "omit"
    for flag in [
        Value::Null,
        json!(true),
        json!(false),
        json!(1),
        json!([]),
        json!({}),
        json!("ALL"),
        json!(""),
    ] {
        let r = kernel_reflect(&kernel, json!({"type": "reflect", "tree": flag.clone()}))
            .await
            .result;
        if flag.is_null() {
            // an explicit null is an absent argument
            assert!(r["tree"].is_object(), "tree=null: {r}");
            continue;
        }
        assert_eq!(r["reason"], "bad_arg", "tree={flag}: {r}");
        assert_eq!(r["arg"], "tree", "tree={flag}: {r}");
        assert!(r.get("tree").is_none());
    }
}

/// `a` (roster_probe) and `b` (fixture_probe) under the root, `c` (roster_probe)
/// under `b`, bare `d` under `a`.
fn handler_tree() -> (Arc<Kernel>, Arc<Agent>) {
    let (kernel, root) = roster_kernel();
    let a = probe_child(&kernel, "a", &root, json!({}));
    let b = handler_child(&kernel, "b", "fixture_probe.tools", &root, json!({}));
    probe_child(&kernel, "c", &b, json!({}));
    let d = Agent::new(
        AgentId::from("d"),
        None,
        Some(a.id.clone()),
        Map::new(),
        Path::new("/tmp/nowhere").join("d"),
        true,
    );
    kernel.register(Arc::clone(&d));
    a.add_child(d);
    (kernel, root)
}

fn tree_ids(r: &Value) -> Vec<String> {
    r["tree"]
        .as_array()
        .expect("tree is an array")
        .iter()
        .map(|v| v.as_str().expect("an id").to_string())
        .collect()
}

#[tokio::test]
async fn handler_modules_ids_lists_only_the_named_handlers() {
    let (kernel, _root) = handler_tree();
    let ask = |hm: Value| {
        let kernel = Arc::clone(&kernel);
        async move {
            let r = kernel_reflect(
                &kernel,
                json!({"type": "reflect", "tree": "ids", "handler_modules": hm}),
            )
            .await
            .result;
            tree_ids(&r)
        }
    };
    assert_eq!(ask(json!(["roster_probe.tools"])).await, ["a", "c"]);
    assert_eq!(ask(json!(["fixture_probe.tools"])).await, ["b"]);
    assert_eq!(
        ask(json!(["roster_probe.tools", "fixture_probe.tools"])).await,
        ["a", "b", "c"]
    );
    assert!(ask(json!([])).await.is_empty());
    let all = kernel_reflect(&kernel, json!({"type": "reflect", "tree": "ids"}))
        .await
        .result;
    assert_eq!(tree_ids(&all).len(), 5);
}

/// A role over `roster_probe.tools` (and `b2` over that role) with one agent of each.
fn role_tree() -> Arc<Kernel> {
    let (kernel, root) = handler_tree();
    let yaml = |base: &str| format!("base: {base}\nsentence: s\n");
    kernel
        .bundles
        .register_meta_yaml("probe_role", &yaml("roster_probe.tools"))
        .unwrap();
    kernel
        .bundles
        .register_meta_yaml("b2", &yaml("probe_role"))
        .unwrap();
    handler_child(&kernel, "r1", "probe_role", &root, json!({}));
    handler_child(&kernel, "r2", "b2", &root, json!({}));
    kernel
}

async fn ids_under(kernel: &Arc<Kernel>, hm: Value) -> Vec<String> {
    let r = kernel_reflect(
        kernel,
        json!({"type": "reflect", "tree": "ids", "handler_modules": hm}),
    )
    .await
    .result;
    tree_ids(&r)
}

#[tokio::test]
async fn handler_modules_ids_lists_a_role_agent_under_its_base_and_its_own_name() {
    let kernel = role_tree();
    assert_eq!(
        ids_under(&kernel, json!(["roster_probe.tools"])).await,
        ["a", "c", "r1", "r2"]
    );
    assert_eq!(ids_under(&kernel, json!(["probe_role"])).await, ["r1"]);
    assert!(ids_under(&kernel, json!(["fixture_probe.tools"]))
        .await
        .iter()
        .all(|i| i != "r1"));
    // A two-step chain: its own name and the final base, not the link between.
    assert_eq!(ids_under(&kernel, json!(["b2"])).await, ["r2"]);
    assert!(!ids_under(&kernel, json!(["probe_role"]))
        .await
        .contains(&"r2".to_string()));
}

#[tokio::test]
async fn handler_modules_tree_lists_a_role_agent_under_its_base() {
    let kernel = role_tree();
    let r = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "handler_modules": ["roster_probe.tools"]}),
    )
    .await
    .result;
    let nodes = r["tree"].as_array().expect("flat list");
    let ids: Vec<&str> = nodes.iter().map(|n| n["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["a", "c", "r1", "r2"]);
}

#[tokio::test]
async fn handler_modules_distilled_tree_is_a_flat_list_of_nodes_without_children() {
    let (kernel, _root) = handler_tree();
    let r = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "handler_modules": ["roster_probe.tools"]}),
    )
    .await
    .result;
    let nodes = r["tree"].as_array().expect("flat list");
    let ids: Vec<&str> = nodes.iter().map(|n| n["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["a", "c"]);
    for n in nodes {
        assert!(n["sentence"].is_string());
        assert!(n.get("children").is_none());
    }
}

#[tokio::test]
async fn handler_modules_full_nodes_carry_their_handler() {
    let (kernel, _root) = handler_tree();
    let r = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "detail": "full", "handler_modules": ["fixture_probe.tools"]}),
    )
    .await
    .result;
    let nodes = r["tree"].as_array().expect("flat list");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["id"], "b");
    assert_eq!(nodes[0]["handler_module"], "fixture_probe.tools");
    assert!(nodes[0].get("parent_id").is_some());
    assert!(nodes[0].get("children").is_none());
}

#[tokio::test]
async fn an_agent_outside_the_list_is_not_asked() {
    let (kernel, root) = handler_tree();
    let _boom = probe_child(&kernel, "boom", &root, json!({}));
    let ok = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "handler_modules": ["fixture_probe.tools"]}),
    )
    .await;
    assert_eq!(ok.status, Status::Completed);
    assert_eq!(ok.result["tree"].as_array().unwrap().len(), 1);
    assert_eq!(ok.result["tree"][0]["id"], "b");
    let bad = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "handler_modules": ["roster_probe.tools"]}),
    )
    .await;
    assert_eq!(bad.status, Status::Failed);
    assert_eq!(
        bad.result,
        json!({"error": "RuntimeError: reflect exploded", "raised": true})
    );
}

#[tokio::test]
async fn handler_modules_pages_after_the_filter() {
    let (kernel, _root) = handler_tree();
    let base = json!({"type": "reflect", "tree": "ids", "limit": 1,
        "handler_modules": ["roster_probe.tools"]});
    let first = kernel_reflect(&kernel, base.clone()).await.result;
    assert_eq!(tree_ids(&first), ["a"]);
    let next = first["next"].clone();
    assert!(next.is_string());
    let mut second = base;
    second["after"] = next;
    let second = kernel_reflect(&kernel, second).await.result;
    assert_eq!(tree_ids(&second), ["c"]);
    assert!(second.get("next").is_none());
}

#[tokio::test]
async fn handler_modules_with_show_and_ignore_subtrees() {
    let (kernel, _root) = handler_tree();
    let shown = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "ids", "show_subtrees": ["b"],
            "handler_modules": ["roster_probe.tools"]}),
    )
    .await
    .result;
    assert_eq!(tree_ids(&shown), ["c"]);
    let ignored = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "ids", "ignore_subtrees": ["b"],
            "handler_modules": ["roster_probe.tools"]}),
    )
    .await
    .result;
    assert_eq!(ignored["tree"], json!(["a", {"id": "b", "pruned": 2}]));
}

#[tokio::test]
async fn a_bad_handler_modules_is_refused_bad_arg() {
    let (kernel, _root) = handler_tree();
    for bad in [
        json!("roster_probe.tools"),
        json!(1),
        json!({}),
        json!(["a", 1]),
    ] {
        let r = kernel_reflect(
            &kernel,
            json!({"type": "reflect", "handler_modules": bad.clone()}),
        )
        .await
        .result;
        assert!(r["error"].is_string(), "{bad}: {r}");
        assert_eq!(r["reason"], "bad_arg", "{bad}: {r}");
        assert_eq!(r["arg"], "handler_modules");
        assert!(r.get("tree").is_none());
        let off = kernel_reflect(
            &kernel,
            json!({"type": "reflect", "tree": "none", "handler_modules": bad}),
        )
        .await
        .result;
        // a wrongly typed argument is refused by the shared entry's check of the
        // declaration, whether or not a tree is asked for
        assert_eq!(off["reason"], "bad_arg", "{off}");
        assert_eq!(off["arg"], "handler_modules", "{off}");
    }
    let r = kernel_reflect(&kernel, json!({"type": "reflect", "handler_modules": null}))
        .await
        .result;
    assert!(r["tree"].is_object());
}

#[tokio::test]
async fn handler_modules_on_a_non_root_target() {
    let (kernel, _root) = handler_tree();
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("b"),
            json!({"type": "reflect", "tree": "ids", "handler_modules": ["roster_probe.tools"]}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(tree_ids(&r), ["c"]);
}

/// A bundle that only carries a catalog name.
struct Named(&'static str);

crate::stand_in_verbs!(named_verbs { PingV "ping" {  } });
#[async_trait::async_trait]
impl crate::bundle::Handler for Named {
    type Call = named_verbs::Call;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        self.0
    }
    fn verbs(&self) -> &'static [crate::Verb] {
        named_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &crate::Agent) -> Vec<crate::DynVerb> {
        named_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static crate::RecordDecl {
        crate::fixture_record!()
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        _call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        Ok(None)
    }
}

#[tokio::test]
async fn bundles_page_like_python() {
    let mut kernel = Kernel::new();
    let mut reg = crate::bundle::BundleRegistry::new();
    reg.register("charlie.tools", Named("charlie"));
    reg.register("alpha.tools", Named("alpha"));
    reg.register("bravo.tools", Named("bravo"));
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    for id in ["k1", "k2"] {
        let c = mk_agent(id, Some(root.id.clone()));
        kernel.register(Arc::clone(&c));
        root.add_child(c);
    }
    let all = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "none", "bundles": "all"}),
    )
    .await
    .result;
    assert_eq!(
        all["bundles"],
        json!([
            {"name": "alpha", "handler_module": "alpha.tools", "schema": {"fields": [], "open": true}},
            {"name": "bravo", "handler_module": "bravo.tools", "schema": {"fields": [], "open": true}},
            {"name": "charlie", "handler_module": "charlie.tools", "schema": {"fields": [], "open": true}},
        ])
    );
    assert!(
        all.get("bundles_next").is_none(),
        "no further page ⇒ no key: {all}"
    );

    // `bundles=ids` pages by name with the SAME `limit`/`after` the flat tree
    // index uses in the same reflect.
    let p1 = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "ids", "bundles": "ids", "limit": 2}),
    )
    .await
    .result;
    assert_eq!(p1["tree"], json!(["kernel_state", "k1"]));
    assert_eq!(p1["next"], "k1");
    assert_eq!(p1["bundles"], json!(["alpha", "bravo"]));
    assert_eq!(p1["bundles_next"], "bravo");

    let p2 = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "ids", "bundles": "ids", "limit": 2, "after": "k1"}),
    )
    .await
    .result;
    assert_eq!(p2["tree"], json!(["k2"]));
    assert!(p2.get("next").is_none(), "{p2}");
    // `k1` names no bundle ⇒ the catalog cursor is gone ⇒ an empty tail.
    assert_eq!(p2["bundles"], json!([]));
    assert!(p2.get("bundles_next").is_none(), "{p2}");

    // `bundles=all` pages too, its cursor the entry's name.
    let pa = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "none", "bundles": "all", "limit": 1}),
    )
    .await
    .result;
    assert_eq!(
        pa["bundles"],
        json!([{"name": "alpha", "handler_module": "alpha.tools",
                "schema": {"fields": [], "open": true}}])
    );
    assert_eq!(pa["bundles_next"], "alpha");
    let pb = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "none", "bundles": "ids", "limit": 5, "after": "alpha"}),
    )
    .await
    .result;
    assert_eq!(pb["bundles"], json!(["bravo", "charlie"]));
    assert!(pb.get("bundles_next").is_none(), "{pb}");
}

#[tokio::test]
async fn a_probe_registered_bundle_is_absent_from_the_catalog() {
    // Python lists entry-point bundles only; a fixture imported without
    // one dispatches but is not installable.
    let (kernel, root) = roster_kernel();
    let ids = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "none", "bundles": "all"}),
    )
    .await
    .result;
    assert_eq!(
        ids["bundles"],
        json!([{"name": "roster_probe", "handler_module": "roster_probe.tools",
                "schema": {"fields": [], "open": true}}])
    );
    // ...and it still dispatches.
    let fixture = Agent::new(
        AgentId::from("fx"),
        Some("fixture_probe.tools".to_string()),
        Some(root.id.clone()),
        Map::new(),
        Path::new("/tmp/nowhere").join("fx"),
        true,
    );
    kernel.register(Arc::clone(&fixture));
    let r = kernel
        .send(vec![Item::new(fixture.id.clone(), json!({"type": "ping"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r, json!({"ok": true}));
}

// ─── An empty display_name is no name ─────────────────────────

#[tokio::test]
async fn empty_display_name_falls_back_to_id() {
    let mut meta = Map::new();
    meta.insert("display_name".into(), json!(""));
    let a = mk_agent_meta("nameless", Some(AgentId::from("kernel_state")), meta);
    assert_eq!(reflect_identity(&a)["display_name"], "nameless");
    assert_eq!(tree_node(&a)["display_name"], "nameless");

    let mut meta = Map::new();
    meta.insert("display_name".into(), json!("Named"));
    let b = mk_agent_meta("named", Some(AgentId::from("kernel_state")), meta);
    assert_eq!(reflect_identity(&b)["display_name"], "Named");
    assert_eq!(tree_node(&b)["display_name"], "Named");
}

/// The TREE ROOT answers `reflect` with the substrate identity even when
/// it carries a handler_module (the root loader). Its persistence verbs still
/// route to the bundle below; `reflect <root>` is the kernel's own identity.
#[tokio::test]
async fn root_with_handler_reflects_identity() {
    let mut kernel = Kernel::new();
    crate::test_support::register_root_loader(&mut kernel.bundles);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some("kernel_state.tools".to_string()),
        None,
        Map::new(),
        std::path::PathBuf::new(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = kernel
        .send(vec![crate::Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect", "tree": "none"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(v["id"], json!("kernel_state"));
    assert_eq!(v["handler_module"], json!("kernel_state.tools"));
    assert_eq!(
        v["sentence"],
        json!("Fantastic kernel. Everything is reachable by sending messages to agents.")
    );
    // A NON-root agent of the same bundle does NOT short-circuit — its reflect
    // is its handler's, which the stand-in loader does not answer.
    let _ = kernel
        .send(vec![crate::Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"kernel_state.tools","id":"session"}),
        )])
        .await;
    let v = kernel
        .send(vec![crate::Item::new(
            AgentId::from("session"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_ne!(
        v["sentence"],
        json!("Fantastic kernel. Everything is reachable by sending messages to agents.")
    );
}

/// The record of the `schemaed` bundle.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SchemaedRecord {
    /// the URL
    #[allow(dead_code)]
    endpoint: String,
}

mod schemaed_record {
    use super::SchemaedRecord;
    crate::record!(SchemaedRecord);
}

/// A bundle that declares a closed record.
struct Schemaed;

crate::stand_in_verbs!(schemaed_verbs { PingV "ping" {  } });
#[async_trait::async_trait]
impl crate::bundle::Handler for Schemaed {
    type Call = schemaed_verbs::Call;
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn name(&self) -> &str {
        "schemaed"
    }
    fn verbs(&self) -> &'static [crate::Verb] {
        schemaed_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &crate::Agent) -> Vec<crate::DynVerb> {
        schemaed_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static crate::RecordDecl {
        &schemaed_record::RECORD
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        _call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        Ok(None)
    }
}

/// Python `_available_bundles(schemas=True)`: `bundles=all` carries a bundle's
/// `schema` — its declared record, after `name`/`handler_module`, and only
/// there; the cheap `bundles=ids` listing stays names.
#[tokio::test]
async fn bundles_all_carries_a_declared_record() {
    let mut kernel = Kernel::new();
    let mut reg = crate::bundle::BundleRegistry::new();
    reg.register("alpha.tools", Named("alpha"));
    reg.register("schemaed.tools", Schemaed);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let all = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "none", "bundles": "all"}),
    )
    .await
    .result;
    assert_eq!(
        all["bundles"],
        json!([
            {"name": "alpha", "handler_module": "alpha.tools",
             "schema": {"fields": [], "open": true}},
            {"name": "schemaed", "handler_module": "schemaed.tools",
             "schema": {"fields": [{"name": "endpoint", "type": "string", "required": true,
                                    "hint": "the URL"}], "open": false}},
        ])
    );
    let keys: Vec<&String> = all["bundles"][1].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["name", "handler_module", "schema"]);
    let ids = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "none", "bundles": "ids"}),
    )
    .await
    .result;
    assert_eq!(ids["bundles"], json!(["alpha", "schemaed"]));
}
