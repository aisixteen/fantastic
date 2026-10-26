//! `await_set` survives a restart: the wait is `await_open` / `await_hold` /
//! `await_close` rows in the kernel's memory store, rebuilt by `kernel_start`.
//! The twin of python `tests/kernel/test_await_set.py::test_await_set_survives_a_restart`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::test_support::{as_agent, register_root_loader, wire};
use fantastic_kernel::{AgentId, Item, Kernel, Outcome};
use fantastic_ram_store::RamStoreBundle;
use serde_json::{json, Map, Value};

/// Records what each agent is dispatched.
#[derive(Default)]
struct Seen(Mutex<Vec<(String, Value)>>);

impl Seen {
    fn of(&self, agent: &str) -> Vec<Value> {
        let g = self.0.lock().unwrap();
        g.iter()
            .filter(|(a, _)| a == agent)
            .map(|(_, p)| p.clone())
            .collect()
    }
}

struct Rec(Arc<Seen>);

#[async_trait]
impl Bundle for Rec {
    fn name(&self) -> &str {
        "rec.tools"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        if payload["type"] == json!("boot") {
            return Ok(None);
        }
        self.0
             .0
            .lock()
            .unwrap()
            .push((id.0.clone(), payload.clone()));
        Ok(Some(json!({"ok": true})))
    }
}

