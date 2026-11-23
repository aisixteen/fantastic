//! #726: a call the previous process left in flight survives a restart as a DEAD
//! LETTER. Its `call` row reached the kernel's memory store and no `settle` row did,
//! so `kernel_start` on the next kernel settles it `cancelled` (the ledger says so,
//! `on_deadletter` is announced to its caller) and hands it once to its caller and
//! once to its target (`take_cut`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{BundleError, BundleRegistry, Handler, Reply};
use fantastic_kernel::executions::{CutCall, CUT_BY_RESTART};
use fantastic_kernel::test_support::{as_agent, register_root_loader, grant};
use fantastic_kernel::{AgentId, Item, Kernel, Outcome};
use fantastic_ram_store::RamStoreBundle;
use serde_json::{json, Map, Value};

struct Rec;

fantastic_kernel::stand_in_verbs!(rec_verbs {
    HangV "hang" {  },
    PingV "ping" {  },
});

#[async_trait]
impl Handler for Rec {
    type Call = rec_verbs::Call;
    fn name(&self) -> &str {
        "rec.tools"
    }
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        rec_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        rec_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }
    async fn handle(
        &self,
        _id: &AgentId,
        call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        if call.payload()["type"] == json!("hang") {
            // the call the process stops in: it never answers
            std::future::pending::<()>().await;
        }
        Ok(Some(json!({"ok": true})))
    }
}

