//! Owner decisions 2026-10-03 (audit 2, set 2) on the kernel's own service loops,
//! against the real `ram_store` — the twin of python
//! `tests/kernel/test_hook_identity_service_tasks_quiet_flush.py`.
//!
//! - KV-5: `cancel` refuses the execution flusher; `ps` still lists it and it
//!   keeps flushing.
//! - CLI-B: the kernel's own ledger-flush send publishes NOTHING on the state
//!   stream (its rows stay recorded by the flush row) — a long-poll waiter is not
//!   woken; an id block reserved inside the flush is quiet too.

use std::sync::atomic::{AtomicBool, Ordering};
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

/// A ram_store `memory` under the root, named by the root; the kernel started.
async fn started() -> Arc<Kernel> {
    let k = seeded();
    let r = call(
        &k,
        "kernel",
        json!({
            "type": "create_agent", "parent_id": root_id(&k), "id": "memory",
            "handler_module": "ram_store.tools", "operator": true, "admins": ["kernel"],
        }),
    )
    .await
    .result;
    assert_eq!(r["id"], json!("memory"), "{r}");
    let mut patch = Map::new();
    patch.insert("memory_store_id".to_string(), json!("memory"));
    k.update(&AgentId::from(root_id(&k)), patch);
    let out = k.start(false).await;
    assert_eq!(out["started"], json!(true), "{out}");
    k
}

fn flusher_id(k: &Arc<Kernel>) -> i64 {
    k.tasks
        .rows()
        .into_iter()
        .find(|t| t["verb"] == json!("flush_executions"))
        .expect("the flusher is listed")["id"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn cancel_refuses_the_flusher_and_it_keeps_flushing() {
    let k = started().await;
    let fid = flusher_id(&k);
    let out = call(&k, "kernel", json!({"type": "cancel", "task_id": fid})).await;
    assert_eq!(out.status.as_str(), "errored");
    assert_eq!(
        out.result,
        json!({
            "error": format!("cancel: task {fid} (flush_executions) is a kernel service task — not cancellable"),
            "reason": "bad_arg",
            "hint": "it runs for the life of the kernel and stays listed in ps; kernel_stop ends it",
            "arg": "task_id",
        })
    );
    let ps = call(&k, "kernel", json!({"type": "ps"})).await.result;
    assert!(
        ps["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == json!(fid)),
        "still listed"
    );
    // and still flushing: a new row leaves RAM
    let pid = call(&k, "kernel", json!({"type": "list_agents"}))
        .await
        .pid
        .unwrap()
        .get();
    for _ in 0..100 {
        if !k.rows.contains(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!k.rows.contains(pid), "the flusher kept writing");
}

type Seen = Arc<Mutex<Vec<Value>>>;

fn tap(k: &Arc<Kernel>, seen: &Seen, woken: &Arc<AtomicBool>) -> fantastic_kernel::SubscriberToken {
    let (seen, woken) = (Arc::clone(seen), Arc::clone(woken));
    k.add_state_subscriber(Arc::new(move |v: &Value, _| {
        seen.lock().unwrap().push(v.clone());
        woken.store(true, Ordering::SeqCst); // a long-poll `events` waiter
    }))
}

#[tokio::test]
async fn the_ledger_flush_publishes_nothing_on_the_state_stream() {
    let k = started().await;
    k.flush_executions().await; // start from an empty RAM
    let seen: Seen = Arc::default();
    let woken = Arc::new(AtomicBool::new(false));
    let token = tap(&k, &seen, &woken);
    call(&k, "kernel", json!({"type": "list_agents"})).await; // rows to flush
    seen.lock().unwrap().clear();
    woken.store(false, Ordering::SeqCst);
    let out = k.flush_executions().await;
    assert!(out["flushed"].as_u64().unwrap() > 0, "{out}");
    assert_eq!(*seen.lock().unwrap(), Vec::<Value>::new());
    assert!(!woken.load(Ordering::SeqCst), "a flush wakes no waiter");
    // the flush is recorded: its own flush row is in the store
    let dump = call(&k, "memory", json!({"type": "dump"})).await.result;
    assert!(dump["tables"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["row"]["kind"] == json!("flush") && r["row"]["id"] == out["flush"]));
    // traffic outside the flush still publishes
    call(&k, "kernel", json!({"type": "list_agents"})).await;
    assert!(
        !seen.lock().unwrap().is_empty(),
        "the stream is not muted, only the flush"
    );
    k.remove_state_subscriber(token);
}

#[tokio::test]
async fn an_id_block_reserved_inside_the_flush_is_quiet() {
    let k = started().await;
    k.flush_executions().await;
    let seen: Seen = Arc::default();
    let woken = Arc::new(AtomicBool::new(false));
    let token = tap(&k, &seen, &woken);
    call(&k, "kernel", json!({"type": "list_agents"})).await;
    // the flush mints its flush row id at the last id of the block; the call it
    // sends crosses the bound inside the silent flush
    while k.rows.next_id() < k.rows.id_reserved() - 1 {
        k.mint_id();
    }
    seen.lock().unwrap().clear();
    woken.store(false, Ordering::SeqCst);
    let before = k.rows.id_reserved();
    k.flush_executions().await;
    assert!(k.rows.id_reserved() > before);
    assert_eq!(*seen.lock().unwrap(), Vec::<Value>::new());
    assert!(
        !woken.load(Ordering::SeqCst),
        "a reservation wakes no waiter"
    );
    // the bound is on the root record (the loader's writer queues it directly)
    assert_eq!(
        k.root().unwrap().meta.read().unwrap()["next_id"],
        json!(k.rows.id_reserved())
    );
    k.remove_state_subscriber(token);
}