fn registry(seen: &Arc<Seen>) -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        RamStoreBundle::default(),
    );
    reg.register("rec.tools", Rec(Arc::clone(seen)));
    reg
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value) -> Outcome {
    k.send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

fn root_id(k: &Arc<Kernel>) -> String {
    k.root().unwrap().id.0.clone()
}

async fn as_a(k: &Arc<Kernel>, who: &str, target: &str, payload: Value) -> Value {
    as_agent(AgentId::from(who), call(k, target, payload))
        .await
        .result
}

/// A first kernel on `dir`: lead + w1 + w2 wired, a started memory store.
async fn first(dir: &std::path::Path) -> (Arc<Kernel>, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let k = bootstrap(registry(&seen), BootstrapOptions::one_shot(dir))
        .expect("boot")
        .kernel;
    call(&k, &root_id(&k), json!({"type": "boot"})).await; // mints the guid
    for id in ["lead", "w1", "w2"] {
        let r = call(&k, "kernel", json!({
            "type": "create_agent", "parent_id": root_id(&k), "id": id, "handler_module": "rec.tools",
        })).await.result;
        assert_eq!(r["id"], json!(id), "{r}");
    }
    wire(&k, "lead", &root_id(&k), json!(["await_set"]));
    for w in ["w1", "w2"] {
        wire(&k, w, "lead", json!(["report"]));
    }
    let r = call(&k, "kernel", json!({
        "type": "create_agent", "parent_id": root_id(&k), "id": "memory", "handler_module": "ram_store.tools",
    })).await.result;
    assert_eq!(r["id"], json!("memory"), "{r}");
    let mut patch = Map::new();
    patch.insert("memory_store_id".to_string(), json!("memory"));
    k.update(&AgentId::from(root_id(&k)), patch);
    assert_eq!(k.start(false).await["started"], json!(true));
    (k, seen)
}

/// Flush, persist the tree, and boot a SECOND kernel on the same dir whose store
/// holds what the first one flushed. Returns it started.
async fn restart(
    k: &Arc<Kernel>,
    dir: &std::path::Path,
    down: Duration,
) -> (Arc<Kernel>, Arc<Seen>) {
    k.flush_executions().await;
    let tables = call(k, "memory", json!({"type": "dump"})).await.result["tables"].clone();
    for id in k.agent_ids() {
        let a = k.agents.get(&id).map(|e| Arc::clone(&e)).unwrap();
        fantastic_kernel::persistence::write_record_at(&a.agent_file(), &a.record()).unwrap();
    }
    // the first kernel is DEAD from here: what it does next never reaches the store
    tokio::time::sleep(down).await;
    let seen = Arc::new(Seen::default());
    let k2 = bootstrap(registry(&seen), BootstrapOptions::one_shot(dir))
        .expect("reboot")
        .kernel;
    call(&k2, "memory", json!({"type": "inject", "tables": tables})).await;
    assert_eq!(k2.start(false).await["started"], json!(true));
    (k2, seen)
}

async fn wait_for(seen: &Seen, agent: &str, n: usize) -> Vec<Value> {
    for _ in 0..300 {
        let got = seen.of(agent);
        if got.len() >= n {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{agent} got {:?}, wanted {n}", seen.of(agent));
}

#[tokio::test]
async fn await_set_survives_a_restart() {
    // 1. open on {w1,w2}; w1 reports (held); restart; w2 reports -> ONE message, BOTH reports
    let tmp = tempfile::TempDir::new().unwrap();
    let (k, _) = first(tmp.path()).await;
    let r = as_a(
        &k,
        "lead",
        "kernel",
        json!({"type": "await_set", "ids": ["w1", "w2"], "label": "L"}),
    )
    .await;
    let wid = r["wait_id"].as_i64().expect("{r}");
    let h = as_a(
        &k,
        "w1",
        "lead",
        json!({"type": "report", "state": "done", "result": "one"}),
    )
    .await;
    assert_eq!(h, json!({"held": true, "wait_id": wid}));
    let (k2, seen2) = restart(&k, tmp.path(), Duration::ZERO).await;
    let h2 = as_a(
        &k2,
        "w2",
        "lead",
        json!({"type": "report", "state": "done", "result": "two"}),
    )
    .await;
    assert_eq!(
        h2,
        json!({"held": true, "wait_id": wid}),
        "the restored wait holds the report"
    );
    let got = wait_for(&seen2, "lead", 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(seen2.of("lead").len(), 1, "ONE message");
    assert_eq!(got[0]["wait_id"], json!(wid));
    assert_eq!(got[0]["label"], json!("L"));
    assert_eq!(got[0]["timed_out"], json!(false));
    assert_eq!(
        got[0]["done"],
        json!([
            {"id": "w1", "state": "done", "result": "one"},
            {"id": "w2", "state": "done", "result": "two"},
        ])
    );

    // 2. a deadline that passed while the kernel was down: completes at once, timed out
    let tmp = tempfile::TempDir::new().unwrap();
    let (k, _) = first(tmp.path()).await;
    as_a(
        &k,
        "lead",
        "kernel",
        json!({"type": "await_set", "ids": ["w1", "w2"], "timeout_s": 0.3}),
    )
    .await;
    as_a(
        &k,
        "w1",
        "lead",
        json!({"type": "report", "state": "done", "result": "one"}),
    )
    .await;
    let (_k2, seen2) = restart(&k, tmp.path(), Duration::from_millis(450)).await;
    let got = wait_for(&seen2, "lead", 1).await;
    assert_eq!(got[0]["timed_out"], json!(true));
    assert_eq!(got[0]["missing"], json!(["w2"]));
    assert_eq!(
        got[0]["done"],
        json!([{"id": "w1", "state": "done", "result": "one"}])
    );

    // 3. a wait closed before the restart is not rebuilt: a report is delivered as itself
    let tmp = tempfile::TempDir::new().unwrap();
    let (k, _) = first(tmp.path()).await;
    as_a(
        &k,
        "lead",
        "kernel",
        json!({"type": "await_set", "ids": ["w1"]}),
    )
    .await;
    as_a(
        &k,
        "w1",
        "lead",
        json!({"type": "report", "state": "done", "result": "x"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await; // the one message, then the close row
    let (k2, seen2) = restart(&k, tmp.path(), Duration::ZERO).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(seen2.of("lead").is_empty(), "nothing re-delivered on boot");
    let r = as_a(
        &k2,
        "w1",
        "lead",
        json!({"type": "report", "state": "done", "result": "y"}),
    )
    .await;
    assert_eq!(r, json!({"ok": true}), "not held: the wait is closed");
}
