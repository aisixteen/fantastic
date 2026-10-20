//! Unit tests for the cabinet bundle, on a real kernel — the Rust mirror of python
//! `cabinet/tests/test_cabinet.py`.
//!
//! Every caller acts through `as_agent` (the dispatch identity of the sending agent),
//! so each send is gated as that agent: the master opens cabinets, a worker creates
//! equipment on its canvas or messages a peer, and the gate refuses what the grants do
//! not allow. The operator wires the manager once, as the readme says. The AI backend is
//! a scripted `fake_ai.tools` that logs every `send` it receives, with who sent it. The
//! spawning canvas is a stub (`canvas_stub.tools`) that answers `new_canvas`, `create`
//! and `place` the way the tui host's canvas does (its nested canvases are the host's).

use super::*;
use async_trait::async_trait;
use fantastic_core::KernelStateBundle;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::test_support::{as_agent, grant};
use fantastic_kernel::{current_origin, Agent, Item, Kernel};
use fantastic_ram_store::RamStoreBundle;
use serde_json::Map;
use std::sync::Mutex;
use tempfile::TempDir;

const MANAGER: &str = "cabinet_manager";
const MASTER: &str = "master";
const CANVAS: &str = "canvas_main";
const STORE: &str = "memory";
const ROOT: &str = "kernel_state";
const FAKE: &str = "fake_ai.tools";
const CANVAS_STUB: &str = "canvas_stub.tools";
const VERBS: [&str; 3] = ["create_agent", "update_agent", "delete_agent"];

/// One `send` the scripted AI received: the agent it reached, the text, and who sent it.
type Log = Arc<Mutex<Vec<(String, String, Option<String>)>>>;

/// The scripted AI backend: it answers `reflect` and logs every `send`.
#[derive(Clone)]
struct FakeAi {
    log: Log,
}

#[async_trait]
impl Bundle for FakeAi {
    fn name(&self) -> &str {
        FAKE
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "reflect" => Ok(Some(json!({"sentence": "a scripted AI"}))),
            "send" => {
                let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
                let origin = current_origin().map(|a| a.0);
                self.log
                    .lock()
                    .unwrap()
                    .push((agent_id.0.clone(), text.to_string(), origin));
                Ok(Some(json!({"response": "heard", "client_id": "cli"})))
            }
            other => Ok(Some(json!({"error": format!("fake ai: no verb {other}")}))),
        }
    }
}

/// The canvas that spawns cabinets, as a stub of the tui host's canvas node: `new_canvas`
/// makes a CHILD canvas of itself (granted on the root, as the host grants it), `create`
/// makes a child of itself (equipment, an AI admitted to the store it names), `place`
/// acknowledges a view.
#[derive(Default)]
struct CanvasStub;

