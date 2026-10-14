//! Unit tests for this bundle crate.
//!
//! All tests drive through `kernel.send`, with a `fantastic-file`
//! agent as the loader's discovered store. That exercises the real
//! wire path the scheduler uses in production, not a stub.

use super::*;
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use serde_json::Map;
use std::path::PathBuf;
use tempfile::TempDir;

/// Per-test agent id. SCHEDULER_TASKS and SCHEDULE_CACHE are
/// process-global statics; sharing one agent id across parallel
/// tests would race. Each test derives a unique id from its tempdir
/// name (already unique by `mktemp` semantics).
fn scheduler_id_for(tmp: &TempDir) -> String {
    format!(
        "sch_{}",
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

fn now() -> f64 {
    now_secs()
}

/// What a probe target saw when it was hit: when, and the scheduler's
/// persisted `schedules.yaml` at that instant (when a capture path is set).
type Seen = (f64, Option<Value>);

/// Every probe hit so far, keyed by probe agent id.
#[derive(Clone, Default)]
struct Hits(Arc<Mutex<HashMap<String, Seen>>>);

impl Hits {
    fn get(&self, id: &str) -> Option<Seen> {
        self.0.lock().unwrap().get(id).cloned()
    }
    fn has(&self, id: &str) -> bool {
        self.0.lock().unwrap().contains_key(id)
    }
    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// A probe target: `hit` records when it ran (optionally blocking `block`
/// seconds) so the tests can measure the scheduler's fire discipline.
struct ProbeBundle {
    hits: Hits,
    capture: Option<PathBuf>,
}

#[async_trait]
impl Bundle for ProbeBundle {
    fn name(&self) -> &str {
        "sched_probe"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Ok(None),
            "reflect" => Ok(Some(
                json!({"id": agent_id.as_str(), "sentence": "scheduler probe", "verbs": {}}),
            )),
            "hit" => {
                let doc = self
                    .capture
                    .as_ref()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .and_then(|s| fantastic_kernel::yaml_io::load(&s).ok());
                self.hits
                    .0
                    .lock()
                    .unwrap()
                    .insert(agent_id.as_str().to_string(), (now(), doc));
                if let Some(b) = payload.get("block").and_then(Value::as_f64) {
                    tokio::time::sleep(Duration::from_secs_f64(b)).await;
                }
                Ok(Some(json!({"hit": agent_id.as_str()})))
            }
            other => Ok(Some(json!({"error": format!("unknown verb '{other}'")}))),
        }
    }
}

/// A kernel with the loader's store (a file agent at `.fantastic`) and a
/// scheduler that fires against `kernel_state` (its $parent — no grant needed).
async fn mk_kernel(tmp: &TempDir) -> (Arc<Kernel>, AgentId) {
    let (kernel, sch, _hits) = mk_probed(tmp, &[], None, true).await;
    (kernel, sch)
}

/// A kernel with (optionally) the store, `probes` probe targets and a
/// scheduler (tick 0.1s) granted to call them — both sides of the deny-all
/// gate opened. `capture` makes every hit read that file.
async fn mk_probed(
    tmp: &TempDir,
    probes: &[&str],
    capture: Option<PathBuf>,
    with_store: bool,
) -> (Arc<Kernel>, AgentId, Hits) {
    let hits = Hits::default();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, SchedulerBundle);
    // THE ROOT LOADER serves `persist_blob`/`load_blob` now (they left the
    // kernel-verb set), so a kernel whose root answers them must have it.
    kernel.bundles.register(
        fantastic_core::HANDLER_MODULE,
        fantastic_core::KernelStateBundle::default(),
    );
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle::default());
    kernel.bundles.register(
        "sched_probe.tools",
        ProbeBundle {
            hits: hits.clone(),
            capture,
        },
    );
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some(fantastic_core::HANDLER_MODULE.to_string()),
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let sch_id = scheduler_id_for(tmp);
    if with_store {
        let r = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({
                    "type":"create_agent",
                    "parent_id": "kernel",
                    "handler_module":"file_bridge.tools", "allow": ["."],
                    "id": format!("ff_{sch_id}"),
                    "root": tmp.path().join(".fantastic").to_string_lossy(),
                    "ingress_rule": "allow_all_ingress",
                }),
            )])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "store: {r}");
    }
    let mut can_call = Map::new();
    for p in probes {
        let r = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({
                    "type":"create_agent",
                    "parent_id": "kernel",
                    "handler_module":"sched_probe.tools",
                    "id": p,
                    "callable_by": { sch_id.clone(): "*" },
                }),
            )])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "probe {p}: {r}");
        can_call.insert((*p).to_string(), json!("*"));
    }
    // Nothing to wire: the scheduler persists THROUGH the loader.
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type":"create_agent",
                "parent_id": "kernel",
                "handler_module":HANDLER_MODULE,
                "id": sch_id,
                "can_call": can_call,
            }),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "scheduler: {r}");
    (kernel, AgentId::from(sch_id.as_str()), hits)
}

