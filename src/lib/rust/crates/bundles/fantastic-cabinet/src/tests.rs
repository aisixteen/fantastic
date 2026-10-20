//! Unit tests for the cabinet bundle, on a real kernel — the Rust mirror of python
//! `cabinet/tests/test_cabinet.py`.
//!
//! Every caller acts through `as_agent` (the dispatch identity of the sending agent),
//! so each send is gated as that agent: the master opens cabinets, a worker messages a
//! peer or creates equipment, and the gate refuses what the grants do not allow. The
//! operator wires the manager once, as the readme says. The AI backend is a scripted
//! `fake_ai.tools` that logs every `send` it receives, with who sent it.

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
const STORE: &str = "memory";
const ROOT: &str = "kernel_state";
const FAKE: &str = "fake_ai.tools";
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

/// A kernel with the operator's wiring: a MASTER AI (on a memory store), a MANAGER that
/// may create, update and delete on the root, and the root that names the manager.
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
            "type": "create_agent", "parent_id": ROOT, "id": MANAGER,
            "handler_module": HANDLER_MODULE, "cabinet_role": "manager",
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
        // the root takes exactly one verb from each worker: create_agent
        assert_eq!(can_call_root(&k, w), json!(["create_agent"]));
        assert_eq!(root_callable[w.as_str()], json!(["create_agent"]));
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

fn can_call_root(k: &Kernel, w: &str) -> Value {
    meta(k, w, "can_call")[ROOT].clone()
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
    let res = as_(
        &k,
        MASTER,
        MANAGER,
        json!({"type": "close_cabinet", "id": gone_cab}),
    )
    .await;
    no_error(&res);
    let deleted: Vec<&str> = res["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(deleted.contains(&gone_cab.as_str()) && deleted.contains(&gone_worker.as_str()));
    assert!(!exists(&k, &gone_cab) && !exists(&k, &gone_worker));
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

// ─── the default backend ─────────────────────────────────────────

#[tokio::test]
async fn the_default_backend_is_the_existing_master() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let res = open(&k, "solve part one", json!({})).await;
    no_error(&res);
    assert_eq!(res["backend"], json!(MASTER));
    assert_eq!(res["new_backend"], json!(false));
    assert!(res.get("warning").is_none());
    let worker = res["worker"].as_str().unwrap();
    assert_eq!(
        k.agents
            .get(&AgentId::from(worker))
            .unwrap()
            .record()
            .handler_module
            .as_deref(),
        Some(FAKE)
    );
    assert_eq!(meta(&k, worker, "endpoint"), json!("http://ollama.test"));
    assert_eq!(meta(&k, worker, "model"), json!("model-a"));
    assert_eq!(meta(&k, worker, "memory_store_id"), json!(STORE));
    assert_eq!(
        k.agents
            .get(&AgentId::from(worker))
            .unwrap()
            .record()
            .parent_id,
        res["id"].as_str().map(str::to_string)
    );
}

#[tokio::test]
async fn setup_over_the_backend_is_flagged_as_a_new_backend() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let res = open(
        &k,
        "a different endpoint",
        json!({"setup": {"endpoint": "http://other.test"}}),
    )
    .await;
    no_error(&res);
    assert_eq!(res["new_backend"], json!(true));
    assert!(res["warning"].as_str().unwrap().contains("discouraged"));
    assert_eq!(
        meta(&k, res["worker"].as_str().unwrap(), "endpoint"),
        json!("http://other.test")
    );
}

#[tokio::test]
async fn open_refuses_loudly_on_bad_inputs() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    assert_eq!(reason_of(&open(&k, "  ", json!({})).await), "missing_arg");
    assert_eq!(
        reason_of(&open(&k, "x", json!({"backend": "nobody"})).await),
        "unknown_agent"
    );
    let res = open(&k, "x", json!({"backend": MANAGER})).await;
    assert_eq!(reason_of(&res), "bad_arg");
    assert!(
        res["error"].as_str().unwrap().contains("not an AI backend"),
        "{res}"
    );
    let res = open(&k, "x", json!({"setup": {"can_call": {"*": "*"}}})).await;
    assert_eq!(reason_of(&res), "bad_arg");
    assert!(res["error"].as_str().unwrap().contains("wiring"), "{res}");
    // an operator send has no master to wire to, and makes nothing
    let before = agent_ids(&k);
    let res = op(&k, MANAGER, json!({"type": "open_cabinet", "task": "x"})).await;
    assert!(res["error"].as_str().unwrap().contains("master"), "{res}");
    assert_eq!(agent_ids(&k), before);
}

// ─── the ceiling ─────────────────────────────────────────────────