#[async_trait]
impl Bundle for CanvasStub {
    fn name(&self) -> &str {
        CANVAS_STUB
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let id = agent_id.as_str();
        let reply = match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => return Ok(None),
            "reflect" => json!({"id": id, "sentence": "stub canvas"}),
            "new_canvas" => new_canvas(id, payload, kernel).await,
            "create" => create_here(id, payload, kernel).await,
            "place" => json!({"placed": true}),
            other => json!({"error": format!("canvas stub: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }
}

/// The node a stub canvas named `name` takes (`canvas_<slug>`).
fn stub_node(name: &str) -> String {
    let slug: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("canvas_{}", slug.trim_matches('_'))
}

async fn new_canvas(id: &str, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let node = stub_node(payload.get("name").and_then(Value::as_str).unwrap_or(""));
    let root = ROOT;
    let made = kernel
        .send(vec![Item::new(
            AgentId::from(root),
            json!({
                "type": "create_agent", "parent_id": id, "id": node,
                "handler_module": CANVAS_STUB,
                "can_call": {"$self": "*"}, "callable_by": {"$self": "*"},
            }),
        )])
        .await
        .remove(0)
        .result;
    if let Some(e) = made.get("error") {
        return json!({"error": e});
    }
    // the root lets the new canvas make agents, and it may ask the root for them
    grant(kernel, root, "callable_by", &node, json!(["create_agent"]));
    grant(kernel, &node, "can_call", root, json!(["create_agent"]));
    json!({"id": node})
}

async fn create_here(id: &str, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let Some(record) = payload.get("record").and_then(Value::as_object) else {
        return json!({"error": "create: record (dict) required"});
    };
    let mut spec = record.clone();
    spec.insert("type".into(), json!("create_agent"));
    spec.insert("parent_id".into(), json!(id));
    let store = record
        .get("memory_store_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(store) = &store {
        // an AI made on the canvas reaches its store (the host's `make_ai` path)
        let mut cc = record
            .get("can_call")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        cc.insert(store.clone(), json!("*"));
        spec.insert("can_call".into(), Value::Object(cc));
    }
    let made = kernel
        .send(vec![Item::new(AgentId::from(ROOT), Value::Object(spec))])
        .await
        .remove(0)
        .result;
    if let (Some(store), None) = (&store, made.get("error")) {
        // the store's side of the gate: its `callable_by` names the new AI
        let ai = made.get("id").and_then(Value::as_str).unwrap_or_default();
        grant(kernel, store, "callable_by", ai, json!("*"));
    }
    made
}

/// A kernel with the operator's wiring: a MASTER AI (on a memory store), a MANAGER that
/// may create, update and delete on the root, the root that names the manager, and the
/// CANVAS that spawns the cabinets (the manager's `canvas_id`).
async fn world(tmp: &TempDir) -> (Arc<Kernel>, Log) {
    let log: Log = Arc::default();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, CabinetBundle);
    kernel
        .bundles
        .register(fantastic_core::HANDLER_MODULE, KernelStateBundle::default());
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        RamStoreBundle::default(),
    );
    kernel.bundles.register(FAKE, FakeAi { log: log.clone() });
    kernel.bundles.register(CANVAS_STUB, CanvasStub);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from(ROOT),
        Some(fantastic_core::HANDLER_MODULE.to_string()),
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    // the operator's writes are ungated: no sender
    op(
        &kernel,
        ROOT,
        json!({
            "type": "create_agent", "parent_id": ROOT, "id": CANVAS,
            "handler_module": CANVAS_STUB,
            "can_call": {"$self": "*", ROOT: ["create_agent", "update_agent"]},
            "callable_by": {"$self": "*"},
        }),
    )
    .await;
    op(
        &kernel,
        ROOT,
        json!({
            "type": "create_agent", "parent_id": ROOT, "id": MANAGER,
            "handler_module": HANDLER_MODULE, "cabinet_role": "manager",
            "canvas_id": CANVAS,
            "can_call": {"$self": "*", ROOT: VERBS},
            "callable_by": {"$self": "*", MASTER: "*"},
        }),
    )
    .await;
    op(
        &kernel,
        ROOT,
        json!({
            "type": "create_agent", "parent_id": ROOT, "id": STORE,
            "handler_module": fantastic_ram_store::HANDLER_MODULE,
        }),
    )
    .await;
    op(
        &kernel,
        ROOT,
        json!({
            "type": "create_agent", "parent_id": ROOT, "id": MASTER,
            "handler_module": FAKE, "endpoint": "http://ollama.test", "model": "model-a",
            "context_window": 8000, "memory_store_id": STORE,
            "can_call": {STORE: "*", MANAGER: "*"},
        }),
    )
    .await;
    grant(&kernel, STORE, "callable_by", MASTER, json!("*"));
    grant(&kernel, ROOT, "callable_by", MANAGER, json!(VERBS));
    grant(
        &kernel,
        ROOT,
        "callable_by",
        CANVAS,
        json!(["create_agent", "update_agent"]),
    );
    assert!(kernel.resolve_target(MASTER).is_some() && kernel.resolve_target(MANAGER).is_some());
    (kernel, log)
}

/// One operator send (ungated).
async fn op(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

/// One send made by `sender` as its own dispatch (gated as that agent).
async fn as_(kernel: &Arc<Kernel>, sender: &str, target: &str, payload: Value) -> Value {
    let k = Arc::clone(kernel);
    let target = target.to_string();
    as_agent(sender, async move {
        k.send(vec![Item::new(AgentId::from(target), payload)])
            .await
            .remove(0)
            .result
    })
    .await
}

async fn open(kernel: &Arc<Kernel>, task: &str, extra: Value) -> Value {
    let mut payload = json!({"type": "open_cabinet", "task": task});
    if let (Some(p), Value::Object(e)) = (payload.as_object_mut(), extra) {
        p.extend(e);
    }
    as_(kernel, MASTER, MANAGER, payload).await
}

/// An opened cabinet's id, asserting the open succeeded.
async fn open_ok(kernel: &Arc<Kernel>, name: &str) -> String {
    let res = open(kernel, &format!("task {name}"), json!({"name": name})).await;
    assert!(res.get("error").is_none(), "{res}");
    res["id"].as_str().unwrap().to_string()
}

/// The worker's `create` on its cabinet's canvas (as the sender): equipment under the canvas.
async fn create_on(kernel: &Arc<Kernel>, canvas: &str, sender: &str, record: Value) -> Value {
    as_(
        kernel,
        sender,
        canvas,
        json!({"type": "create", "record": record}),
    )
    .await
}

/// One field of a live agent's record, or Null.
fn meta(kernel: &Kernel, id: &str, key: &str) -> Value {
    kernel
        .agents
        .get(&AgentId::from(id))
        .map(|a| a.record().meta.get(key).cloned().unwrap_or(Value::Null))
        .unwrap_or(Value::Null)
}

fn worker_of(kernel: &Kernel, cab: &str) -> String {
    meta(kernel, cab, "worker").as_str().unwrap().to_string()
}

fn canvas_of(kernel: &Kernel, cab: &str) -> String {
    meta(kernel, cab, "canvas").as_str().unwrap().to_string()
}

fn exists(kernel: &Kernel, id: &str) -> bool {
    kernel.agents.contains_key(&AgentId::from(id))
}

fn agent_ids(kernel: &Kernel) -> std::collections::BTreeSet<String> {
    kernel.agent_ids().into_iter().map(|a| a.0).collect()
}

fn no_error(res: &Value) {
    assert!(res.get("error").is_none(), "{res}");
}

fn reason_of(res: &Value) -> &str {
    res.get("reason").and_then(Value::as_str).unwrap_or("")
}

fn deleted_of(res: &Value) -> Vec<String> {
    res["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

// ─── wiring ──────────────────────────────────────────────────────

#[tokio::test]
async fn open_wires_every_worker_to_every_peer_and_the_master() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let names = ["alpha", "beta", "gamma"];
    let mut cabs = Vec::new();
    for name in names {
        cabs.push(open_ok(&k, name).await);
    }
    let workers: Vec<String> = cabs.iter().map(|c| worker_of(&k, c)).collect();
    let root_callable = meta(&k, ROOT, "callable_by");
    for (i, w) in workers.iter().enumerate() {
        let peers: Vec<&String> = workers.iter().filter(|x| *x != w).collect();
        let can_call = meta(&k, w, "can_call");
        let callable_by = meta(&k, w, "callable_by");
        for p in &peers {
            // egress and ingress name every peer, for messages and nothing else
            assert_eq!(can_call[p.as_str()], json!(MESSAGE_VERBS), "{w}");
            assert_eq!(callable_by[p.as_str()], json!(MESSAGE_VERBS), "{w}");
        }
        // the master calls the worker in full; the worker only messages the master
        assert_eq!(can_call[MASTER], json!(MESSAGE_VERBS));
        assert_eq!(callable_by[MASTER], json!("*"));
        // the directory lists exactly its peers, with their tasks
        let dir = meta(&k, w, "cabinet_peers");
        let dir = dir.as_array().unwrap();
        assert_eq!(dir.len(), 2, "{w}: {dir:?}");
        for (j, p) in workers.iter().enumerate() {
            if p != w {
                let row = dir.iter().find(|d| d["worker"] == json!(p)).unwrap();
                assert_eq!(row["task"], json!(format!("task {}", names[j])));
            }
        }
        let text = meta(&k, w, "post_system_prompt");
        let text = text.as_str().unwrap();
        assert!(text.contains(&format!("cabinet `{}`", cabs[i])), "{text}");
        assert!(text.contains("one of 3 cooperating workers"), "{text}");
        for p in &peers {
            assert!(text.contains(p.as_str()), "{text}");
        }
        // the worker holds nothing on the root, and its canvas is its one place to build
        assert!(can_call.get(ROOT).is_none());
        assert!(root_callable.get(w.as_str()).is_none());
        let canvas = canvas_of(&k, &cabs[i]);
        assert_eq!(can_call[canvas.as_str()], json!(CANVAS_VERBS));
        assert_eq!(
            meta(&k, &canvas, "callable_by")[w.as_str()],
            json!(CANVAS_VERBS)
        );
        assert!(text.contains(&format!("canvas `{canvas}`")), "{text}");
    }
    // the master reaches each worker in full, and the manager monitors them
    assert_eq!(
        meta(&k, MASTER, "can_call")[workers[0].as_str()],
        json!("*")
    );
    assert_eq!(
        meta(&k, MASTER, "callable_by")[workers[2].as_str()],
        json!(MESSAGE_VERBS)
    );
    assert_eq!(
        meta(&k, MANAGER, "can_call")[workers[1].as_str()],
        json!("*")
    );
}

#[tokio::test]
async fn each_cabinets_canvas_is_a_child_of_the_canvas_that_spawns_it() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "nest").await;
    let canvas = canvas_of(&k, &cab);
    assert_eq!(canvas, "canvas_cabinet_nest");
    // nested under the spawning canvas, not a flat peer, and the cabinet is the manager's
    assert_eq!(
        k.agents
            .get(&AgentId::from(canvas.as_str()))
            .unwrap()
            .record()
            .parent_id,
        Some(CANVAS.to_string())
    );
    assert_eq!(
        k.agents
            .get(&AgentId::from(cab.as_str()))
            .unwrap()
            .record()
            .parent_id,
        Some(MANAGER.to_string())
    );
    assert_eq!(meta(&k, &cab, "canvas"), json!(canvas));
}

#[tokio::test]
async fn close_unwires_the_peers_and_leaves_nothing_behind() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let mut cabs = Vec::new();
    for i in 0..3 {
        cabs.push(open_ok(&k, &format!("c{i}")).await);
    }
    let workers: Vec<String> = cabs.iter().map(|c| worker_of(&k, c)).collect();
    let (gone_cab, gone_worker) = (cabs[1].clone(), workers[1].clone());
    let gone_canvas = canvas_of(&k, &gone_cab);
    let res = as_(
        &k,
        MASTER,
        MANAGER,
        json!({"type": "close_cabinet", "id": gone_cab}),
    )
    .await;
    no_error(&res);
    let deleted = deleted_of(&res);
    assert!(deleted.contains(&gone_cab) && deleted.contains(&gone_worker));
    assert!(deleted.contains(&gone_canvas));
    assert!(!exists(&k, &gone_cab) && !exists(&k, &gone_worker) && !exists(&k, &gone_canvas));
    // the root's grant for the closed cabinet's canvas goes with it (the kernel prunes it)
    let root_callable = meta(&k, ROOT, "callable_by");
    assert!(root_callable.get(&gone_canvas).is_none());
    for w in [&workers[0], &workers[2]] {
        let can_call = meta(&k, w, "can_call");
        let callable_by = meta(&k, w, "callable_by");
        assert!(can_call.get(&gone_worker).is_none() && callable_by.get(&gone_worker).is_none());
        let dir = meta(&k, w, "cabinet_peers");
        let dir = dir.as_array().unwrap();
        assert_eq!(dir.len(), 1);
        assert!(dir.iter().all(|d| d["worker"] != json!(gone_worker)));
    }
    assert!(meta(&k, MASTER, "can_call").get(&gone_worker).is_none());
    assert!(meta(&k, MASTER, "callable_by").get(&gone_worker).is_none());
}

#[tokio::test]
async fn close_refuses_an_agent_that_is_not_a_cabinet_of_this_manager() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let res = as_(
        &k,
        MASTER,
        MANAGER,
        json!({"type": "close_cabinet", "id": MASTER}),
    )
    .await;
    assert_eq!(reason_of(&res), "unknown_agent", "{res}");
}