async fn schedule(
    kernel: &Arc<Kernel>,
    sch: &AgentId,
    target: &str,
    interval: u64,
    extra: Value,
) -> Value {
    let mut payload = json!({
        "type": "schedule",
        "target": target,
        "payload": {"type": "hit"},
        "interval_seconds": interval,
    });
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            if k == "hit" {
                payload["payload"]
                    .as_object_mut()
                    .unwrap()
                    .extend(v.as_object().cloned().unwrap_or_default());
            } else {
                payload[k] = v.clone();
            }
        }
    }
    kernel
        .send(vec![Item::new(sch.clone(), payload)])
        .await
        .remove(0)
        .result
}

async fn wait_for(mut pred: impl FnMut() -> bool, secs: f64) -> bool {
    let deadline = now() + secs;
    while now() < deadline {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    pred()
}

/// Stop a scheduler's tick loop the way an operator would.
///
/// `shutdown` is NOT a verb — python has no such key and it answers
/// `scheduler: unknown type 'shutdown'`. The tick loop is a kernel TASK, so the way
/// to stop one is to cancel that task (or delete the agent, which does the same
/// through `on_delete`).
async fn stop_tick(kernel: &Arc<Kernel>, sch: &AgentId) {
    let ids: Vec<fantastic_kernel::TaskId> = kernel
        .tasks
        .rows()
        .iter()
        .filter(|r| r["verb"] == "tick_loop" && r["agent"] == sch.as_str())
        .filter_map(|r| r["id"].as_i64().map(fantastic_kernel::TaskId))
        .collect();
    for id in ids {
        kernel.cancel_task(&id).await;
    }
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("scheduler"));
}

#[tokio::test]
async fn reflect_reports_state() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], sch.as_str());
    assert_eq!(r["paused"], false);
    // `create_agent` auto-fires boot (Python parity — see
    // lifecycle::create_from_payload), so the tick loop is running by
    // reflect time.
    assert_eq!(r["running"], true);
}

#[tokio::test]
async fn boot_succeeds_without_store_schedule_failfasts() {
    // Boot never needs a store (python): the tick loop starts; the first WRITE
    // surfaces the loader's error, and nothing lingers in the cache.
    let tmp = TempDir::new().unwrap();
    let (kernel, sch, _hits) = mk_probed(&tmp, &[], None, false).await;
    let r = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "boot"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["running"], true, "{r}");
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({
                "type": "schedule",
                "target": "kernel_state",
                "payload": {"type": "list_agents"},
                "interval_seconds": 5,
            }),
        )])
        .await
        .remove(0)
        .result;
    let err = r["error"].as_str().unwrap_or_default();
    assert!(err.starts_with("scheduler.schedule:"), "{r}");
    assert!(err.contains("no store"), "{r}");
    let listed = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "list"}))])
        .await
        .remove(0)
        .result;
    assert!(listed["schedules"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn schedule_persists_and_lists() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({
                "type": "schedule",
                "target": "kernel_state",
                "payload": {"type": "list_agents"},
                "interval_seconds": 5,
            }),
        )])
        .await
        .remove(0)
        .result;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    assert!(sid.starts_with("sch_"));
    let listed = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "list"}))])
        .await
        .remove(0)
        .result;
    let schedules = listed["schedules"].as_array().unwrap();
    assert!(schedules.iter().any(|s| s["id"] == sid));
    // schedules.yaml exists on disk via the loader's store.
    assert!(tmp
        .path()
        .join(format!(".fantastic/agents/{}/schedules.yaml", sch))
        .exists());
}

#[tokio::test]
async fn schedule_requires_target_and_payload_type() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "schedule", "interval_seconds": 5}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r["error"].as_str().unwrap().contains("target"));
    let r2 = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "schedule", "target": "kernel_state"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r2["error"].as_str().unwrap().contains("payload.type"));
}

#[tokio::test]
async fn schedule_rejects_a_bad_timeout() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    for bad_value in [json!(0), json!(-1), json!("5"), json!(true)] {
        let r = kernel
            .send(vec![Item::new(
                sch.clone(),
                json!({
                    "type": "schedule",
                    "target": "kernel_state",
                    "payload": {"type": "list_agents"},
                    "fire_timeout": bad_value,
                }),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], "bad_arg", "{r}");
        assert_eq!(r["arg"], "fire_timeout", "{r}");
    }
    let listed = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "list"}))])
        .await
        .remove(0)
        .result;
    assert!(listed["schedules"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn unschedule_removes() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({
                "type": "schedule",
                "target": "kernel_state",
                "payload": {"type": "list_agents"},
                "interval_seconds": 5,
            }),
        )])
        .await
        .remove(0)
        .result;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    let removed = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "unschedule", "schedule_id": sid}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(removed["removed"], true);
    let listed = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "list"}))])
        .await
        .remove(0)
        .result;
    assert!(listed["schedules"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn pause_resume_flips_meta() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    // python `kernel.update(id, paused=…)`: each flip publishes the `updated`
    // state event the root loader persists from.
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
    let sink = Arc::clone(&seen);
    let _tap = kernel.add_state_subscriber(Arc::new(move |ev, _| {
        sink.lock().unwrap().push(ev.clone());
    }));
    let _ = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "pause"}))])
        .await
        .remove(0)
        .result;
    let r = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["paused"], true);
    let _ = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "resume"}))])
        .await
        .remove(0)
        .result;
    let r2 = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r2["paused"], false);
    let flips = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|e| {
            e["agent_id"] == json!(sch.0)
                && e["kind"] == "updated"
                && e["changed"] == json!(["paused"])
        })
        .count();
    assert_eq!(flips, 2, "pause and resume each publish `updated`");
}