#[tokio::test]
async fn the_ceiling_refuses_loudly_and_makes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    k.update(
        &AgentId::from(MANAGER),
        json!({"max_cabinets": 2}).as_object().unwrap().clone(),
    );
    open_ok(&k, "one").await;
    open_ok(&k, "two").await;
    let before = agent_ids(&k);
    let res = open(&k, "three", json!({"name": "three"})).await;
    assert_eq!(reason_of(&res), "ceiling", "{res}");
    assert_eq!(res["ceiling"], json!(2));
    assert_eq!(res["open"], json!(2));
    assert!(res["error"].as_str().unwrap().contains("ceiling is 2"));
    assert_eq!(agent_ids(&k), before);
}

#[tokio::test]
async fn a_bad_ceiling_is_refused_loudly() {
    for bad_value in [json!(0), json!(-1), json!(true), json!("2"), json!(2.5)] {
        let tmp = TempDir::new().unwrap();
        let (k, _) = world(&tmp).await;
        k.update(
            &AgentId::from(MANAGER),
            json!({"max_cabinets": bad_value.clone()})
                .as_object()
                .unwrap()
                .clone(),
        );
        let before = agent_ids(&k);
        let res = open(&k, "x", json!({"name": "x"})).await;
        assert_eq!(reason_of(&res), "bad_arg", "{bad_value}: {res}");
        assert_eq!(res["arg"], json!("max_cabinets"));
        assert_eq!(agent_ids(&k), before);
    }
}

// ─── the worker's stores ─────────────────────────────────────────

#[tokio::test]
async fn a_worker_on_a_missing_memory_store_is_refused() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let before = agent_ids(&k);
    let res = open(
        &k,
        "x",
        json!({"name": "x", "setup": {"memory_store_id": "no_such_store"}}),
    )
    .await;
    assert_eq!(reason_of(&res), "unknown_agent", "{res}");
    assert_eq!(res["id"], json!("no_such_store"));
    assert_eq!(agent_ids(&k), before);
}

#[tokio::test]
async fn a_worker_on_a_missing_credential_store_is_refused() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    k.update(
        &AgentId::from(MASTER),
        json!({"credentials": {"store": "no_keys", "ref": "api"}})
            .as_object()
            .unwrap()
            .clone(),
    );
    let before = agent_ids(&k);
    let res = open(&k, "x", json!({"name": "x"})).await;
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
    k.update(
        &AgentId::from(MASTER),
        json!({"credentials": {"store": "keys", "ref": "api"}})
            .as_object()
            .unwrap()
            .clone(),
    );
    let res = open(&k, "x", json!({"name": "x"})).await;
    no_error(&res);
    let w = res["worker"].as_str().unwrap();
    assert_eq!(meta(&k, "keys", "callable_by")[w], json!("*"));
    assert_eq!(meta(&k, w, "can_call")["keys"], json!("*"));
    assert_eq!(
        meta(&k, w, "credentials"),
        json!({"store": "keys", "ref": "api"})
    );
}

// ─── wiring failures remove the half-made cabinet ────────────────

#[tokio::test]
async fn a_wiring_failure_removes_the_half_made_cabinet() {
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
    assert_eq!(agent_ids(&k), before);
}

#[tokio::test]
async fn a_worker_that_cannot_be_created_removes_its_cabinet() {
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
    assert!(!exists(&k, "cabinet_sq"));
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
    assert!(res.get("error").is_some(), "{res}");
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

// ─── peer messages and equipment ─────────────────────────────────

#[tokio::test]
async fn a_worker_messages_a_peer_and_the_peer_receives_it() {
    let tmp = TempDir::new().unwrap();
    let (k, log) = world(&tmp).await;
    let a = worker_of(&k, &open_ok(&k, "a").await);
    let b = worker_of(&k, &open_ok(&k, "b").await);
    let res = as_(
        &k,
        &a,
        &b,
        json!({"type": "send", "text": "peer hello from a"}),
    )
    .await;
    no_error(&res);
    let heard = log.lock().unwrap().clone();
    // the peer's turn received the text, sent by worker a
    assert!(
        heard.iter().any(|(to, text, origin)| to == &b
            && text == "peer hello from a"
            && origin.as_deref() == Some(a.as_str())),
        "{heard:?}"
    );
}

#[tokio::test]
async fn a_worker_reaches_a_peer_only_with_messages() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let a = worker_of(&k, &open_ok(&k, "a").await);
    let b = worker_of(&k, &open_ok(&k, "b").await);
    // the peer answers `reflect`, and nothing that changes it
    no_error(&as_(&k, &a, &b, json!({"type": "reflect"})).await);
    let refused = as_(&k, &a, &b, json!({"type": "stop"})).await;
    assert_eq!(reason_of(&refused), "unauthorized", "{refused}");
    assert_eq!(refused["side"], json!("egress"));
    // and the peer may not clear its neighbour either
    assert_eq!(
        reason_of(&as_(&k, &b, &a, json!({"type": "clear"})).await),
        "unauthorized"
    );
}

