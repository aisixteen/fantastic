//! The kernel keeps NO conversation and NO durable tape. Its execution rows
//! (`call`, `settle`, `task`, `ack`) live in RAM until the in-kernel FLUSHER
//! writes them to the root's MEMORY STORE (`memory_store_id`) and drops them;
//! `ledger`, dead letters, `ack_deadletter` and named-call dedup read the store
//! back. `kernel_start` refuses to run without a store. Also here: the id
//! counter's block reservation and the graceful stop landing its last rows.
//!
//! The twin of python `tests/kernel/test_memory_store.py`, against the real
//! `ram_store` bundle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::{Bundle, BundleError, BundleRegistry, Reply};
use fantastic_kernel::test_support::register_root_loader;
use fantastic_kernel::{with_sender, AgentId, Item, Kernel, Outcome};
use fantastic_ram_store::RamStoreBundle;
use serde_json::{json, Map, Value};

// ── a probe bundle: advertises `shutdown`, can hold, fail, and count ───────────

#[derive(Default)]
struct ProbeState {
    shut: Mutex<Vec<String>>,
    slow_started: tokio::sync::Notify,
    counts: Mutex<HashMap<String, i64>>,
}

struct Probe(Arc<ProbeState>);

#[async_trait]
impl Bundle for Probe {
    fn name(&self) -> &str {
        "mem_probe.tools"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match payload["type"].as_str().unwrap_or("") {
            "boot" => Ok(None),
            "reflect" => Ok(Some(json!({
                "id": id.0, "sentence": "probe",
                "verbs": {"shutdown": "wraps up", "slow": "", "work": ""},
            }))),
            "shutdown" => {
                self.0.shut.lock().unwrap().push(id.0.clone());
                // the shutdown dispatch may still use its tools: a nested send
                // passes the drain
                let r = kernel
                    .send(vec![Item::new(
                        AgentId::from("kernel"),
                        json!({"type": "ps"}),
                    )])
                    .await
                    .remove(0);
                Ok(Some(json!({"wrapped": true, "tools_ok": r.is_completed()})))
            }
            "slow" => {
                self.0.slow_started.notify_one();
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(Some(json!({"finished": true})))
            }
            "work" => Ok(Some(json!({"ok": true}))),
            "count" => {
                let mut c = self.0.counts.lock().unwrap();
                let n = c.entry(id.0.clone()).or_insert(0);
                *n += 1;
                Ok(Some(json!({"n": *n})))
            }
            "fail" => Err("probe failed on purpose".into()),
            other => Ok(Some(
                json!({"error": format!("unknown '{other}'"), "reason": "unhandled"}),
            )),
        }
    }
}

fn registry(state: &Arc<ProbeState>) -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        RamStoreBundle::default(),
    );
    reg.register("mem_probe.tools", Probe(Arc::clone(state)));
    reg
}