/// `tick_now` answers the fire's OUTCOME: `{type:'tick'}` sent to an AI agent,
/// which has no such verb, is a refusal `{fired:true}` alone would hide. Python
/// `test_tick_now_reports_a_refused_fire`.
#[tokio::test]
async fn tick_now_reports_a_refused_fire() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({
                "type": "schedule",
                "target": "ghost",
                "payload": {"type": "tick"},
                "interval_seconds": 999_999,
            }),
        )])
        .await
        .remove(0)
        .result;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    let fired = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "tick_now", "schedule_id": sid}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(fired["fired"], true, "{fired}");
    let said = if fired["error"].is_null() {
        fired["result"]["error"].clone()
    } else {
        fired["error"].clone()
    };
    assert!(said.to_string().contains("ghost"), "{fired}");
}

#[tokio::test]
async fn tick_now_fires_immediately_and_appends_history() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({
                "type": "schedule",
                "target": "kernel_state",
                "payload": {"type": "list_agents"},
                "interval_seconds": 999_999,
            }),
        )])
        .await
        .remove(0)
        .result;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    let fired = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "tick_now", "schedule_id": sid}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(fired["fired"], true);
    assert!(
        fired.get("result").is_some() && fired.get("error").is_some(),
        "{fired}"
    );
    let hist = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "history", "limit": 10}),
        )])
        .await
        .remove(0)
        .result;
    let entries = hist["history"].as_array().unwrap();
    assert!(!entries.is_empty());
    assert_eq!(entries[0]["target"], "kernel_state");
}

#[tokio::test]
async fn tick_loop_fires_due_schedule() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    // Schedule with a 0-interval (loop will fire on first tick).
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({
                "type": "schedule",
                "target": "kernel_state",
                "payload": {"type": "list_agents"},
                "interval_seconds": 1,
            }),
        )])
        .await
        .remove(0)
        .result;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    let _ = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "boot"}))])
        .await
        .remove(0)
        .result;
    // the loop fires at the schedule's due time → wait ~1.5s for at least one fire.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    stop_tick(&kernel, &sch).await;
    let listed = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "list"}))])
        .await
        .remove(0)
        .result;
    let sched = listed["schedules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == sid)
        .unwrap()
        .clone();
    let run_count = sched["run_count"].as_u64().unwrap();
    assert!(
        run_count >= 1,
        "expected ≥1 fire after 1.5s, got run_count={run_count}",
    );
}

#[tokio::test]
async fn due_schedules_fire_as_one_concurrent_batch() {
    // Two due schedules; the FIRST one blocks 0.3s. Sequential firing would
    // start the second ≥0.3s after the first; one batch starts both together.
    let tmp = TempDir::new().unwrap();
    let (kernel, sch, hits) = mk_probed(&tmp, &["p_slow", "p_fast"], None, true).await;
    // BOTH DUE when the loop looks: it fires at each schedule's exact due time (no
    // tick to coalesce on), so the two are made due while the scheduler is paused,
    // and the resume wakes it to one batch.
    let _ = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "pause"}))])
        .await;
    let r1 = schedule(&kernel, &sch, "p_slow", 1, json!({"hit": {"block": 0.3}})).await;
    let r2 = schedule(&kernel, &sch, "p_fast", 1, json!({})).await;
    assert!(r1["schedule_id"].is_string(), "{r1}");
    assert!(r2["schedule_id"].is_string(), "{r2}");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let _ = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "resume"}))])
        .await;
    let both = wait_for(|| hits.has("p_slow") && hits.has("p_fast"), 3.0).await;
    stop_tick(&kernel, &sch).await;
    assert!(both, "both probes must be hit");
    let gap = hits.get("p_fast").unwrap().0 - hits.get("p_slow").unwrap().0;
    assert!(
        gap.abs() < 0.25,
        "not one batch: fast started {gap:.3}s after slow"
    );
    let listed = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "list"}))])
        .await
        .remove(0)
        .result;
    for s in listed["schedules"].as_array().unwrap() {
        assert!(s["run_count"].as_u64().unwrap() >= 1, "{s}");
    }
}