#[tokio::test]
async fn the_default_backend_is_the_existing_master() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let res = open(&k, "plain", json!({"name": "plain"})).await;
    no_error(&res);
    assert_eq!(res["backend"], json!(MASTER));
    assert_eq!(res["new_backend"], json!(false));
    assert!(res.get("warning").is_none());
    let w = res["worker"].as_str().unwrap();
    assert_eq!(meta(&k, w, "endpoint"), json!("http://ollama.test"));
    assert_eq!(meta(&k, w, "memory_store_id"), json!(STORE));
}

#[tokio::test]
async fn setup_over_the_backend_is_flagged_as_a_new_backend() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let res = open(
        &k,
        "other",
        json!({"name": "other", "setup": {"endpoint": "http://elsewhere.test"}}),
    )
    .await;
    no_error(&res);
    assert_eq!(res["new_backend"], json!(true));
    assert!(res["warning"].as_str().unwrap().contains("NEW backend"));
    let w = res["worker"].as_str().unwrap();
    assert_eq!(meta(&k, w, "endpoint"), json!("http://elsewhere.test"));
}

#[tokio::test]
async fn open_refuses_loudly_on_bad_inputs() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let before = agent_ids(&k);
    let cases = [
        // no task
        json!({"type": "open_cabinet"}),
        // a name that is not a string
        json!({"type": "open_cabinet", "task": "t", "name": 3}),
        // a setup that carries wiring
        json!({"type": "open_cabinet", "task": "t", "name": "w", "setup": {"can_call": {}}}),
        // a backend that does not exist
        json!({"type": "open_cabinet", "task": "t", "name": "b", "backend": "nope"}),
        // a backend that is not an AI
        json!({"type": "open_cabinet", "task": "t", "name": "n", "backend": ROOT}),
    ];
    for payload in cases {
        let res = as_(&k, MASTER, MANAGER, payload).await;
        assert!(res.get("error").is_some(), "{res}");
    }
    assert_eq!(agent_ids(&k), before);
}