fn seeded() -> (Arc<Kernel>, Arc<ProbeState>) {
    let state = Arc::new(ProbeState::default());
    let booted = bootstrap(registry(&state), BootstrapOptions::in_memory()).expect("boot");
    (booted.kernel, state)
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value) -> Outcome {
    k.send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

async fn call_named(k: &Arc<Kernel>, target: &str, payload: Value, id: &str) -> Outcome {
    k.send(vec![Item::new(AgentId::from(target), payload).with_id(id)])
        .await
        .remove(0)
}

fn root_id(k: &Arc<Kernel>) -> String {
    k.root().unwrap().id.0.clone()
}

async fn mk(k: &Arc<Kernel>, aid: &str, extra: Value) -> String {
    let mut p = json!({
        "type": "create_agent",
        "parent_id": root_id(k),
        "id": aid,
        "handler_module": "mem_probe.tools",
        "callable_by": {"$parent": "*", "rest": "*", "caller": "*"},
    });
    for (key, v) in extra.as_object().unwrap() {
        p[key] = v.clone();
    }
    let r = call(k, "kernel", p).await.result;
    assert_eq!(r["id"], json!(aid), "{r}");
    aid.to_string()
}

/// A ram_store `memory` under the root, named by the root; the kernel started.
async fn with_store(k: &Arc<Kernel>, operator: bool, start: bool) {
    let r = call(
        k,
        "kernel",
        json!({
            "type": "create_agent", "parent_id": root_id(k), "id": "memory",
            "handler_module": "ram_store.tools", "operator": operator, "admins": ["kernel"],
        }),
    )
    .await
    .result;
    assert_eq!(r["id"], json!("memory"), "{r}");
    let mut patch = Map::new();
    patch.insert("memory_store_id".to_string(), json!("memory"));
    k.update(&AgentId::from(root_id(k)), patch);
    if start {
        let out = k.start(false).await;
        assert_eq!(out["started"], json!(true), "{out}");
    }
}

fn guid(k: &Arc<Kernel>) -> String {
    k.root().unwrap().meta.read().unwrap()["guid"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn dump(k: &Arc<Kernel>) -> Value {
    call(k, "memory", json!({"type": "dump"})).await.result["tables"].clone()
}

async fn kernel_rows(k: &Arc<Kernel>) -> Vec<Value> {
    let owner = format!("{}|kernel", guid(k));
    dump(k).await["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["owner"] == json!(owner))
        .map(|r| r["row"].clone())
        .collect()
}

fn pid(o: &Outcome) -> i64 {
    o.pid.unwrap().get()
}

// ── kernel_start: no store, no start ───────────────────────────────────

#[tokio::test]
async fn kernel_start_fails_loud_without_a_store() {
    let (k, _) = seeded();
    let out = call(&k, "kernel", json!({"type": "kernel_start"}))
        .await
        .result;
    assert_eq!(out["reason"], json!("missing_arg"), "{out}");
    assert_eq!(out["arg"], json!("memory_store_id"));
    assert!(
        out["hint"].as_str().unwrap().contains("ram_store"),
        "the refusal says what to create"
    );
    assert!(!k.started());
}

#[tokio::test]
async fn kernel_start_refuses_a_store_that_does_not_serve_the_operator() {
    let (k, _) = seeded();
    with_store(&k, false, false).await;
    let out = k.start(false).await;
    assert_eq!(out["reason"], json!("unauthorized"), "{out}");
    assert!(out["hint"].as_str().unwrap().contains("operator"));
    assert!(!k.started());
}

#[tokio::test]
async fn kernel_start_boots_the_store_first_then_everyone() {
    let (k, _) = seeded();
    mk(&k, "probe", json!({})).await;
    with_store(&k, true, false).await;
    let out = call(&k, "kernel", json!({"type": "kernel_start"}))
        .await
        .result;
    assert_eq!(out["started"], json!(true), "{out}");
    assert_eq!(out["store"], json!("memory"));
    assert_eq!(out["booted"][0], json!(root_id(&k)));
    assert_eq!(out["booted"][1], json!("memory"));
    assert!(out["booted"].as_array().unwrap().contains(&json!("probe")));
    let again = call(&k, "kernel", json!({"type": "kernel_start"}))
        .await
        .result;
    assert_eq!(again["already"], json!(true));
}

// ── the flusher ────────────────────────────────────────────────────────

#[tokio::test]
async fn execution_rows_flush_into_the_store_and_leave_ram() {
    let (k, _) = seeded();
    mk(&k, "probe", json!({})).await;
    with_store(&k, true, true).await;
    let out = call(&k, "probe", json!({"type": "work"})).await;
    let id = pid(&out);
    k.flush_executions().await;
    let rows = kernel_rows(&k).await;
    let c = rows
        .iter()
        .find(|r| r["id"] == json!(id))
        .expect("the call row");
    assert_eq!(c["kind"], json!("call"));
    assert_eq!(c["caller"], Value::Null);
    let settle = rows
        .iter()
        .find(|r| r["kind"] == json!("settle") && r["parent"] == json!(id))
        .expect("the settle row");
    // UNNAMED: the settle keeps a digest, never the body (the growth fix, §3)
    assert_eq!(
        settle["outcome"],
        json!({"status": "completed", "bytes": "{\"ok\": true}".len()})
    );
    assert!(rows.iter().any(
        |r| r["kind"] == json!("flush") && r["covers"].as_array().unwrap().contains(&json!(id))
    ));
    assert!(!k.rows.contains(id), "flushed rows leave RAM");
    assert!(!k.ledger.has(id), "a settled, flushed execution too");
}

#[tokio::test]
async fn stored_rows_keep_none_and_reduce_bytes_to_a_length() {
    let (k, _) = seeded();
    mk(&k, "probe", json!({})).await;
    with_store(&k, true, true).await;
    let out = k
        .send(vec![Item::new(
            AgentId::from("probe"),
            json!({"type": "work", "none": null}),
        )
        .with_binary_key("bytes", vec![0, 1, 2])])
        .await
        .remove(0);
    k.flush_executions().await;
    let rows = kernel_rows(&k).await;
    let c = rows.iter().find(|r| r["id"] == json!(pid(&out))).unwrap();
    assert_eq!(c["payload"]["bytes"], json!({"len": 3}));
    assert!(c["payload"].as_object().unwrap().contains_key("none"));
    assert_eq!(c["payload"]["none"], Value::Null);
    let before = dump(&k).await;
    call(&k, "memory", json!({"type": "inject", "tables": before})).await;
    let after = dump(&k).await;
    // the flusher may land this test's own dump/inject calls in between — appended after
    let n = before["rows"].as_array().unwrap().len();
    assert_eq!(
        after["rows"].as_array().unwrap()[..n],
        before["rows"].as_array().unwrap()[..]
    );
    assert_eq!(after["fold_refs"], before["fold_refs"]);
    assert_eq!(after["grants"], before["grants"]);
}

#[tokio::test]
async fn a_settle_keeps_the_body_only_when_named_or_the_error_when_dead() {
    let (k, _) = seeded();
    mk(&k, "probe", json!({})).await;
    with_store(&k, true, true).await;
    let named = call_named(&k, "probe", json!({"type": "work"}), "n-1").await;
    let plain = call(&k, "probe", json!({"type": "work"})).await;
    let dead = call(&k, "probe", json!({"type": "fail"})).await;
    k.flush_executions().await;
    let rows = kernel_rows(&k).await;
    let kept: HashMap<i64, Value> = rows
        .iter()
        .filter(|r| r["kind"] == json!("settle"))
        .map(|r| (r["parent"].as_i64().unwrap(), r["outcome"].clone()))
        .collect();
    assert_eq!(
        kept[&pid(&named)],
        json!({"status": "completed", "result": {"ok": true}})
    );
    assert_eq!(
        kept[&pid(&plain)],
        json!({"status": "completed", "bytes": 12})
    );
    assert_eq!(kept[&pid(&dead)]["status"], json!("failed"));
    assert!(kept[&pid(&dead)]["error"]
        .as_str()
        .unwrap()
        .contains("probe failed on purpose"));
    assert!(kept[&pid(&dead)].get("result").is_none());
}

#[tokio::test]
async fn repeated_ledger_calls_grow_the_store_linearly() {
    // Measured before the fix: ~6x per `ledger` call (2 KB → 14.8 MB in five).
    let (k, _) = seeded();
    mk(&k, "probe", json!({})).await;
    with_store(&k, true, true).await;
    async fn size(k: &Arc<Kernel>) -> usize {
        k.flush_executions().await;
        serde_json::to_string(&dump(k).await).unwrap().len()
    }
    let mut sizes = vec![size(&k).await];
    for _ in 0..6 {
        call(&k, "kernel", json!({"type": "ledger"})).await;
        sizes.push(size(&k).await);
    }
    let steps: Vec<i64> = sizes
        .windows(2)
        .map(|w| w[1] as i64 - w[0] as i64)
        .collect();
    let max = *steps[1..].iter().max().unwrap();
    assert!(max < 2 * steps[1], "not linear: {sizes:?}");
}

#[tokio::test]
async fn the_flush_records_itself_and_never_loops() {
    let (k, _) = seeded();
    mk(&k, "probe", json!({})).await;
    with_store(&k, true, true).await;
    call(&k, "probe", json!({"type": "work"})).await;
    tokio::time::sleep(Duration::from_millis(600)).await; // the debounce, several times over
    let n1 = kernel_rows(&k).await.len();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let rows = kernel_rows(&k).await;
    // the dumps above are operator reads — calls like any other — so the count may
    // grow by THEIR rows; what must not appear is a flush that only carries flushes
    let by_id: HashMap<i64, &Value> = rows
        .iter()
        .map(|r| (r["id"].as_i64().unwrap(), r))
        .collect();
    for f in rows.iter().filter(|r| r["kind"] == json!("flush")) {
        let covered: Vec<&&Value> = f["covers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| by_id.get(&i.as_i64().unwrap()))
            .collect();
        let only_flushes = !covered.is_empty()
            && covered
                .iter()
                .all(|c| c["kind"] == json!("call") && c["payload"]["type"] == json!("append"));
        assert!(
            !only_flushes,
            "a flush that only re-records flushes — the loop is back: {f}"
        );
    }
    assert!(rows.len() >= n1);
}

#[tokio::test]
async fn ledger_is_a_projection_of_store_and_ram_scoped_to_the_caller() {
    let (k, state) = seeded();
    mk(&k, "probe", json!({})).await;
    mk(
        &k,
        "caller",
        json!({"can_call": {"probe": "*", root_id(&k): ["ledger", "ack_deadletter"]}}),
    )
    .await;
    with_store(&k, true, true).await;
    let kk = Arc::clone(&k);
    let mine = with_sender(AgentId::from("caller"), async move {
        call_named(&kk, "probe", json!({"type": "work"}), "mine-1").await
    })
    .await;
    let other = call(&k, "probe", json!({"type": "work"})).await;
    k.flush_executions().await;
    let kk = Arc::clone(&k);
    let live = tokio::spawn(async move { call(&kk, "probe", json!({"type": "slow"})).await });
    tokio::time::timeout(Duration::from_secs(2), state.slow_started.notified())
        .await
        .expect("slow started");
    let led = call(&k, "kernel", json!({"type": "ledger"})).await.result;
    let by: HashMap<i64, Value> = led["executions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["id"].as_i64().unwrap(), e.clone()))
        .collect();
    assert_eq!(
        by[&pid(&mine)]["status"],
        json!("completed"),
        "read back from the store"
    );
    assert_eq!(
        by[&pid(&mine)]["outcome"]["result"],
        json!({"ok": true}),
        "named: kept"
    );
    assert!(
        by[&pid(&other)]["outcome"].get("result").is_none(),
        "unnamed: a digest"
    );
    assert_eq!(by[&pid(&other)]["outcome"]["bytes"], json!(12));
    assert!(
        led["executions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["status"] == json!("in_flight")),
        "RAM overlay"
    );
    let kk = Arc::clone(&k);
    let scoped = with_sender(AgentId::from("caller"), async move {
        call(&kk, "kernel", json!({"type": "ledger"})).await.result
    })
    .await;
    let ids: Vec<i64> = scoped["executions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_i64().unwrap())
        .collect();
    assert!(
        ids.contains(&pid(&mine)) && !ids.contains(&pid(&other)),
        "{scoped}"
    );
    live.abort();
}

#[tokio::test]
async fn named_dedup_survives_the_flush() {
    let (k, state) = seeded();
    mk(&k, "probe", json!({})).await;
    with_store(&k, true, true).await;
    let first = call_named(&k, "probe", json!({"type": "count"}), "job-1").await;
    k.flush_executions().await;
    assert!(
        k.ledger.resolve_name("job-1").is_none(),
        "the name left RAM with its rows"
    );
    let again = call_named(&k, "probe", json!({"type": "count"}), "job-1").await;
    assert_eq!(again.result, first.result);
    assert_eq!(again.result, json!({"n": 1}));
    assert_eq!(
        state.counts.lock().unwrap()["probe"],
        1,
        "the step never ran twice"
    );
}

#[tokio::test]
async fn named_dedup_survives_a_restart_on_the_same_store() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state = Arc::new(ProbeState::default());
    let k = bootstrap(registry(&state), BootstrapOptions::one_shot(tmp.path()))
        .expect("boot")
        .kernel;
    call(&k, &root_id(&k), json!({"type": "boot"})).await; // mints the guid
    mk(&k, "probe", json!({})).await;
    with_store(&k, true, true).await;
    call_named(&k, "probe", json!({"type": "count"}), "nightly").await;
    k.flush_executions().await;
    let tables = dump(&k).await;
    // PERSIST THE TREE (python `_testkit.persist`): every record at its address.
    for id in k.agent_ids() {
        let a = k.agents.get(&id).map(|e| Arc::clone(&e)).unwrap();
        fantastic_kernel::persistence::write_record_at(&a.agent_file(), &a.record()).unwrap();
    }
    let next = k.rows.next_id();
    // a NEW kernel on the same dir: same guid, same counter bound, an empty RAM store
    let k2 = bootstrap(registry(&state), BootstrapOptions::one_shot(tmp.path()))
        .expect("reboot")
        .kernel;
    assert_eq!(guid(&k2), guid(&k));
    assert!(
        k2.rows.next_id() >= next,
        "ids never repeat across a restart"
    );
    call(&k2, "memory", json!({"type": "inject", "tables": tables})).await;
    let out = k2.start(false).await;
    assert_eq!(out["started"], json!(true), "{out}");
    let again = call_named(&k2, "probe", json!({"type": "count"}), "nightly").await;
    assert_eq!(again.result, json!({"n": 1}));
    assert_eq!(state.counts.lock().unwrap()["probe"], 1);
}

// ── dead letters ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_dead_letter_is_announced_listed_and_acked() {
    let (k, _) = seeded();
    mk(&k, "probe", json!({})).await;
    mk(
        &k,
        "caller",
        json!({"can_call": {"probe": "*", root_id(&k): ["ledger", "ack_deadletter"]}}),
    )
    .await;
    with_store(&k, true, true).await;
    let mut inbox = k.ensure_inbox(&AgentId::from("caller"));
    let as_caller = |p: Value, target: &'static str| {
        let kk = Arc::clone(&k);
        async move {
            with_sender(AgentId::from("caller"), async move {
                call(&kk, target, p).await
            })
            .await
        }
    };
    let out = as_caller(json!({"type": "fail"}), "probe").await;
    assert_eq!(out.status.as_str(), "failed");
    let ev = inbox
        .try_recv()
        .expect("on_deadletter reached the caller")
        .payload;
    assert_eq!(ev["type"], json!("on_deadletter"));
    assert_eq!(ev["id"], json!(pid(&out)), "{ev}");
    k.flush_executions().await;
    let led = as_caller(json!({"type": "ledger"}), "kernel").await.result;
    let entry = led["executions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == json!(pid(&out)))
        .unwrap()
        .clone();
    assert_eq!(entry["status"], json!("failed"));
    assert!(entry.get("acked").is_none());
    let acked = as_caller(json!({"type": "ack_deadletter", "id": pid(&out)}), "kernel")
        .await
        .result;
    assert_eq!(acked, json!({"acked": true, "id": pid(&out)}));
    let ok = as_caller(json!({"type": "work"}), "probe").await;
    let refused = as_caller(json!({"type": "ack_deadletter", "id": pid(&ok)}), "kernel")
        .await
        .result;
    assert_eq!(
        refused["reason"],
        json!("bad_arg"),
        "only a dead letter is acked"
    );
    k.flush_executions().await;
    let led = call(&k, "kernel", json!({"type": "ledger"})).await.result;
    let entry = led["executions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == json!(pid(&out)))
        .unwrap()
        .clone();
    assert_eq!(entry["acked"], json!(true));
    let rows = kernel_rows(&k).await;
    assert!(rows
        .iter()
        .any(|r| r["kind"] == json!("ack") && r["parent"] == json!(pid(&out))));
}

#[tokio::test]
async fn a_graceful_stop_lands_in_flight_work_as_cancelled_dead_letters() {
    let (k, state) = seeded();
    mk(&k, "probe", json!({})).await;
    with_store(&k, true, true).await;
    // shorten nothing: the quiet window is the kernel's; a slow task is cancelled
    let kk = Arc::clone(&k);
    let slow = tokio::spawn(async move { call(&kk, "probe", json!({"type": "slow"})).await });
    tokio::time::timeout(Duration::from_secs(2), state.slow_started.notified())
        .await
        .expect("slow started");
    let id = k
        .tasks
        .rows()
        .into_iter()
        .find(|t| t["verb"] == json!("slow"))
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    // The store is torn down by nothing (a ram_store keeps its tables through the
    // teardown walk), so the store after the stop holds what the stop flushed.
    let kk = Arc::clone(&k);
    let stop = tokio::spawn(async move { kk.shutdown().await });
    let settled = tokio::time::timeout(Duration::from_secs(20), slow)
        .await
        .expect("the stop settled it")
        .unwrap();
    assert_eq!(settled.status.as_str(), "cancelled");
    tokio::time::timeout(Duration::from_secs(20), stop)
        .await
        .expect("the stop finished")
        .unwrap();
    // Read as the kernel's own shutdown traffic: the stopped kernel is draining.
    let rows = fantastic_kernel::send::with_shutdown(kernel_rows(&k)).await;
    let settle = rows
        .iter()
        .find(|r| r["kind"] == json!("settle") && r["parent"] == json!(id))
        .expect("a dead letter, flushed");
    assert_eq!(settle["outcome"]["status"], json!("cancelled"));
}

// ── retention: the store-side window ───────────────────────────────────

#[tokio::test]
async fn the_kernels_store_is_pruned_to_its_window() {
    // `ledger_cap` is the store-side window: over it, the oldest settled
    // executions go — unnamed first — while a named one (its dedup contract) and
    // an unacked dead letter survive.
    let (k, _) = seeded();
    mk(&k, "probe", json!({})).await;
    with_store(&k, true, true).await;
    let named = call_named(&k, "probe", json!({"type": "work"}), "keep-me").await;
    let dead = call(&k, "probe", json!({"type": "fail"})).await;
    for _ in 0..40 {
        call(&k, "probe", json!({"type": "work"})).await;
    }
    k.set_ledger_cap(Some(20));
    k.flush_executions().await;
    let rows = kernel_rows(&k).await;
    let calls: Vec<&Value> = rows
        .iter()
        .filter(|r| r["kind"] == json!("call") && r.get("internal").is_none())
        .collect();
    let ids: Vec<i64> = calls.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert!(
        calls.len() <= 22,
        "the store holds {} calls against a window of 20",
        calls.len()
    );
    assert!(
        ids.contains(&pid(&named)),
        "a named execution is kept — it carries a dedup contract"
    );
    assert!(
        ids.contains(&pid(&dead)),
        "an unacked dead letter waits for a person"
    );
    let settles: Vec<i64> = rows
        .iter()
        .filter(|r| r["kind"] == json!("settle"))
        .map(|r| r["parent"].as_i64().unwrap())
        .collect();
    for id in &ids {
        assert!(
            settles.contains(id) || k.ledger.has(*id),
            "no settle row outlives its call and no call loses its settle"
        );
    }
}

/// A `ram_store` whose `read` answers an error while `fail_read` is set — the
/// store going away under the kernel's prune.
struct FlakyStore {
    inner: RamStoreBundle,
    fail_read: Arc<std::sync::atomic::AtomicBool>,
    refused: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Bundle for FlakyStore {
    fn name(&self) -> &str {
        "flaky_store"
    }
    async fn handle(
        &self,
        id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        use std::sync::atomic::Ordering;
        if payload["type"] == json!("read") && self.fail_read.load(Ordering::SeqCst) {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Ok(Some(json!({"error": "gone"})));
        }
        self.inner.handle(id, payload, kernel).await
    }
}

/// The twin of python `test_a_failed_store_read_in_the_prune_is_loud_and_the_
/// flusher_lives`: a store READ that fails inside the window prune prunes
/// nothing (python prints `  [kernel] prune of the kernel's store failed: …` and
/// returns 0); the flush LOOP survives it, so a later execution's rows still
/// leave RAM without anyone calling `flush_executions`.
#[tokio::test]
async fn a_failed_store_read_in_the_prune_prunes_nothing_and_the_flusher_lives() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let fail_read = Arc::new(AtomicBool::new(false));
    let refused = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(ProbeState::default());
    let mut reg = registry(&state);
    reg.register(
        "flaky_store.tools",
        FlakyStore {
            inner: RamStoreBundle::default(),
            fail_read: Arc::clone(&fail_read),
            refused: Arc::clone(&refused),
        },
    );
    let k = bootstrap(reg, BootstrapOptions::in_memory())
        .expect("boot")
        .kernel;
    mk(&k, "probe", json!({})).await;
    let r = call(
        &k,
        "kernel",
        json!({
            "type": "create_agent", "parent_id": root_id(&k), "id": "memory",
            "handler_module": "flaky_store.tools", "operator": true, "admins": ["kernel"],
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
    for _ in 0..5 {
        call(&k, "probe", json!({"type": "work"})).await;
    }
    k.set_ledger_cap(Some(1));
    fail_read.store(true, Ordering::SeqCst);
    assert_eq!(
        k.prune_executions().await,
        0,
        "a failed read prunes nothing"
    );
    // …and INSIDE the flusher task: a new row wakes it, its batch lands, the
    // window is over the cap, and the prune's read fails.
    let before = refused.load(Ordering::SeqCst);
    call(&k, "probe", json!({"type": "work"})).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while refused.load(Ordering::SeqCst) == before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the flusher never pruned"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fail_read.store(false, Ordering::SeqCst);
    let out = call(&k, "probe", json!({"type": "work"})).await;
    let id = pid(&out);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while k.rows.contains(id) || k.ledger.has(id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the flusher died: execution {id} never left RAM"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ── the id counter: block reservation in the root record ───────────────

#[tokio::test]
async fn the_id_counter_reserves_blocks_in_the_root_record() {
    let (k, _) = seeded();
    let root = k.root().unwrap();
    assert!(
        !root.meta.read().unwrap().contains_key("next_id"),
        "load never edits a record"
    );
    call(&k, &root_id(&k), json!({"type": "boot"})).await;
    with_store(&k, true, true).await;
    let n = k.mint_id();
    let bound = root.meta.read().unwrap()["next_id"]
        .as_i64()
        .expect("start wrote the reserved bound");
    assert!(bound > n);
    for _ in 0..fantastic_kernel::rows::ID_BLOCK {
        k.mint_id();
    }
    let later = root.meta.read().unwrap()["next_id"].as_i64().unwrap();
    assert!(later > bound, "a started kernel persists the next block");
    // save → load resumes past the reserved block
    let snap = k.save();
    let (k2, _) = seeded();
    k2.load(snap).unwrap();
    assert!(
        k2.mint_id() >= later,
        "a reboot resumes past the reserved block"
    );
}

#[tokio::test]
async fn kernel_stop_acks() {
    let (k, _) = seeded();
    let out = call(&k, "kernel", json!({"type": "kernel_stop"}))
        .await
        .result;
    assert_eq!(out, json!({"type": "kernel_stop", "ok": true}));
}