#[tokio::test]
async fn next_run_is_persisted_before_the_fire() {
    // The probe reads schedules.yaml the moment it is hit: the store must
    // already hold the bumped run_count/next_run (persist-BEFORE-fire).
    let tmp = TempDir::new().unwrap();
    let sch_id = scheduler_id_for(&tmp);
    let capture = tmp
        .path()
        .join(format!(".fantastic/agents/{sch_id}/schedules.yaml"));
    let (kernel, sch, hits) = mk_probed(&tmp, &["p_cap"], Some(capture), true).await;
    let r = schedule(&kernel, &sch, "p_cap", 1, json!({})).await;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    let _ = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "boot"}))])
        .await;
    let hit = wait_for(|| hits.has("p_cap"), 3.0).await;
    stop_tick(&kernel, &sch).await;
    assert!(hit, "probe must be hit");
    let (at, doc) = hits.get("p_cap").unwrap();
    let doc = doc.expect("store had no schedules.yaml at fire time");
    let rec = doc
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == sid)
        .cloned()
        .unwrap();
    assert_eq!(rec["run_count"], 1, "{rec}");
    assert!(
        rec["next_run"].as_f64().unwrap() > at - 0.5,
        "{rec} at {at}"
    );
}

#[tokio::test]
async fn a_schedule_timeout_bounds_its_fire() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch, _hits) = mk_probed(&tmp, &["p_hang"], None, true).await;
    let r = schedule(
        &kernel,
        &sch,
        "p_hang",
        60,
        json!({"fire_timeout": 0.1, "hit": {"block": 2.0}}),
    )
    .await;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    assert_eq!(r["schedule"]["fire_timeout"], 0.1, "{r}");
    let t0 = now();
    let fired = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "tick_now", "schedule_id": sid}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(fired["fired"], true, "{fired}");
    assert!(
        now() - t0 < 1.5,
        "fire was not bounded by the schedule timeout"
    );
    let h = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "history", "schedule_id": sid}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(h["count"], 1, "{h}");
    let err = h["history"][0]["error"].as_str().unwrap_or_default();
    assert!(err.contains("timed_out"), "{h}");
}

#[tokio::test]
async fn non_int_args_are_bad_arg_refusals() {
    // A present-but-wrong number is a TYPO, not a new default:
    // `interval_seconds: "60"` must never schedule hourly what the caller wrote
    // as per-minute.
    let tmp = TempDir::new().unwrap();
    let (kernel, sch, _hits) = mk_probed(&tmp, &["p_x"], None, true).await;
    for bad_value in [
        json!("60"),
        json!(1.5),
        json!(true),
        json!(null),
        json!(0),
        json!(-1),
        json!([60]),
    ] {
        let r = kernel
            .send(vec![Item::new(
                sch.clone(),
                json!({
                    "type": "schedule", "target": "p_x", "payload": {"type": "hit"},
                    "interval_seconds": bad_value.clone(),
                }),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], json!("bad_arg"), "{bad_value}: {r}");
        assert_eq!(r["arg"], json!("interval_seconds"), "{bad_value}: {r}");
    }
    // Nothing was scheduled by a refused call.
    let listed = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "list"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(listed["schedules"], json!([]), "{listed}");
    // ABSENT still takes the documented default (60s), and is NOT refused.
    let ok = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "schedule", "target": "p_x", "payload": {"type": "hit"}}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(ok["schedule"]["interval_seconds"], json!(60), "{ok}");

    for bad_value in [json!("100"), json!(2.0), json!(true), json!(null), json!(0)] {
        let h = kernel
            .send(vec![Item::new(
                sch.clone(),
                json!({"type": "history", "limit": bad_value.clone()}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(h["reason"], json!("bad_arg"), "{bad_value}: {h}");
        assert_eq!(h["arg"], json!("limit"), "{bad_value}: {h}");
    }
    // ABSENT `limit` is the default page, not a refusal.
    let h = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "history"}))])
        .await
        .remove(0)
        .result;
    assert!(
        h.get("history").is_some() && h.get("reason").is_none(),
        "{h}"
    );
}

#[tokio::test]
async fn schedule_refuses_a_non_object_payload() {
    // #136 — `payload: [1]` can never carry a `type`; say so as an ARG error.
    let tmp = TempDir::new().unwrap();
    let (kernel, sch, _hits) = mk_probed(&tmp, &["p_x"], None, true).await;
    for bad_value in [json!([1]), json!("hit"), json!(7)] {
        let r = kernel
            .send(vec![Item::new(
                sch.clone(),
                json!({"type": "schedule", "target": "p_x", "payload": bad_value.clone()}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], json!("bad_arg"), "{bad_value}: {r}");
        assert_eq!(r["arg"], json!("payload"), "{bad_value}: {r}");
    }
}

#[tokio::test]
async fn an_idle_scheduler_never_wakes_and_a_change_wakes_it() {
    // No idle wakeups: with nothing to wait for the loop
    // waits on a change; a schedule, or the record's own `paused` flipped by
    // update_agent, wakes it.
    let tmp = TempDir::new().unwrap();
    let (kernel, sch, hits) = mk_probed(&tmp, &["p_idle"], None, true).await;
    let _ = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "boot"}))])
        .await
        .remove(0)
        .result;
    // no schedule: nothing fires, and nothing is read on a timer
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "schedule", "target": "p_idle", "payload": {"type": "hit"}, "interval_seconds": 1}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("schedule_id").is_some(), "{r}");
    assert!(
        wait_for(|| hits.has("p_idle"), 3.0).await,
        "a schedule woke nothing"
    );
    let up = |paused: bool| {
        Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": sch.as_str(), "paused": paused}),
        )
    };
    let _ = kernel.send(vec![up(true)]).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    hits.clear();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!hits.has("p_idle"), "fired while paused");
    let _ = kernel.send(vec![up(false)]).await;
    assert!(
        wait_for(|| hits.has("p_idle"), 3.0).await,
        "an update_agent resume woke nothing"
    );
    stop_tick(&kernel, &sch).await;
}