#[tokio::test]
async fn open_refuses_loudly_when_the_manager_names_no_canvas() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    op(
        &k,
        ROOT,
        json!({"type": "update_agent", "id": MANAGER, "canvas_id": null}),
    )
    .await;
    let before = agent_ids(&k);
    let res = open(&k, "x", json!({"name": "x"})).await;
    assert_eq!(reason_of(&res), "missing_arg", "{res}");
    assert_eq!(res["arg"], json!("canvas_id"));
    assert_eq!(agent_ids(&k), before);
}

#[tokio::test]
async fn open_refuses_loudly_when_the_canvas_does_not_exist() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    op(
        &k,
        ROOT,
        json!({"type": "update_agent", "id": MANAGER, "canvas_id": "canvas_nowhere"}),
    )
    .await;
    let before = agent_ids(&k);
    let res = open(&k, "x", json!({"name": "x"})).await;
    assert_eq!(reason_of(&res), "unknown_agent", "{res}");
    assert_eq!(res["id"], json!("canvas_nowhere"));
    assert_eq!(agent_ids(&k), before);
}

#[tokio::test]
async fn the_ceiling_refuses_loudly_and_makes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    op(
        &k,
        ROOT,
        json!({"type": "update_agent", "id": MANAGER, "max_cabinets": 1}),
    )
    .await;
    open_ok(&k, "one").await;
    let before = agent_ids(&k);
    let res = open(&k, "two", json!({"name": "two"})).await;
    assert_eq!(reason_of(&res), "ceiling", "{res}");
    assert_eq!(res["ceiling"], json!(1));
    assert_eq!(res["open"], json!(1));
    assert_eq!(agent_ids(&k), before);
}

#[tokio::test]
async fn a_bad_ceiling_is_refused_loudly() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    for bad in [json!(0), json!(true), json!("3")] {
        op(
            &k,
            ROOT,
            json!({"type": "update_agent", "id": MANAGER, "max_cabinets": bad}),
        )
        .await;
        let res = open(&k, "x", json!({"name": "x"})).await;
        assert_eq!(reason_of(&res), "bad_arg", "{bad}: {res}");
        assert_eq!(res["arg"], json!("max_cabinets"));
    }
}

#[tokio::test]
async fn a_worker_on_a_missing_memory_store_is_refused() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let res = open(
        &k,
        "x",
        json!({"name": "x", "setup": {"memory_store_id": "no_such_store"}}),
    )
    .await;
    assert_eq!(reason_of(&res), "unknown_agent", "{res}");
    assert!(!exists(&k, "cabinet_x"));
}

#[tokio::test]
async fn a_worker_on_a_missing_credential_store_is_refused() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let before = agent_ids(&k);
    let res = open(
        &k,
        "x",
        json!({"name": "x", "setup": {"credentials": {"store": "no_keys", "ref": "r"}}}),
    )
    .await;
    assert_eq!(reason_of(&res), "unknown_agent", "{res}");
    assert_eq!(res["id"], json!("no_keys"));
    assert_eq!(agent_ids(&k), before);
}

#[tokio::test]
async fn a_worker_with_a_credential_store_is_wired_to_read_it() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    op(
        &k,
        ROOT,
        json!({"type": "create_agent", "parent_id": ROOT, "id": "keys",
               "handler_module": fantastic_ram_store::HANDLER_MODULE}),
    )
    .await;
    let res = open(
        &k,
        "x",
        json!({"name": "x", "setup": {"credentials": {"store": "keys", "ref": "r"}}}),
    )
    .await;
    no_error(&res);
    let w = res["worker"].as_str().unwrap();
    assert_eq!(meta(&k, w, "can_call")["keys"], json!("*"));
    assert_eq!(meta(&k, "keys", "callable_by")[w], json!("*"));
}