fn registry() -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        RamStoreBundle::default(),
    );
    reg.register("rec.tools", Rec);
    reg
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value) -> Outcome {
    k.operator_send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

fn root_id(k: &Arc<Kernel>) -> String {
    k.root().unwrap().id.0.clone()
}

/// A first kernel on `dir`: `a` may call `b`, a started memory store.
async fn first(dir: &std::path::Path) -> Arc<Kernel> {
    let k = bootstrap(
        registry(),
        BootstrapOptions::one_shot(dir.join(".fantastic"), dir),
    )
    .expect("boot")
    .kernel;
    call(&k, &root_id(&k), json!({"type": "boot"})).await; // mints the guid
    for id in ["a", "b", "memory"] {
        let hm = if id == "memory" {
            "ram_store.tools"
        } else {
            "rec.tools"
        };
        let r = call(
            &k,
            "kernel",
            json!({"type": "create_agent", "parent_id": root_id(&k), "id": id, "handler_module": hm}),
        )
        .await
        .result;
        assert_eq!(r["id"], json!(id), "{r}");
    }
    grant(&k, "a", "b", json!({"call": ["*"]}));
    let mut patch = Map::new();
    patch.insert("memory_store_id".to_string(), json!("memory"));
    fantastic_kernel::test_support::update(&k, &AgentId::from(root_id(&k)), patch);
    assert_eq!(k.start(false).await["started"], json!(true));
    k
}

/// What the first kernel flushed, in a SECOND kernel on the same dir — not yet
/// started. The first one is dead from the dump on: nothing it does later reaches
/// the store.
async fn second(k: &Arc<Kernel>, dir: &std::path::Path) -> Arc<Kernel> {
    k.flush_executions().await;
    let tables = call(k, "memory", json!({"type": "dump"})).await.result["tables"].clone();
    for id in k.agent_ids() {
        let a = k.resolve_target(id.as_str()).unwrap();
        fantastic_kernel::persistence::write_record_at(&a.agent_file(), &a.record()).unwrap();
    }
    let k2 = bootstrap(
        registry(),
        BootstrapOptions::one_shot(dir.join(".fantastic"), dir),
    )
    .expect("reboot")
    .kernel;
    call(&k2, "memory", json!({"type": "inject", "tables": tables})).await;
    k2
}

async fn ledger(k: &Arc<Kernel>) -> Vec<Value> {
    call(k, "kernel", json!({"type": "ledger", "limit": 1000}))
        .await
        .result["executions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[tokio::test]
async fn a_call_cut_by_a_restart_is_a_dead_letter_told_once_to_its_caller_and_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = first(tmp.path()).await;
    // `a` calls `b`, and the call never answers; `a` also makes one that does
    let k1 = Arc::clone(&k);
    let _hung = tokio::spawn(async move {
        as_agent(AgentId::from("a"), call(&k1, "b", json!({"type": "hang"}))).await
    });
    let done = as_agent(AgentId::from("a"), call(&k, "b", json!({"type": "ping"}))).await;
    assert_eq!(done.result, json!({"ok": true}));
    let mut hung = None;
    for _ in 0..200 {
        hung = ledger(&k)
            .await
            .into_iter()
            .find(|e| e["status"] == json!("in_flight") && e["target"] == json!("b"))
            .and_then(|e| e["id"].as_i64());
        if hung.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let hung = hung.expect("the hung call is in flight");

    let k2 = second(&k, tmp.path()).await;
    let mut inbox = k2.ensure_inbox(&AgentId::from("a"));
    assert_eq!(k2.start(false).await["started"], json!(true));

    // THE LEDGER: the cut call is a dead letter, nothing is left in flight
    let entries = ledger(&k2).await;
    let cut = entries
        .iter()
        .find(|e| e["id"] == json!(hung))
        .unwrap_or_else(|| panic!("the cut call is in the ledger: {entries:?}"));
    assert_eq!(cut["status"], json!("cancelled"), "{cut}");
    assert_eq!(cut["outcome"]["error"], json!(CUT_BY_RESTART), "{cut}");
    // (the one in flight is this `ledger` read itself, on the root)
    assert!(
        entries
            .iter()
            .filter(|e| e["status"] == json!("in_flight"))
            .all(|e| e["target"] == json!(root_id(&k2))),
        "{entries:?}"
    );
    let ping = entries
        .iter()
        .filter(|e| e["target"] == json!("b") && e["id"] != json!(hung))
        .collect::<Vec<_>>();
    assert!(
        ping.iter().all(|e| e["status"] == json!("completed")),
        "a settled call is left as it was: {ping:?}"
    );

    // ANNOUNCED to its caller, like any dead letter
    let mut told = Vec::new();
    while let Some(ev) = inbox.try_recv() {
        told.push(ev.payload);
    }
    assert!(
        told.iter().any(|e| e["type"] == json!("on_deadletter")
            && e["id"] == json!(hung)
            && e["status"] == json!("cancelled")),
        "{told:?}"
    );

    // HANDED ONCE to the caller and once to the target
    let want = CutCall {
        id: hung,
        target: "b".to_string(),
        caller: Some("a".to_string()),
        verb: Some("hang".to_string()),
        task: None,
    };
    assert_eq!(k2.take_cut(&AgentId::from("a")), vec![want.clone()]);
    assert!(k2.take_cut(&AgentId::from("a")).is_empty(), "once");
    assert_eq!(k2.take_cut(&AgentId::from("b")), vec![want]);
    assert!(k2.take_cut(&AgentId::from("b")).is_empty(), "once");

    // the dead letter is retired like any
    let r = call(&k2, "kernel", json!({"type": "ack_deadletter", "id": hung}))
        .await
        .result;
    assert_eq!(r, json!({"acked": true, "id": hung}));

    // a second restart cuts nothing more: the settle row is on the store now
    let k3 = second(&k2, tmp.path()).await;
    assert_eq!(k3.start(false).await["started"], json!(true));
    assert!(k3.take_cut(&AgentId::from("a")).is_empty());
    assert!(k3.take_cut(&AgentId::from("b")).is_empty());
}

/// The task a call still in flight was made in stays in the store when the window
/// prunes around it, so a restart still knows the call was its caller's own work
/// (`take_cut`'s `task`) and not a dispatch's plumbing.
#[tokio::test]
async fn the_task_of_a_call_in_flight_outlives_the_window() {
    let tmp = tempfile::TempDir::new().unwrap();
    let k = first(tmp.path()).await;
    // `a`'s task makes a call that never answers
    let k1 = Arc::clone(&k);
    k.spawn_task(&AgentId::from("a"), "work", async move {
        as_agent(AgentId::from("a"), call(&k1, "b", json!({"type": "hang"}))).await;
    });
    let mut hung = None;
    for _ in 0..200 {
        hung = ledger(&k)
            .await
            .into_iter()
            .find(|e| e["status"] == json!("in_flight") && e["target"] == json!("b"))
            .and_then(|e| e["id"].as_i64());
        if hung.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let hung = hung.expect("the hung call is in flight");
    for _ in 0..20 {
        let done = as_agent(AgentId::from("a"), call(&k, "b", json!({"type": "ping"}))).await;
        assert_eq!(done.result, json!({"ok": true}));
    }
    // the window prunes everything settled older than the newest few: the hung call
    // is the oldest call left, and its task is older still
    k.flush_executions().await;
    k.set_ledger_cap(Some(5));
    assert!(k.prune_executions().await > 0, "the window pruned");

    let k2 = second(&k, tmp.path()).await;
    assert_eq!(k2.start(false).await["started"], json!(true));
    let cut = k2.take_cut(&AgentId::from("a"));
    assert_eq!(cut.len(), 1, "{cut:?}");
    assert_eq!(cut[0].id, hung);
    assert_eq!(cut[0].task.as_deref(), Some("work"), "{cut:?}");
}