// ─── python parity (plan §2 A5) ──────────────────────────────────

/// The vendored readme is python's, byte for byte (two lines + `\n`).
#[test]
fn scheduler_readme_is_python_verbatim() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4) // src/lib above crates/bundles/<crate>
        .expect("src/lib above the crate")
        .join("python/bundled_agents/scheduler/src/scheduler/readme.md");
    let Ok(src) = std::fs::read_to_string(&py) else {
        eprintln!("skipping: python tree not present at {}", py.display());
        return;
    };
    assert_eq!(src, README, "readme has drifted from {}", py.display());
}

/// The `schedule_fired` emit doc is python's, two em-dashes and all.
#[tokio::test]
async fn docs_emits_is_python_verbatim() {
    assert_eq!(
        SchedulerBundle.emits_doc().unwrap()["schedule_fired"],
        "{type:'schedule_fired', schedule_id, scheduler_id, target, payload, result, error, ts, duration_ms} — emitted on the scheduler's OWN inbox after every fire (never the target's — the payload send is the tick)"
    );
}

/// `verbs` is python's `VERBS` order, each entry the FIRST LINE of that
/// verb's docstring. `boot`'s is truncated mid-sentence; that is the python text.
#[tokio::test]
async fn verb_docs_are_python_verbatim() {
    let docs = SchedulerBundle.verb_docs();
    let verbs = docs.as_object().expect("verbs object");
    assert_eq!(
        verbs.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "reflect",
            "boot",
            "schedule",
            "unschedule",
            "list",
            "pause",
            "resume",
            "tick_now",
            "history"
        ]
    );
    assert_eq!(
        verbs["boot"],
        "Idempotent. Starts the tick loop; persistence is THROUGH the loader."
    );
    assert_eq!(
        verbs["unschedule"],
        "args: schedule_id:str (req). Returns {removed:bool, schedule_id}."
    );
    assert_eq!(
        verbs["list"],
        "No args. Returns {schedules: [<schedule record>, ...]} — every schedule."
    );
}

/// `shutdown` is not a verb: python's `VERBS` has no such key, so it is an
/// unknown type — never a `null` that reads as a successful stop.
#[tokio::test]
async fn shutdown_is_an_unknown_type() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "shutdown"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], "scheduler: unknown type 'shutdown'");
}

/// Refusals quote with python's `repr`.
#[tokio::test]
async fn refusal_text_uses_python_repr() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "tick_now", "schedule_id": "nope"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], "schedule 'nope' not found");
    // A truthy NON-string id is echoed through repr too.
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "tick_now", "schedule_id": 7}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], "schedule 7 not found");
}

/// `boot` is idempotent and says only `{running:true}`; a caller cannot
/// branch on whether it was the first.
#[tokio::test]
async fn boot_twice_replies_running_only() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    for _ in 0..2 {
        let r = kernel
            .send(vec![Item::new(sch.clone(), json!({"type": "boot"}))])
            .await
            .remove(0)
            .result;
        assert_eq!(r, json!({"running": true}), "{r}");
    }
    stop_tick(&kernel, &sch).await;
}

/// The validation ORDER is python's: payload shape → `interval_seconds` →
/// target → `payload.type` → `timeout`. A call with two faults is told about the
/// first one python names.
#[tokio::test]
async fn schedule_validation_order_is_pythons() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let call = |p: Value| {
        let kernel = Arc::clone(&kernel);
        let sch = sch.clone();
        async move { kernel.send(vec![Item::new(sch, p)]).await.remove(0).result }
    };
    // No target AND a bad interval ⇒ the INTERVAL is named (it is checked first).
    let r = call(json!({"type": "schedule", "interval_seconds": "60",
                        "payload": {"type": "ping"}}))
    .await;
    assert_eq!(r["arg"], "interval_seconds", "{r}");
    // A non-object payload beats the interval.
    let r = call(json!({"type": "schedule", "payload": [1], "interval_seconds": "60"})).await;
    assert_eq!(r["arg"], "payload", "{r}");
    // No target AND no payload.type ⇒ the TARGET is named.
    let r = call(json!({"type": "schedule", "payload": {}})).await;
    assert_eq!(r["error"], "schedule: target required", "{r}");
    // A truthy NON-STRING target is stored verbatim.
    let r = call(json!({"type": "schedule", "target": 7, "payload": {"type": "ping"}})).await;
    assert_eq!(r["schedule"]["target"], json!(7), "{r}");
}