#[tokio::test]
async fn a_wiring_failure_removes_the_half_made_cabinet_and_its_canvas() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    // a cabinet whose worker does not exist: wiring the new worker to it must fail
    op(
        &k,
        ROOT,
        json!({"type": "create_agent", "parent_id": MANAGER, "id": "cabinet_ghost",
               "handler_module": HANDLER_MODULE, "cabinet_role": "cabinet",
               "worker": "ghost_worker", "task": "ghost"}),
    )
    .await;
    let before = agent_ids(&k);
    let res = open(&k, "x", json!({"name": "x"})).await;
    let error = res["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("wiring failed and the cabinet was removed"),
        "{res}"
    );
    assert!(error.contains("`ghost_worker` does not exist"), "{res}");
    assert!(!exists(&k, "cabinet_x") && !exists(&k, "cabinet_x_worker"));
    assert!(!exists(&k, "canvas_cabinet_x"), "its canvas goes too");
    assert_eq!(agent_ids(&k), before);
}

#[tokio::test]
async fn a_worker_that_cannot_be_created_removes_its_cabinet_and_its_canvas() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    // a squatter already holds the id the new worker needs
    op(
        &k,
        ROOT,
        json!({"type": "create_agent", "parent_id": ROOT, "id": "cabinet_sq_worker",
               "handler_module": FAKE}),
    )
    .await;
    let before = agent_ids(&k);
    let res = open(&k, "x", json!({"name": "sq"})).await;
    assert_eq!(reason_of(&res), "exists", "{res}");
    assert!(!exists(&k, "cabinet_sq") && !exists(&k, "canvas_cabinet_sq"));
    assert_eq!(agent_ids(&k), before);
}

#[tokio::test]
async fn a_duplicate_name_is_refused_and_the_first_cabinet_stays() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let first = open(&k, "first", json!({"name": "dup"})).await;
    no_error(&first);
    let worker = first["worker"].as_str().unwrap().to_string();
    let res = open(&k, "second", json!({"name": "dup"})).await;
    assert_eq!(reason_of(&res), "exists", "{res}");
    assert_eq!(
        meta(&k, first["id"].as_str().unwrap(), "task"),
        json!("first")
    );
    assert_eq!(
        k.agents
            .get(&AgentId::from(worker.as_str()))
            .unwrap()
            .record()
            .parent_id,
        first["id"].as_str().map(str::to_string)
    );
}

#[tokio::test]
async fn a_canvas_name_clash_is_refused_and_the_first_canvas_stays() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    // a canvas already holds the node a cabinet named `clash` would take
    op(
        &k,
        ROOT,
        json!({"type": "create_agent", "parent_id": CANVAS, "id": "canvas_cabinet_clash",
               "handler_module": CANVAS_STUB}),
    )
    .await;
    let before = agent_ids(&k);
    let res = open(&k, "x", json!({"name": "clash"})).await;
    assert!(
        res["error"]
            .as_str()
            .unwrap_or("")
            .contains("canvas_cabinet_clash"),
        "{res}"
    );
    assert!(!exists(&k, "cabinet_clash"));
    assert_eq!(agent_ids(&k), before);
}

// ─── peer messages and equipment ─────────────────────────────────

#[tokio::test]
async fn a_worker_messages_a_peer_and_the_peer_receives_it() {
    let tmp = TempDir::new().unwrap();
    let (k, log) = world(&tmp).await;
    let a = open_ok(&k, "a").await;
    let b = open_ok(&k, "b").await;
    let (wa, wb) = (worker_of(&k, &a), worker_of(&k, &b));
    let out = as_(&k, &wa, &wb, json!({"type": "send", "text": "hello peer"})).await;
    assert!(out.get("error").is_none(), "{out}");
    let heard = log.lock().unwrap().clone();
    assert!(
        heard.iter().any(|(to, text, from)| to == &wb
            && text == "hello peer"
            && from.as_deref() == Some(wa.as_str())),
        "{heard:?}"
    );
}

#[tokio::test]
async fn a_worker_reaches_a_peer_only_with_messages() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let a = open_ok(&k, "a").await;
    let b = open_ok(&k, "b").await;
    let (wa, wb) = (worker_of(&k, &a), worker_of(&k, &b));
    // a peer's status is its cabinet's, not the worker's to call
    let res = as_(&k, &wa, &b, json!({"type": "status", "line": "x"})).await;
    assert_eq!(reason_of(&res), "unauthorized", "{res}");
    // nor the peer's cabinet, nor the peer's canvas
    let res = as_(&k, &wa, &wb, json!({"type": "reflect"})).await;
    assert!(
        res.get("error").is_none(),
        "messages include reflect: {res}"
    );
    let peer_canvas = canvas_of(&k, &b);
    let res = as_(
        &k,
        &wa,
        &peer_canvas,
        json!({"type": "place", "kind": "html"}),
    )
    .await;
    assert_eq!(reason_of(&res), "unauthorized", "{res}");
}

#[tokio::test]
async fn a_worker_reaches_its_master_only_with_messages() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "m").await;
    let w = worker_of(&k, &cab);
    let res = as_(&k, &w, MASTER, json!({"type": "send", "text": "to master"})).await;
    assert!(res.get("error").is_none(), "{res}");
    let res = as_(&k, &w, MASTER, json!({"type": "open_cabinet", "task": "t"})).await;
    assert_eq!(reason_of(&res), "unauthorized", "{res}");
}

