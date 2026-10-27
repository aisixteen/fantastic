//! Pause / resume (#219): a paused agent holds its messages in its memory store
//! and runs none; resume writes a resumed row, then runs the held ones in order.
//! Self-contained: `tests.rs`'s harness is private to that module.
//!
//! The agent may serve other passes too (a context recall after boot), so each
//! assertion finds the request that CONTAINS the held text, not a fixed pass index.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::agent_loop::BackendConfig;
use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderStream};
use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

const HANDLER: &str = "pause_mock.tools";

const CFG: BackendConfig = BackendConfig {
    name: "pause_mock",
    model_window: crate::constants::DEFAULT_CONTEXT_WINDOW,
    provider_cache: false,
};

/// Every model request one agent was served, in order.
#[derive(Default)]
struct Served {
    requests: Mutex<Vec<Vec<Value>>>,
}

static SERVED: OnceLock<Mutex<HashMap<String, Arc<Served>>>> = OnceLock::new();

fn served_for(agent: &AgentId) -> Arc<Served> {
    let map = SERVED.get_or_init(|| Mutex::new(HashMap::new()));
    Arc::clone(
        map.lock()
            .unwrap()
            .entry(agent.as_str().to_string())
            .or_default(),
    )
}

struct Mock {
    served: Arc<Served>,
}

#[async_trait]
impl Provider for Mock {
    async fn chat(
        &self,
        messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<ProviderStream, ProviderError> {
        self.served.requests.lock().unwrap().push(messages.to_vec());
        let ev: Vec<Result<ProviderEvent, ProviderError>> =
            vec![Ok(ProviderEvent::Token("answer".to_string()))];
        Ok(Box::pin(futures_util::stream::iter(ev)))
    }
    fn model(&self) -> String {
        "pause-model".to_string()
    }
}

struct PauseBundle;

#[async_trait]
impl Bundle for PauseBundle {
    fn name(&self) -> &str {
        CFG.name
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let served = served_for(agent_id);
        let reply = crate::verbs::handle_common(agent_id, payload, kernel, CFG, move |_client| {
            Arc::new(Mock {
                served: Arc::clone(&served),
            }) as Arc<dyn Provider>
        })
        .await?
        .unwrap_or_else(|| json!({"error": "pause_mock: unknown type"}));
        Ok(Some(reply))
    }
}

/// A kernel with the mock backend and the ram memory store, and one agent on it.
async fn setup(tmp: &TempDir, tag: &str) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER, PauseBundle);
    kernel.bundles.register("stub_registry.tools", StubRegistry);
    kernel.bundles.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    fantastic_ram_store::test_support::ensure_memory(&kernel).await;

    let id = format!("pause_{tag}");
    let memory =
        fantastic_ram_store::test_support::memory_meta(&kernel, &id, json!({"cli": "*"})).await;
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "kernel",
        "handler_module": HANDLER,
        "id": id,
        "model": "pause-model",
        "context_window": 32768,
    });
    for (k, v) in memory {
        create[k] = v;
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    let agent = AgentId::from(id.as_str());
    (kernel, agent)
}

async fn send(kernel: &Arc<Kernel>, to: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(to.clone(), payload)])
        .await
        .remove(0)
        .result
}

/// Poll until a served request contains `needle`; return that request rendered.
/// Failing after the poll budget beats waiting out the send ceiling.
async fn await_request(served: &Arc<Served>, needle: &str) -> Option<String> {
    for _ in 0..250 {
        let found = served
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| Value::Array(r.clone()).to_string())
            .find(|s| s.contains(needle));
        if found.is_some() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

#[tokio::test]
async fn pause_stops_the_generation_and_holds_a_send() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = setup(&tmp, "hold").await;
    let served = served_for(&id);
    let p = send(&kernel, &id, json!({"type": "pause"})).await;
    assert_eq!(p["paused"], true);
    let r = send(&kernel, &id, json!({"type": "send", "text": "held-probe"})).await;
    assert_eq!(r["held"], 1, "{r}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let ran = served
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| Value::Array(r.clone()).to_string().contains("held-probe"));
    assert!(!ran, "a held send ran");
}

#[tokio::test]
async fn resume_runs_held_after_the_resumed_row() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = setup(&tmp, "resume").await;
    let served = served_for(&id);
    send(&kernel, &id, json!({"type": "pause"})).await;
    send(&kernel, &id, json!({"type": "send", "text": "held-probe"})).await;
    let r = send(&kernel, &id, json!({"type": "resume"})).await;
    assert_eq!(r["resumed"], true, "{r}");
    assert_eq!(r["released"], 1, "{r}");
    let s = await_request(&served, "held-probe")
        .await
        .expect("the held turn never reached the model");
    let resumed_at = s
        .find("resumed after a pause")
        .expect("no resumed row in that request");
    let held_at = s.find("held-probe").expect("held text vanished");
    assert!(
        resumed_at < held_at,
        "resumed row must come before the held turn"
    );
}

#[tokio::test]
async fn held_messages_keep_their_order() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = setup(&tmp, "order").await;
    let served = served_for(&id);
    send(&kernel, &id, json!({"type": "pause"})).await;
    for t in ["m1-probe", "m2-probe", "m3-probe"] {
        send(&kernel, &id, json!({"type": "send", "text": t})).await;
    }
    let r = send(&kernel, &id, json!({"type": "resume"})).await;
    assert_eq!(r["released"], 3, "{r}");
    let s = await_request(&served, "m3-probe")
        .await
        .expect("not all held turns reached the model");
    let a = s.find("m1-probe").expect("m1 missing");
    let b = s.find("m2-probe").expect("m2 missing");
    let c = s.find("m3-probe").expect("m3 missing");
    assert!(a < b && b < c, "held turns out of order");
}