/// `timeout` is stored AS SENT: an int `5` stays `5`, it does not become
/// `5.0` on its way through an `f64`.
#[tokio::test]
async fn an_int_timeout_stays_an_int() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "schedule", "target": "kernel_state",
                   "payload": {"type": "list_agents"}, "fire_timeout": 5}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["schedule"]["fire_timeout"], json!(5), "{r}");
    assert!(r["schedule"]["fire_timeout"].is_i64(), "{r}");
}

/// `schedule_id` is read under python TRUTHINESS, and the raw value is
/// echoed back.
#[tokio::test]
async fn empty_schedule_id_is_absent_like_python() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let call = |p: Value| {
        let kernel = Arc::clone(&kernel);
        let sch = sch.clone();
        async move { kernel.send(vec![Item::new(sch, p)]).await.remove(0).result }
    };
    for falsy in [json!(""), json!(null), json!(0)] {
        let r = call(json!({"type": "unschedule", "schedule_id": falsy})).await;
        assert_eq!(r["error"], "unschedule: schedule_id required", "{r}");
        let r = call(json!({"type": "tick_now", "schedule_id": falsy})).await;
        assert_eq!(r["error"], "tick_now: schedule_id required", "{r}");
        // A falsy id on pause/resume is the WHOLE-SCHEDULER form.
        let r = call(json!({"type": "pause", "schedule_id": falsy})).await;
        assert_eq!(r["scheduler_id"], sch.as_str(), "{r}");
    }
    // A truthy non-string id is echoed raw.
    let r = call(json!({"type": "unschedule", "schedule_id": 7})).await;
    assert_eq!(r["removed"], false, "{r}");
    assert_eq!(r["schedule_id"], json!(7), "{r}");
}

/// `paused` is python TRUTHINESS on both the agent meta and each schedule;
/// reflect still answers a bool.
#[tokio::test]
async fn paused_meta_uses_python_truthiness() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": sch.as_str(), "paused": "no"}),
        )])
        .await
        .remove(0)
        .result;
    // A non-empty string is TRUE in python — `paused: "no"` is a paused scheduler.
    let r = reflect_reply(&sch, &kernel);
    assert_eq!(r["paused"], true, "{r}");
    assert!(r["paused"].is_boolean(), "{r}");
}

/// The sidecar bytes are python's `yaml_io.dump`: a block YAML sequence,
/// indent 2, key order kept, unicode verbatim (`allow_unicode`).
#[tokio::test]
async fn schedules_sidecar_uses_python_yaml_layout() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "schedule", "target": "córe",
                   "payload": {"type": "list_agents"}}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r["schedule_id"].is_string(), "{r}");
    let body = std::fs::read_to_string(
        tmp.path()
            .join(format!(".fantastic/agents/{}/schedules.yaml", sch.as_str())),
    )
    .expect("schedules.yaml");
    assert!(body.starts_with("- id: "), "{body}");
    assert!(body.contains("\n  target: córe\n"), "{body}");
    assert!(
        body.contains("\n  payload:\n    type: list_agents\n"),
        "{body}"
    );
}

/// `delete_agent` evicts the process schedule cache (python's
/// `on_delete`): an agent recreated under the same id in the same
/// process starts empty instead of inheriting its predecessor's schedules.
#[tokio::test]
async fn recreated_scheduler_drops_the_process_cache() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "schedule", "target": "kernel_state", "payload": {"type": "list_agents"}}),
        )])
        .await
        .remove(0)
        .result;
    let sid = r["schedule_id"].clone();
    assert!(sid.is_string(), "{r}");
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "delete_agent", "id": sch.as_str()}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        SCHEDULE_CACHE.lock().get(&sch).map(Vec::len),
        None,
        "delete evicts the process cache"
    );
}

/// A non-`completed` fire reads `'<status>: ' + str(detail)`, and a detail
/// python never had reads `None`. `completed` keeps `error` null even when the
/// reply itself carries an `error` key: the STATUS is the verdict.
#[test]
fn cancelled_fire_error_reads_none() {
    assert_eq!(
        fire_outcome(Status::Cancelled, Value::Null),
        (Value::Null, json!("cancelled: None"))
    );
    assert_eq!(
        fire_outcome(Status::Cancelled, json!({})),
        (Value::Null, json!("cancelled: None"))
    );
    assert_eq!(
        fire_outcome(Status::TimedOut, json!({"error": "gone"})),
        (Value::Null, json!("timed_out: gone"))
    );
    // A non-dict result is `str()`'d whole.
    assert_eq!(
        fire_outcome(Status::Failed, json!([1, "x"])),
        (Value::Null, json!("failed: [1, 'x']"))
    );
    // `completed` carries the reply through and leaves `error` null.
    assert_eq!(
        fire_outcome(Status::Completed, json!({"error": "domain said no"})),
        (json!({"error": "domain said no"}), Value::Null)
    );
}