#[tokio::test]
async fn a_worker_creates_equipment_on_its_own_canvas_only() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let a = open_ok(&k, "a").await;
    let b = open_ok(&k, "b").await;
    let (wa, canvas_a, canvas_b) = (worker_of(&k, &a), canvas_of(&k, &a), canvas_of(&k, &b));
    let made = create_on(
        &k,
        &canvas_a,
        &wa,
        json!({"handler_module": "file_bridge.tools", "id": "a_exec", "allow": ["."]}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    assert_eq!(
        k.agents
            .get(&AgentId::from("a_exec"))
            .unwrap()
            .record()
            .parent_id,
        Some(canvas_a.clone())
    );
    // the peer's canvas is not its to build on
    let refused = create_on(
        &k,
        &canvas_b,
        &wa,
        json!({"handler_module": "file_bridge.tools", "id": "sneak"}),
    )
    .await;
    assert_eq!(reason_of(&refused), "unauthorized", "{refused}");
    assert!(!exists(&k, "sneak"));
    // a worker's `new_canvas` on its own canvas is refused: its grant is create and place
    let nested = as_(
        &k,
        &wa,
        &canvas_a,
        json!({"type": "new_canvas", "name": "deeper"}),
    )
    .await;
    assert_eq!(reason_of(&nested), "unauthorized", "{nested}");
    assert!(!exists(&k, "canvas_deeper"));
}

#[tokio::test]
async fn a_worker_reaches_its_equipment_only_through_the_grants_it_is_given() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let a = open_ok(&k, "a").await;
    let b = open_ok(&k, "b").await;
    let (wa, wb) = (worker_of(&k, &a), worker_of(&k, &b));
    let canvas_a = canvas_of(&k, &a);
    let made = create_on(
        &k,
        &canvas_a,
        &wa,
        json!({"handler_module": FAKE, "id": "a_exec",
               "callable_by": {wa.clone(): "*"}}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    // the worker's own can_call names no equipment: the gate refuses until a grant does
    let refused = as_(&k, &wa, "a_exec", json!({"type": "reflect"})).await;
    assert_eq!(reason_of(&refused), "unauthorized", "{refused}");
    grant(&k, &wa, "can_call", "a_exec", json!("*"));
    let granted = as_(&k, &wa, "a_exec", json!({"type": "reflect"})).await;
    assert!(granted.get("error").is_none(), "{granted}");
    // the peer does not, whatever the equipment grants
    let refused = as_(&k, &wb, "a_exec", json!({"type": "reflect"})).await;
    assert_eq!(reason_of(&refused), "unauthorized", "{refused}");
}

#[tokio::test]
async fn a_place_on_the_cabinet_canvas_is_allowed_to_the_worker() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "p").await;
    let placed = as_(
        &k,
        &worker_of(&k, &cab),
        &canvas_of(&k, &cab),
        json!({"type": "place", "kind": "html"}),
    )
    .await;
    assert_eq!(placed, json!({"placed": true}), "{placed}");
}

#[tokio::test]
async fn a_helper_ai_made_as_equipment_reaches_its_memory_store() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "helpers").await;
    let (w, canvas) = (worker_of(&k, &cab), canvas_of(&k, &cab));
    let made = create_on(
        &k,
        &canvas,
        &w,
        json!({"handler_module": FAKE, "id": "helper", "memory_store_id": STORE}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    assert_eq!(meta(&k, "helper", "can_call")[STORE], json!("*"));
    assert_eq!(meta(&k, STORE, "callable_by")["helper"], json!("*"));
    // an agent made with no store names none, and reaches none
    let plain = create_on(
        &k,
        &canvas,
        &w,
        json!({"handler_module": "file_bridge.tools", "id": "plain"}),
    )
    .await;
    assert!(plain.get("error").is_none(), "{plain}");
    assert!(meta(&k, "plain", "can_call").get(STORE).is_none());
}

#[tokio::test]
async fn equipment_of_a_closed_cabinet_is_removed_with_it() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "a").await;
    let canvas = canvas_of(&k, &cab);
    let made = create_on(
        &k,
        &canvas,
        &worker_of(&k, &cab),
        json!({"handler_module": "file_bridge.tools", "id": "a_exec"}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    let res = as_(
        &k,
        MASTER,
        MANAGER,
        json!({"type": "close_cabinet", "id": cab}),
    )
    .await;
    no_error(&res);
    let deleted = deleted_of(&res);
    assert!(
        deleted.contains(&cab)
            && deleted.contains(&canvas)
            && deleted.contains(&"a_exec".to_string())
    );
    assert!(!exists(&k, &canvas) && !exists(&k, "a_exec"));
}

#[tokio::test]
async fn the_equip_verb_is_gone_and_no_longer_documented() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "old").await;
    // the operator's own send reaches the cabinet's handler: no `equip` there
    let res = op(
        &k,
        &cab,
        json!({"type": "equip", "handler_module": "file_bridge.tools"}),
    )
    .await;
    assert_eq!(reason_of(&res), "unhandled", "{res}");
    assert!(!exists(&k, &format!("{}_eq", worker_of(&k, &cab))));
    let docs = CabinetBundle.verb_docs();
    assert!(docs.get("equip").is_none());
    // the verb's name, quoted: `equipment` (a field of the view) is not the verb
    assert!(!VIEW_HTML.contains("\"equip\"") && !VIEW_HTML.contains("`equip`"));
    assert!(!README.contains("`equip`"));
}

// ─── the cabinet's panel ─────────────────────────────────────────

#[tokio::test]
async fn the_status_cell_names_its_canvas_and_opens_it() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "panel").await;
    let canvas = canvas_of(&k, &cab);
    let view = as_(&k, MASTER, &cab, json!({"type": "view"})).await;
    assert_eq!(view["canvas"], json!(canvas), "{view}");
    let v = meta(&k, &cab, "view");
    assert_eq!(v["kind"], json!("html"));
    let html = v["bytes"].as_str().unwrap();
    assert!(html.contains(&format!("const ID = \"{cab}\";")));
    assert!(!html.contains("__CABINET_ID__"));
    assert!(html.contains("fantastic.watch(ID"));
    assert_eq!(v["calls"], json!(["view", "watch"]));
    // the button opens the cabinet's canvas through the page, and is off without one
    assert!(html.contains("id=\"open\""));
    assert!(html.contains("fantastic.openCanvas(canvas)"));
    assert!(html.contains("canvas = v.canvas ?? null"));
    assert!(html.contains("$(\"open\").disabled = !canvas;"));
}