#[tokio::test]
async fn pause_twice_and_resume_twice_are_noops() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = setup(&tmp, "twice").await;
    let p1 = send(&kernel, &id, json!({"type": "pause"})).await;
    let p2 = send(&kernel, &id, json!({"type": "pause"})).await;
    assert_eq!(p1["paused"], true);
    assert_eq!(p2["paused"], true);
    let r1 = send(&kernel, &id, json!({"type": "resume"})).await;
    let r2 = send(&kernel, &id, json!({"type": "resume"})).await;
    assert_eq!(r1["resumed"], true, "{r1}");
    assert_eq!(r2["resumed"], false, "{r2}");
    assert_eq!(r2["released"], 0, "{r2}");
}

#[test]
fn pause_and_resume_are_documented_like_every_other_verb() {
    let docs = crate::verbs::verb_docs();
    for verb in ["pause", "resume"] {
        let d = docs[verb].as_str().unwrap_or_default();
        assert!(
            d.starts_with("args: "),
            "`docs` carries no signature for {verb}: {d:?}"
        );
    }
    assert!(docs["pause"]
        .as_str()
        .unwrap()
        .contains("{paused:bool, held:int}"));
    assert!(docs["resume"]
        .as_str()
        .unwrap()
        .contains("{resumed:bool, released:int}"));
}

/// A stand-in registry: answers every `get_count` with `reply`, and records the asks.
struct StubRegistry;

static REG_ASKS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
static REG_REPLY: Mutex<Option<Value>> = Mutex::new(None);

#[async_trait]
impl Bundle for StubRegistry {
    fn name(&self) -> &str {
        "stub_registry"
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        if payload["type"] != "get_count" {
            return Ok(None);
        }
        REG_ASKS.lock().unwrap().push(payload.clone());
        let r = REG_REPLY.lock().unwrap().clone();
        Ok(Some(r.unwrap_or(json!({"running": 0, "max": null}))))
    }
}

#[tokio::test]
async fn status_reports_paused() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = setup(&tmp, "status").await;
    assert_eq!(
        send(&kernel, &id, json!({"type": "status"})).await["paused"],
        false
    );
    send(&kernel, &id, json!({"type": "pause"})).await;
    assert_eq!(
        send(&kernel, &id, json!({"type": "status"})).await["paused"],
        true
    );
    send(&kernel, &id, json!({"type": "resume"})).await;
    assert_eq!(
        send(&kernel, &id, json!({"type": "status"})).await["paused"],
        false
    );
}

#[tokio::test]
async fn resume_unrestricted_without_spawned_by() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = setup(&tmp, "nospawn").await;
    send(&kernel, &id, json!({"type": "pause"})).await;
    let r = send(&kernel, &id, json!({"type": "resume"})).await;
    assert_eq!(r["resumed"], true, "{r}");
}

/// A registry agent plus a pausable child it "spawned" (`spawned_by`, may call it).
async fn spawned_child(tmp: &TempDir, tag: &str, reply: Value) -> (Arc<Kernel>, AgentId) {
    let (kernel, id) = setup(tmp, tag).await;
    let reg = format!("stubreg_{tag}");
    let made = send(
        &kernel,
        &AgentId::from("kernel_state"),
        json!({"type": "create_agent", "parent_id": "kernel", "id": reg,
               "handler_module": "stub_registry.tools", "callable_by": {id.as_str(): "*"}}),
    )
    .await;
    assert!(made.get("error").is_none(), "{made}");
    send(
        &kernel,
        &AgentId::from("kernel_state"),
        json!({"type": "update_agent", "id": id.as_str(), "spawned_by": reg, "role": "writer",
               "can_call": {reg.clone(): "*", "memory": "*"}}),
    )
    .await;
    *REG_REPLY.lock().unwrap() = Some(reply);
    (kernel, id)
}

#[tokio::test]
async fn resume_refused_when_registry_says_limit() {
    let tmp = TempDir::new().unwrap();
    let limit = json!({"error": "limit", "scope": "total", "role": "writer",
                       "running": 1, "max": 1, "reason": "all roles together are at the limit"});
    let (kernel, id) = spawned_child(&tmp, "limit", limit.clone()).await;
    send(&kernel, &id, json!({"type": "pause"})).await;
    let r = send(&kernel, &id, json!({"type": "resume"})).await;
    assert_eq!(r, limit, "{r}");
    let s = send(&kernel, &id, json!({"type": "status"})).await;
    assert_eq!(s["paused"], true, "no resume row was written: {s}");
    let ask = REG_ASKS.lock().unwrap().last().cloned().unwrap();
    assert_eq!(ask["resume"], id.as_str());
    assert_eq!(ask["role"], "writer");
    // the registry makes room: the same resume goes through
    *REG_REPLY.lock().unwrap() = Some(json!({"running": 0, "max": 1}));
    let r = send(&kernel, &id, json!({"type": "resume"})).await;
    assert_eq!(r["resumed"], true, "{r}");
}