/// The RAM cache is updated BEFORE the persist, so a tick whose write failed
/// keeps its bumped `run_count`/`next_run` and does not re-fire on the next sweep.
///
/// Warming the cache only on success would make the opposite true: every sweep
/// would re-read the stale list, re-fire the same schedule and fail to persist
/// again, forever.
#[tokio::test]
async fn failed_tick_persist_does_not_refire_next_tick() {
    let tmp = TempDir::new().unwrap();
    // NO STORE: every `persist_blob` fails, which is the point.
    let (kernel, sch, hits) = mk_probed(&tmp, &["p_np"], None, false).await;
    SCHEDULE_CACHE.lock().insert(
        sch.clone(),
        vec![json!({
            "id": "sch_fixed",
            "target": "p_np",
            "payload": {"type": "hit"},
            "interval_seconds": 60,
            "created_at": now() - 120.0,
            "next_run": now() - 1.0,
            "run_count": 0,
            "paused": false,
        })],
    );
    let _ = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "boot"}))])
        .await
        .remove(0)
        .result;
    assert!(wait_for(|| hits.has("p_np"), 3.0).await, "never fired");
    // Let time go by: the schedule must not re-fire before its interval.
    tokio::time::sleep(Duration::from_millis(600)).await;
    stop_tick(&kernel, &sch).await;
    let listed = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "list"}))])
        .await
        .remove(0)
        .result;
    let s = listed["schedules"][0].clone();
    assert_eq!(
        s["run_count"], 1,
        "the failed persist re-fired the tick: {s}"
    );
    assert!(
        s["next_run"].as_f64().unwrap_or(0.0) > now(),
        "next_run was not kept: {s}"
    );
}

/// The other half — `tick_now` ROLLS THE CACHE BACK when its persist fails
/// (python decrements `run_count` by hand), while `pause` keeps its change.
#[tokio::test]
async fn a_failed_persist_rolls_back_tick_now_but_not_pause() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch, _hits) = mk_probed(&tmp, &["p_rb"], None, false).await;
    SCHEDULE_CACHE.lock().insert(
        sch.clone(),
        vec![json!({
            "id": "sch_rb",
            "target": "p_rb",
            "payload": {"type": "hit"},
            "interval_seconds": 60,
            "created_at": now(),
            "next_run": now() + 60.0,
            "run_count": 0,
            "paused": false,
        })],
    );
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "tick_now", "schedule_id": "sch_rb"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r["error"].is_string(), "the persist must fail: {r}");
    assert_eq!(
        SCHEDULE_CACHE.lock()[&sch][0]["run_count"],
        0,
        "tick_now must roll its bump back"
    );
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "pause", "schedule_id": "sch_rb"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r["error"].is_string(), "the persist must fail: {r}");
    assert_eq!(
        SCHEDULE_CACHE.lock()[&sch][0]["paused"],
        true,
        "pause keeps its change on a failed persist"
    );
}

/// The per-schedule pause/resume reply leads with the FLAG,
/// then the id (python's key order).
#[tokio::test]
async fn pause_reply_key_order_matches_python() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "pause", "schedule_id": "nope"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["paused", "schedule_id"]
    );
    let r = kernel
        .send(vec![Item::new(
            sch.clone(),
            json!({"type": "resume", "schedule_id": "nope"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["resumed", "schedule_id"]
    );
}

/// A failed tick persist and a failed history append each print python's
/// `logging.lastResort` line (`scheduler/tools.py:236-239,286-289`), not rust's
/// old `tracing` text. Captured through the injected writer the fix introduces.
#[tokio::test]
async fn a_failed_persist_prints_python_line() {
    let tmp = TempDir::new().unwrap();
    // NO STORE: every `persist_blob` fails, which is the point.
    let (kernel, sch, hits) = mk_probed(&tmp, &["p_log"], None, false).await;
    // Swap the stderr writer for a buffer. Parallel tests may write into it too,
    // so every assertion is scoped to THIS scheduler's id.
    *LOG_SINK.lock() = Some(Vec::new());
    SCHEDULE_CACHE.lock().insert(
        sch.clone(),
        vec![json!({
            "id": "sch_log",
            "target": "p_log",
            "payload": {"type": "hit"},
            "interval_seconds": 60,
            "created_at": now() - 120.0,
            "next_run": now() - 1.0,
            "run_count": 0,
            "paused": false,
        })],
    );
    let _ = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "boot"}))])
        .await
        .remove(0)
        .result;
    assert!(wait_for(|| hits.has("p_log"), 3.0).await, "never fired");
    // The history append runs after the fire — give it a sweep to land.
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop_tick(&kernel, &sch).await;
    let lines: Vec<String> = LOG_SINK
        .lock()
        .take()
        .unwrap_or_default()
        .into_iter()
        .filter(|l| l.contains(sch.as_str()))
        .collect();
    let persist = lines
        .iter()
        .find(|l| l.starts_with(&format!("scheduler {sch}: persist failed: ")))
        .unwrap_or_else(|| panic!("no persist line in {lines:?}"));
    assert!(persist.contains("no store"), "{persist}");
    let history = lines
        .iter()
        .find(|l| l.starts_with(&format!("scheduler {sch}: history persist failed: ")))
        .unwrap_or_else(|| panic!("no history line in {lines:?}"));
    assert!(history.contains("no store"), "{history}");
}