#[tokio::test]
async fn cabinets_lists_each_canvas_and_its_equipment() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "list").await;
    let canvas = canvas_of(&k, &cab);
    let made = create_on(
        &k,
        &canvas,
        &worker_of(&k, &cab),
        json!({"handler_module": "file_bridge.tools", "id": "list_exec"}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    let res = as_(&k, MASTER, MANAGER, json!({"type": "cabinets"})).await;
    let row = &res["cabinets"][0];
    assert_eq!(row["canvas"], json!(canvas));
    assert_eq!(row["equipment"], json!(["list_exec"]));
}

// ─── concurrency: one manager, several opens in one batch ────────

#[tokio::test]
async fn three_opens_in_one_batch_keep_every_grant() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let (r1, r2, r3) = tokio::join!(
        open(&k, "t1", json!({"name": "b1"})),
        open(&k, "t2", json!({"name": "b2"})),
        open(&k, "t3", json!({"name": "b3"})),
    );
    for r in [&r1, &r2, &r3] {
        no_error(r);
    }
    let cabs: Vec<String> = [&r1, &r2, &r3]
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    let workers: Vec<String> = cabs.iter().map(|c| worker_of(&k, c)).collect();
    for w in &workers {
        for p in workers.iter().filter(|x| *x != w) {
            assert_eq!(meta(&k, w, "can_call")[p.as_str()], json!(MESSAGE_VERBS));
            assert_eq!(meta(&k, w, "callable_by")[p.as_str()], json!(MESSAGE_VERBS));
        }
    }
    // the manager calls each worker in full and asks the canvas for a canvas, each open kept
    let manager_cc = meta(&k, MANAGER, "can_call");
    for w in &workers {
        assert_eq!(manager_cc[w.as_str()], json!("*"));
    }
    assert_eq!(manager_cc[CANVAS], json!(["new_canvas"]));
    // the spawning canvas lets the manager ask it for canvases, and nothing else
    assert_eq!(
        meta(&k, CANVAS, "callable_by")[MANAGER],
        json!(["new_canvas"])
    );
}

// ─── status ──────────────────────────────────────────────────────

#[tokio::test]
async fn status_reaches_the_masters_monitor() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "s").await;
    let w = worker_of(&k, &cab);
    let out = as_(
        &k,
        &w,
        &cab,
        json!({"type": "status", "line": "halfway", "progress": 50}),
    )
    .await;
    assert_eq!(out, json!({"ok": true, "cabinet": cab}), "{out}");
    let monitor = as_(&k, MASTER, MANAGER, json!({"type": "cabinets"})).await;
    assert_eq!(monitor["cabinets"][0]["status"]["line"], json!("halfway"));
}

#[tokio::test]
async fn status_refuses_bad_fields_loudly() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "bad").await;
    let w = worker_of(&k, &cab);
    let res = as_(&k, &w, &cab, json!({"type": "status"})).await;
    assert_eq!(reason_of(&res), "bad_arg", "{res}");
    let res = as_(
        &k,
        &w,
        &cab,
        json!({"type": "status", "line": "x", "progress": 101}),
    )
    .await;
    assert_eq!(reason_of(&res), "bad_arg", "{res}");
    let res = as_(
        &k,
        &w,
        &cab,
        json!({"type": "status", "line": "x", "progress": true}),
    )
    .await;
    assert_eq!(reason_of(&res), "bad_arg", "{res}");
}

#[tokio::test]
async fn reflect_tells_the_role_and_the_ceiling() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let manager = as_(&k, MASTER, MANAGER, json!({"type": "reflect"})).await;
    assert_eq!(manager["role"], json!("manager"));
    assert_eq!(manager["canvas_id"], json!(CANVAS));
    assert_eq!(manager["open"], json!(0));
    let cab = open_ok(&k, "r").await;
    let cabinet = as_(&k, MASTER, &cab, json!({"type": "reflect"})).await;
    assert_eq!(cabinet["role"], json!("cabinet"));
    assert_eq!(cabinet["canvas"], json!(canvas_of(&k, &cab)));
}

#[tokio::test]
async fn an_agent_without_a_cabinet_role_refuses_every_verb() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    // a cabinet.tools agent the operator made without a role: the operator's own send
    op(
        &k,
        ROOT,
        json!({"type": "create_agent", "parent_id": ROOT, "id": "roleless",
               "handler_module": HANDLER_MODULE}),
    )
    .await;
    let res = op(&k, "roleless", json!({"type": "reflect"})).await;
    assert_eq!(reason_of(&res), "unhandled", "{res}");
}

// ─── conclude ────────────────────────────────────────────────────

#[tokio::test]
async fn conclude_keeps_the_named_cabinets_and_refuses_unknown_ids() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let mut cabs = Vec::new();
    for i in 0..3 {
        cabs.push(open_ok(&k, &format!("k{i}")).await);
    }
    let before = agent_ids(&k);
    // an unknown id refuses the whole conclude, and deletes nothing
    let res = as_(
        &k,
        MASTER,
        MANAGER,
        json!({"type": "conclude", "keep": [cabs[0], "cabinet_nope"]}),
    )
    .await;
    assert_eq!(reason_of(&res), "unknown_keep", "{res}");
    assert_eq!(res["unknown"], json!(["cabinet_nope"]));
    assert_eq!(agent_ids(&k), before);
    // a kept cabinet stays with its worker and its canvas; the rest are concluded
    let res = as_(
        &k,
        MASTER,
        MANAGER,
        json!({"type": "conclude", "keep": [cabs[1]]}),
    )
    .await;
    no_error(&res);
    assert_eq!(res["kept"], json!([cabs[1]]));
    assert_eq!(res["concluded"], json!([cabs[0], cabs[2]]));
    let kept_canvas = canvas_of(&k, &cabs[1]);
    assert!(
        exists(&k, &cabs[1]) && exists(&k, &kept_canvas) && exists(&k, &worker_of(&k, &cabs[1]))
    );
    for gone in [&cabs[0], &cabs[2]] {
        assert!(!exists(&k, gone));
    }
    let monitor = as_(&k, MASTER, MANAGER, json!({"type": "cabinets"})).await;
    assert_eq!(monitor["count"], json!(1));
}

