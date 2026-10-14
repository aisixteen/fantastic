//! The ROW STATE EVENT — the twin of python
//! `tests/kernel/test_row_event.py`: every EXECUTION row the kernel puts is
//! announced on the state stream as `{agent_id, kind: "row", row: {id, kind,
//! parent?, caller?, settled?}}`, content-free; nothing inside the kernel's own
//! ledger flush is published, so an idle kernel publishes nothing.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::BundleRegistry;
use fantastic_kernel::test_support::register_root_loader;
use fantastic_kernel::{AgentId, Item, Kernel, Outcome};
use fantastic_ram_store::RamStoreBundle;
use serde_json::{json, Map, Value};

fn seeded() -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        RamStoreBundle::default(),
    );
    bootstrap(reg, BootstrapOptions::in_memory())
        .expect("boot")
        .kernel
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value) -> Outcome {
    k.send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

fn root_id(k: &Arc<Kernel>) -> String {
    k.root().unwrap().id.0.clone()
}

fn tap(k: &Arc<Kernel>) -> (Arc<Mutex<Vec<Value>>>, fantastic_kernel::SubscriberToken) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = Arc::clone(&seen);
    let token = k.add_state_subscriber(Arc::new(move |ev: &Value, _| {
        if ev["kind"] == json!("row") {
            s.lock().unwrap().push(ev.clone());
        }
    }));
    (seen, token)
}

#[tokio::test]
async fn a_call_announces_its_call_row_and_its_settle() {
    let k = seeded();
    let root = root_id(&k);
    let (seen, token) = tap(&k);
    let out = call(&k, "kernel", json!({"type": "list_agents"})).await;
    k.remove_state_subscriber(token);
    let pid = json!(out.pid.expect("a pid").get());
    let rows = seen.lock().unwrap().clone();
    let c = rows
        .iter()
        .find(|e| e["row"]["kind"] == json!("call") && e["row"]["id"] == pid)
        .expect("the call row");
    assert_eq!(c["agent_id"], json!(root), "{c}");
    assert_eq!(c["row"]["caller"], Value::Null, "the operator's call: {c}");
    let s = rows
        .iter()
        .find(|e| e["row"]["kind"] == json!("settle") && e["row"]["parent"] == pid)
        .expect("the settle row");
    assert_eq!(s["agent_id"], json!(root), "{s}");
    assert_eq!(s["row"]["settled"], json!(root), "{s}");
    for e in &rows {
        let keys: Vec<&String> = e["row"].as_object().unwrap().keys().collect();
        assert!(
            keys.iter()
                .all(|k| ["id", "kind", "parent", "caller", "settled"].contains(&k.as_str())),
            "content-free: {e}"
        );
        assert!(e.get("ts").is_some(), "{e}");
    }
}

#[tokio::test]
async fn the_flush_publishes_nothing_so_idle_is_silent() {
    let k = seeded();
    let r = call(
        &k,
        "kernel",
        json!({"type": "create_agent", "parent_id": root_id(&k), "id": "memory",
               "handler_module": "ram_store.tools"}),
    )
    .await
    .result;
    assert_eq!(r["id"], json!("memory"), "{r}");
    let mut patch = Map::new();
    patch.insert("memory_store_id".to_string(), json!("memory"));
    k.update(&AgentId::from(root_id(&k)), patch);
    assert_eq!(k.start(false).await["started"], json!(true));
    tokio::time::sleep(Duration::from_millis(500)).await; // the start's rows flushed
    let (seen, token) = tap(&k);
    let mut row = Map::new();
    row.insert("kind".into(), json!("call"));
    row.insert("verb".into(), json!("after"));
    let rid = k.rows_put(&AgentId::from("probe"), row).expect("put");
    for _ in 0..100 {
        if !k.rows.contains(rid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!k.rows.contains(rid), "never flushed");
    tokio::time::sleep(Duration::from_millis(300)).await;
    k.remove_state_subscriber(token);
    let ids: Vec<Value> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|e| e["row"]["id"].clone())
        .collect();
    assert_eq!(
        ids,
        vec![json!(rid)],
        "the flush announced rows, or more came while idle"
    );
}