/// python `item["timeout"] = sch["fire_timeout"]`: a fire carries the timeout AS
/// STORED (an int stays an int), so the kernel's prose and call row print the
/// schedule's own number.
#[test]
fn a_fire_carries_the_stored_timeout_raw() {
    let item = fire_item(&json!({"target": "t", "payload": {"type": "x"}, "fire_timeout": 5}));
    assert_eq!(item.raw_timeout, Some(json!(5)));
    assert!(item.timeout.is_none());
    let item = fire_item(&json!({"target": "t", "payload": {"type": "x"}, "fire_timeout": null}));
    assert!(item.raw_timeout.is_none());
}

/// The scheduler's unknown type is a BARE `{error}`
/// over python's repr of the RAW value. Compared as text (key order is wire).
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(sch.clone(), json!({"type": "frob"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"scheduler: unknown type 'frob'","reason":"unhandled","verb":"frob"}"#
    );
    let r = SchedulerBundle
        .handle(&sch, &json!({}), &kernel)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"scheduler: unknown type None","reason":"unhandled","verb":null}"#
    );
}

/// A schedule id is python's
/// `f"sch_{secrets.token_hex(3)}"` — 6 LOWERCASE hex, random per schedule.
#[tokio::test]
async fn schedule_ids_are_sch_and_token_hex_3() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..8 {
        let r = kernel
            .send(vec![Item::new(
                sch.clone(),
                json!({"type": "schedule", "target": "kernel_state",
                       "payload": {"type": "list_agents"}, "interval_seconds": 3600}),
            )])
            .await
            .remove(0)
            .result;
        let sid = r["schedule_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"))
            .to_string();
        let tail = sid.strip_prefix("sch_").unwrap_or_else(|| panic!("{sid}"));
        assert_eq!(tail.len(), 6, "{sid}");
        assert!(
            tail.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{sid}"
        );
        seen.insert(sid);
    }
    assert_eq!(seen.len(), 8, "ids repeat: {seen:?}");
}

/// Per-agent state. Two schedulers of the same kind
/// share nothing (schedules, tick loop); `delete_agent` stops the deleted one's
/// tick loop and frees its slot, and the survivor keeps running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_schedulers_share_nothing_and_delete_frees_only_its_own() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let b = AgentId::from(format!("{}_b", a.as_str()).as_str());
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel",
                   "handler_module": HANDLER_MODULE, "id": b.as_str()}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r.get("error").is_none(), "{r}");
    let ask = |who: AgentId, payload: Value| {
        let kernel = Arc::clone(&kernel);
        async move {
            kernel
                .send(vec![Item::new(who, payload)])
                .await
                .remove(0)
                .result
        }
    };
    let r = ask(
        a.clone(),
        json!({"type": "schedule", "target": "kernel_state",
               "payload": {"type": "list_agents"}, "interval_seconds": 3600}),
    )
    .await;
    assert!(r["schedule_id"].is_string(), "{r}");
    assert_eq!(
        ask(a.clone(), json!({"type": "list"})).await["schedules"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        ask(b.clone(), json!({"type": "list"})).await["schedules"],
        json!([])
    );
    assert_eq!(
        ask(a.clone(), json!({"type": "reflect"})).await["running"],
        json!(true)
    );
    assert_eq!(
        ask(b.clone(), json!({"type": "reflect"})).await["running"],
        json!(true)
    );
    let loops = |who: &AgentId| {
        kernel
            .tasks
            .rows()
            .iter()
            .filter(|r| r["verb"] == "tick_loop" && r["agent"] == who.as_str())
            .count()
    };
    assert_eq!(loops(&a), 1);
    assert_eq!(loops(&b), 1);
    let gone = ask(
        AgentId::from("kernel_state"),
        json!({"type": "delete_agent", "id": a.as_str()}),
    )
    .await;
    assert!(gone.get("error").is_none(), "{gone}");
    assert_eq!(
        loops(&a),
        0,
        "delete stopped the deleted scheduler's tick loop"
    );
    assert_eq!(loops(&b), 1, "the survivor keeps ticking");
    assert_eq!(
        ask(b.clone(), json!({"type": "reflect"})).await["running"],
        json!(true)
    );
    assert!(
        SCHEDULE_CACHE.lock().get(&a).is_none(),
        "the deleted one's cache is gone"
    );
    stop_tick(&kernel, &b).await;
}