#[tokio::test]
async fn conclude_keeps_a_result_with_its_canvas_path_and_closes_the_rest() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "res").await;
    let other = open_ok(&k, "oth").await;
    let (w, canvas) = (worker_of(&k, &cab), canvas_of(&k, &cab));
    for id in ["keep_me", "drop_me"] {
        let made = create_on(
            &k,
            &canvas,
            &w,
            json!({"handler_module": "file_bridge.tools", "id": id}),
        )
        .await;
        assert!(made.get("error").is_none(), "{made}");
    }
    // a canvas under the cabinet's canvas holds a result one level deeper (made by the host)
    let made = op(
        &k,
        ROOT,
        json!({"type": "create_agent", "parent_id": canvas, "id": "deep", "handler_module": CANVAS_STUB}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    let made = op(
        &k,
        ROOT,
        json!({"type": "create_agent", "parent_id": "deep", "id": "deep_keep", "handler_module": "file_bridge.tools"}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    let before = agent_ids(&k);
    for bad in [canvas.clone(), w.clone(), "nope".to_string()] {
        let res = as_(
            &k,
            MASTER,
            MANAGER,
            json!({"type": "conclude", "keep": [bad]}),
        )
        .await;
        assert_eq!(reason_of(&res), "unknown_keep", "{res}");
    }
    assert_eq!(agent_ids(&k), before);
    let res = as_(
        &k,
        MASTER,
        MANAGER,
        json!({"type": "conclude", "keep": ["deep_keep"]}),
    )
    .await;
    no_error(&res);
    assert_eq!(res["kept"], json!(["deep_keep"]));
    assert_eq!(res["concluded"], json!([cab, other]));
    // the result, the canvas and the path to it (`deep`) stay; the rest goes
    assert!(exists(&k, "deep_keep") && exists(&k, "deep") && exists(&k, &canvas));
    for gone in [
        &cab,
        &w,
        &"keep_me".to_string(),
        &"drop_me".to_string(),
        &other,
    ] {
        assert!(!exists(&k, gone), "{gone} is gone");
    }
    let deleted = deleted_of(&res);
    assert!(!["deep_keep", "deep", canvas.as_str()]
        .iter()
        .any(|k| deleted.iter().any(|d| d == k)));
    assert!(
        deleted.contains(&cab) && deleted.contains(&w) && deleted.contains(&"drop_me".to_string())
    );
}

#[tokio::test]
async fn conclude_removes_every_cabinet_worker_canvas_and_equipment() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cabs = [open_ok(&k, "n0").await, open_ok(&k, "n1").await];
    let workers: Vec<String> = cabs.iter().map(|c| worker_of(&k, c)).collect();
    let canvases: Vec<String> = cabs.iter().map(|c| canvas_of(&k, c)).collect();
    let made = create_on(
        &k,
        &canvases[0],
        &workers[0],
        json!({"handler_module": "file_bridge.tools", "id": "n0_exec"}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    let res = as_(&k, MASTER, MANAGER, json!({"type": "conclude"})).await;
    no_error(&res);
    assert_eq!(res["concluded"], json!(cabs));
    let deleted = deleted_of(&res);
    for gone in cabs.iter().chain(&workers).chain(&canvases) {
        assert!(deleted.contains(gone), "{gone} is in {deleted:?}");
        assert!(!exists(&k, gone), "{gone}");
    }
    assert!(!exists(&k, "n0_exec"));
    assert!(exists(&k, MANAGER) && exists(&k, MASTER));
    let monitor = as_(&k, MASTER, MANAGER, json!({"type": "cabinets"})).await;
    assert_eq!(monitor["count"], json!(0));
}

// ─── the manager's lock ──────────────────────────────────────────

#[tokio::test]
async fn a_manager_lock_leaves_the_table_when_its_last_holder_lets_go() {
    let (a, b) = ("lock_probe_manager_a", "lock_probe_manager_b");
    let has = |id: &str| MANAGER_LOCKS.lock().unwrap().contains_key(id);
    {
        let _ga = manager_lock(a).await;
        assert!(has(a), "held: the entry is in the table");
        let _gb = manager_lock(b).await; // not blocked by `a`: its own lock
        assert!(has(b));
    }
    assert!(
        !has(a) && !has(b),
        "released: no entry outlives its holders"
    );
}

/// A waiter keeps the entry alive: the lock leaves the table only when nobody waits on it.
#[tokio::test]
async fn a_waiting_holder_keeps_the_manager_lock_in_the_table() {
    let id = "lock_probe_manager_waiter";
    let first = manager_lock(id).await;
    let waiter = tokio::spawn(async move {
        let _second = manager_lock(id).await;
    });
    // let the waiter reach the lock
    tokio::task::yield_now().await;
    drop(first);
    waiter.await.unwrap();
    assert!(!MANAGER_LOCKS.lock().unwrap().contains_key(id));
}