#[tokio::test]
async fn a_worker_reaches_its_master_only_with_messages() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let w = worker_of(&k, &open_ok(&k, "a").await);
    no_error(&as_(&k, &w, MASTER, json!({"type": "reflect"})).await);
    assert_eq!(
        reason_of(&as_(&k, &w, MASTER, json!({"type": "stop"})).await),
        "unauthorized"
    );
    // the master calls its worker in full
    no_error(&as_(&k, MASTER, &w, json!({"type": "reflect"})).await);
}

#[tokio::test]
async fn equipment_is_made_under_the_worker_and_reached_by_it_alone() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let a = worker_of(&k, &open_ok(&k, "a").await);
    let b = worker_of(&k, &open_ok(&k, "b").await);
    // the worker sends create_agent to the root, naming ITSELF as the parent
    let made = as_(
        &k,
        &a,
        ROOT,
        json!({"type": "create_agent", "parent_id": a, "id": "a_exec",
               "handler_module": FAKE, "callable_by": {a.clone(): "*"}}),
    )
    .await;
    no_error(&made);
    assert_eq!(
        k.agents
            .get(&AgentId::from("a_exec"))
            .unwrap()
            .record()
            .parent_id,
        Some(a.clone())
    );
    // the worker reaches its equipment through its `$subtree` grant
    no_error(&as_(&k, &a, "a_exec", json!({"type": "reflect"})).await);
    // the peer does not: the equipment grants its own worker and nothing else
    assert_eq!(
        reason_of(&as_(&k, &b, "a_exec", json!({"type": "reflect"})).await),
        "unauthorized"
    );
}

#[tokio::test]
async fn a_worker_on_the_root_may_only_create() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let a = worker_of(&k, &open_ok(&k, "a").await);
    // the root's one grant to a worker is create_agent: no update, no delete
    for verb in ["update_agent", "delete_agent"] {
        let refused = as_(
            &k,
            &a,
            ROOT,
            json!({"type": verb, "id": MASTER, "description": "x"}),
        )
        .await;
        assert_eq!(reason_of(&refused), "unauthorized", "{verb}: {refused}");
    }
    assert!(exists(&k, MASTER));
}

#[tokio::test]
async fn a_worker_cannot_reach_a_peers_status_or_the_manager() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let a_cab = open_ok(&k, "a").await;
    let b_cab = open_ok(&k, "b").await;
    let a = worker_of(&k, &a_cab);
    assert_eq!(
        reason_of(&as_(&k, &a, &b_cab, json!({"type": "status", "line": "hijack"})).await),
        "unauthorized"
    );
    assert_eq!(
        reason_of(&as_(&k, &a, MANAGER, json!({"type": "cabinets"})).await),
        "unauthorized"
    );
    // its own cabinet's status is its one channel up
    assert_eq!(
        as_(&k, &a, &a_cab, json!({"type": "status", "line": "mine"})).await["ok"],
        json!(true)
    );
}

// ─── status up to the master's view ──────────────────────────────

#[tokio::test]
async fn status_reaches_the_masters_monitor() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "rep").await;
    let w = worker_of(&k, &cab);
    let res = as_(
        &k,
        &w,
        &cab,
        json!({"type": "status", "line": "halfway", "progress": 50, "result": {"n": 3}}),
    )
    .await;
    assert_eq!(res, json!({"ok": true, "cabinet": cab}));
    let listed = as_(&k, MASTER, MANAGER, json!({"type": "cabinets"})).await;
    let row = listed["cabinets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(cab))
        .unwrap();
    assert_eq!(row["status"]["line"], json!("halfway"));
    assert_eq!(row["status"]["progress"], json!(50));
    assert_eq!(row["status"]["result"], json!({"n": 3}));
    let view = as_(&k, MASTER, &cab, json!({"type": "view"})).await;
    assert_eq!(view["status"]["line"], json!("halfway"));
    assert_eq!(view["worker"], json!(w));
}

#[tokio::test]
async fn status_is_emitted_live_on_the_cabinet_id() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "live").await;
    let w = worker_of(&k, &cab);
    // a watcher of the cabinet's id: the view draws from exactly these events
    let mut rx = k.ensure_inbox(&AgentId::from("watch_live"));
    k.watch(&AgentId::from(cab.as_str()), AgentId::from("watch_live"))
        .await;
    as_(
        &k,
        &w,
        &cab,
        json!({"type": "status", "line": "compiling", "progress": 40, "result": null}),
    )
    .await;
    let mut updates = Vec::new();
    while let Some(item) = rx.try_recv() {
        if item.payload.get("type") == Some(&json!("update")) {
            updates.push(item.payload);
        }
    }
    assert_eq!(updates.len(), 1, "{updates:?}");
    let update = &updates[0];
    assert_eq!(update["cabinet"], json!(cab));
    assert_eq!(update["status"]["line"], json!("compiling"));
    assert_eq!(update["status"]["progress"], json!(40));
    assert!(update["status"]["at"].is_number());
}

