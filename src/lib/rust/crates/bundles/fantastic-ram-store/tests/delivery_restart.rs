//! A settle is delivered to its call's receiver AT LEAST ONCE across a stop: one
//! whose row reached the kernel's memory store before its delivery ran is
//! delivered once at the next `kernel_start`, and stamped, so the start after
//! that delivers nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{BundleError, BundleRegistry, Handler, Reply};
use fantastic_kernel::test_support::{as_agent, grant, register_root_loader};
use fantastic_kernel::{AgentId, Item, Kernel, Outcome};
use fantastic_ram_store::RamStoreBundle;
use serde_json::{json, Map, Value};

/// The `reply` payloads one kernel's receivers got.
#[derive(Default)]
struct Seen {
    replies: Mutex<Vec<Value>>,
    /// The first delivery never finishes: the process stops inside it.
    stall_first: AtomicBool,
}

struct Rec {
    seen: Arc<Seen>,
}

fantastic_kernel::stand_in_verbs!(rec_verbs {
    ReplyV "reply" { re: Option<i64>, target: Option<String>, verb: Option<String>, status: Option<String>, result: Option<serde_json::Value> },
    WorkV "work" {  },
    HangV "hang" {  },
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
        let p = call.payload();
        if p["type"] == json!("reply") {
            self.seen.replies.lock().unwrap().push(p);
            if self.seen.stall_first.swap(false, Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            return Ok(None);
        }
        if p["type"] == json!("hang") {
            // the call the process stops in: it never answers
            std::future::pending::<()>().await;
        }
        Ok(Some(json!({"ok": true})))
    }
}

fn registry(seen: &Arc<Seen>) -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(fantastic_ram_store::HANDLER_MODULE, RamStoreBundle::default());
    reg.register(
        "rec.tools",
        Rec {
            seen: Arc::clone(seen),
        },
    );
    reg
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value) -> Outcome {
    fantastic_kernel::test_support::send_settled(k, vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

fn root_id(k: &Arc<Kernel>) -> String {
    k.root().unwrap().id.0.clone()
}

async fn first(dir: &std::path::Path, seen: &Arc<Seen>) -> Arc<Kernel> {
    let k = bootstrap(registry(seen), BootstrapOptions::one_shot(dir.join(".fantastic"), dir))
        .expect("boot")
        .kernel;
    call(&k, &root_id(&k), json!({"type": "boot"})).await;
    for id in ["a", "b", "memory"] {
        let hm = if id == "memory" { "ram_store.tools" } else { "rec.tools" };
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

/// What `k` flushed, in a new kernel on the same dir (not yet started).
async fn next(k: &Arc<Kernel>, dir: &std::path::Path, seen: &Arc<Seen>) -> Arc<Kernel> {
    k.flush_executions().await;
    let tables = call(k, "memory", json!({"type": "dump"})).await.result["tables"].clone();
    for id in k.agent_ids() {
        let a = k.resolve_target(id.as_str()).unwrap();
        fantastic_kernel::persistence::write_record_at(&a.agent_file(), &a.record()).unwrap();
    }
    let k2 = bootstrap(registry(seen), BootstrapOptions::one_shot(dir.join(".fantastic"), dir))
        .expect("reboot")
        .kernel;
    call(&k2, "memory", json!({"type": "inject", "tables": tables})).await;
    k2
}

/// The kernel's stored rows of `kind` about execution `pid`, serialized.
async fn stored(k: &Arc<Kernel>, kind: &str, pid: i64) -> Vec<String> {
    call(k, "memory", json!({"type": "dump"})).await.result["tables"]["rows"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|e| e["row"].clone())
        .filter(|r| r["kind"] == json!(kind) && r["parent"] == json!(pid))
        .map(|r| serde_json::to_string(&r).unwrap())
        .collect()
}

#[tokio::test]
async fn an_undelivered_settle_is_delivered_after_a_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let seen1 = Arc::new(Seen::default());
    seen1.stall_first.store(true, Ordering::SeqCst);
    let k = first(tmp.path(), &seen1).await;
    // `a` calls `b`; the settle is owed to `a`'s `reply`, and that delivery never
    // finishes in this process
    let k1 = Arc::clone(&k);
    // a SEND (#795): it acks, and the settle is delivered to `a`'s `reply` verb
    let _stalled = tokio::spawn(async move {
        as_agent(
            AgentId::from("a"),
            fantastic_kernel::send::as_operator(k1.send(vec![Item::new(AgentId::from("b"), json!({"type": "work"}))])),
        )
        .await
    });
    for _ in 0..300 {
        if !seen1.replies.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = seen1.replies.lock().unwrap()[0]["re"]
        .as_i64()
        .expect("the delivery started");

    // the stop flushes the settle row without its `delivered` stamp
    let seen2 = Arc::new(Seen::default());
    let k2 = next(&k, tmp.path(), &seen2).await;
    let settle_before = stored(&k2, "settle", pid).await;
    assert_eq!(settle_before.len(), 1, "the settle row reached the store");
    assert!(stored(&k2, "delivered", pid).await.is_empty(), "no delivery recorded");
    assert_eq!(k2.start(false).await["started"], json!(true));
    let got = seen2.replies.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "delivered once at the start: {got:?}");
    assert_eq!(got[0]["re"], json!(pid));
    assert_eq!(got[0]["target"], json!("b"));
    assert_eq!(got[0]["verb"], json!("work"));
    assert_eq!(got[0]["status"], json!("completed"));

    // the next start delivers it no more
    let seen3 = Arc::new(Seen::default());
    let k3 = next(&k2, tmp.path(), &seen3).await;
    // APPEND-ONLY: the settle row is the bytes it was, and one `delivered` row
    // now names it
    assert_eq!(stored(&k3, "settle", pid).await, settle_before);
    assert_eq!(stored(&k3, "delivered", pid).await.len(), 1);
    assert_eq!(k3.start(false).await["started"], json!(true));
    assert!(seen3.replies.lock().unwrap().is_empty(), "{:?}", seen3.replies.lock().unwrap());
    // and the ledger still holds the call, settled
    let entries = call(&k3, "kernel", json!({"type": "ledger", "limit": 1000}))
        .await
        .result["executions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let e = entries.iter().find(|e| e["id"] == json!(pid)).expect("the call is in the ledger");
    assert_eq!(e["status"], json!("completed"), "{e}");
}

/// A call a restart CUT is a dead letter, and it is DELIVERED to its receiver once.
#[tokio::test]
async fn a_cut_call_delivers_its_dead_letter_to_its_receiver() {
    let tmp = tempfile::TempDir::new().unwrap();
    let seen1 = Arc::new(Seen::default());
    let k = first(tmp.path(), &seen1).await;
    let k1 = Arc::clone(&k);
    let _hung = tokio::spawn(async move {
        as_agent(AgentId::from("a"), call(&k1, "b", json!({"type": "hang"}))).await
    });
    let mut hung = None;
    for _ in 0..300 {
        hung = call(&k, "kernel", json!({"type": "ledger", "limit": 1000}))
            .await
            .result["executions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .find(|e| e["status"] == json!("in_flight") && e["target"] == json!("b"))
            .and_then(|e| e["id"].as_i64());
        if hung.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let hung = hung.expect("the hung call is in flight");
    let seen2 = Arc::new(Seen::default());
    let k2 = next(&k, tmp.path(), &seen2).await;
    assert_eq!(k2.start(false).await["started"], json!(true));
    let got = seen2.replies.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["re"], json!(hung));
    assert_eq!(got[0]["status"], json!("cancelled"));
    assert_eq!(
        got[0]["result"]["error"],
        json!(fantastic_kernel::executions::CUT_BY_RESTART)
    );
    // delivered once
    let seen3 = Arc::new(Seen::default());
    let k3 = next(&k2, tmp.path(), &seen3).await;
    assert_eq!(k3.start(false).await["started"], json!(true));
    assert!(seen3.replies.lock().unwrap().is_empty());
}