#[tokio::test]
async fn status_refuses_bad_fields_loudly() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "bad").await;
    let w = worker_of(&k, &cab);
    assert_eq!(
        reason_of(&as_(&k, &w, &cab, json!({"type": "status"})).await),
        "bad_arg"
    );
    assert_eq!(
        reason_of(
            &as_(
                &k,
                &w,
                &cab,
                json!({"type": "status", "line": "x", "progress": 101})
            )
            .await
        ),
        "bad_arg"
    );
    assert_eq!(
        reason_of(
            &as_(
                &k,
                &w,
                &cab,
                json!({"type": "status", "line": "x", "progress": true})
            )
            .await
        ),
        "bad_arg"
    );
    assert_eq!(meta(&k, &cab, "status")["line"], json!(""));
}

// ─── conclusion ──────────────────────────────────────────────────

#[tokio::test]
async fn conclude_removes_every_cabinet_worker_and_equipment() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cabs = vec![open_ok(&k, "n0").await, open_ok(&k, "n1").await];
    let workers: Vec<String> = cabs.iter().map(|c| worker_of(&k, c)).collect();
    no_error(
        &as_(
            &k,
            &workers[0],
            ROOT,
            json!({"type": "create_agent", "parent_id": workers[0], "id": "n0_exec",
                   "handler_module": FAKE, "callable_by": {workers[0].clone(): "*"}}),
        )
        .await,
    );
    let res = as_(&k, MASTER, MANAGER, json!({"type": "conclude"})).await;
    no_error(&res);
    let mut concluded: Vec<String> = res["concluded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    concluded.sort();
    let mut expect = cabs.clone();
    expect.sort();
    assert_eq!(concluded, expect);
    let deleted: Vec<&str> = res["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for gone in cabs.iter().chain(workers.iter()) {
        assert!(deleted.contains(&gone.as_str()), "{gone}");
        assert!(!exists(&k, gone), "{gone}");
    }
    assert!(deleted.contains(&"n0_exec") && !exists(&k, "n0_exec"));
    assert!(exists(&k, MANAGER) && exists(&k, MASTER));
    let master_can_call = meta(&k, MASTER, "can_call");
    let master_callable_by = meta(&k, MASTER, "callable_by");
    for w in &workers {
        assert!(master_can_call.get(w).is_none() && master_callable_by.get(w).is_none());
    }
    assert_eq!(
        as_(&k, MASTER, MANAGER, json!({"type": "cabinets"})).await["count"],
        json!(0)
    );
}

#[tokio::test]
async fn reflect_tells_the_role_and_the_ceiling() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    k.update(
        &AgentId::from(MANAGER),
        json!({"max_cabinets": 4}).as_object().unwrap().clone(),
    );
    let m = as_(&k, MASTER, MANAGER, json!({"type": "reflect"})).await;
    assert_eq!(m["role"], json!("manager"));
    assert_eq!(m["max_cabinets"], json!(4));
    assert_eq!(m["open"], json!(0));
    let cab = open_ok(&k, "x").await;
    let c = as_(&k, MASTER, &cab, json!({"type": "reflect"})).await;
    assert_eq!(c["role"], json!("cabinet"));
    assert_eq!(c["worker"], json!(worker_of(&k, &cab)));
}

#[tokio::test]
async fn an_agent_without_a_cabinet_role_refuses_every_verb() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let made = op(
        &k,
        ROOT,
        json!({"type": "create_agent", "parent_id": ROOT, "id": "loose", "handler_module": HANDLER_MODULE}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    let out = op(&k, "loose", json!({"type": "reflect"})).await;
    assert_eq!(reason_of(&out), "unhandled", "{out}");
    assert!(out["error"].as_str().unwrap().contains("no cabinet_role"));
}

#[tokio::test]
async fn each_cabinet_is_a_live_html_panel_of_its_own_id() {
    let tmp = TempDir::new().unwrap();
    let (k, _) = world(&tmp).await;
    let cab = open_ok(&k, "panel").await;
    let view = meta(&k, &cab, "view");
    assert_eq!(view["kind"], json!("html"));
    let bytes = view["bytes"].as_str().unwrap();
    assert!(bytes.contains(&format!("const ID = \"{cab}\";")), "{bytes}");
    assert!(!bytes.contains("__CABINET_ID__"));
    assert!(bytes.contains("fantastic.watch(ID"));
    assert_eq!(view["calls"], json!(["view", "watch"]));
}
