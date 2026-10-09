//! exec_bridge MONITORS (owner, 2026-10-09) — python
//! `exec_bridge/tests/test_monitor.py`, case for case: a match is sent with its
//! header and nothing else; `every` throttles and `tail` caps; the arguments are
//! checked; `cancel` and `unmonitor` end a monitor while its job keeps running; a
//! monitor ends with its job, delivering what waited.

use super::*;
use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{Agent, Item};
use serde_json::Map;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Records every message it is sent.
#[derive(Default, Clone)]
struct Ear(Arc<Mutex<Vec<(Instant, String)>>>);

#[async_trait]
impl Bundle for Ear {
    fn name(&self) -> &str {
        "monitor_listener_probe"
    }
    async fn handle(
        &self,
        id: &AgentId,
        p: &Value,
        _k: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match p.get("type").and_then(Value::as_str) {
            Some("send") => {
                self.0.lock().unwrap().push((
                    Instant::now(),
                    p.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ));
                Ok(Some(json!({"ok": true})))
            }
            _ => Ok(Some(
                json!({"id": id.as_str(), "sentence": "records what it is sent"}),
            )),
        }
    }
}

impl Ear {
    fn texts(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.clone())
            .collect()
    }
    fn stamps(&self) -> Vec<Instant> {
        self.0.lock().unwrap().iter().map(|(t, _)| *t).collect()
    }
}

const PRINTER: &str = "import sys, time\n\
n, gap, fatal, hold = int(sys.argv[1]), float(sys.argv[2]), set(sys.argv[3].split(',')), float(sys.argv[4])\n\
for k in range(n):\n    print(('FATAL step %d' if str(k) in fatal else 'INFO step %d') % k, flush=True)\n    time.sleep(gap)\n\
time.sleep(hold)\n";

fn python() -> PathBuf {
    which::which("python3")
        .or_else(|_| which::which("python"))
        .expect("python on PATH")
}

async fn call(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    k.send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

/// Per-test agent ids: the bundle's tables are process-wide (ONE KERNEL PER
/// PROCESS), and the tests run in parallel.
fn ids(tmp: &TempDir) -> (String, String) {
    let tag = tmp
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .replace('.', "_");
    (format!("runner_{tag}"), format!("ear_{tag}"))
}

async fn setup(tmp: &TempDir) -> (Arc<Kernel>, Ear) {
    let (runner, ear_id) = ids(tmp);
    let ear = Ear::default();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ExecBridgeBundle);
    kernel
        .bundles
        .register("monitor_listener_probe.tools", ear.clone());
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
    let r = call(
        &kernel,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": "kernel",
        "id": ear_id, "handler_module": "monitor_listener_probe.tools",
        "callable_by": {runner.clone(): "*"}}),
    )
    .await;
    assert_eq!(r["id"], json!(ear_id), "{r}");
    let r = call(
        &kernel,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": "kernel",
        "id": runner, "handler_module": HANDLER_MODULE, "ingress_rule": "allow_all_ingress",
        "cwd": tmp.path().to_string_lossy(), "inherit_env": ["PATH"], "allowed": [".*"],
        "can_call": {ear_id.clone(): ["send"]}}),
    )
    .await;
    assert_eq!(r["id"], json!(runner), "{r}");
    (kernel, ear)
}

async fn printer(k: &Arc<Kernel>, rn: &str, n: u32, gap: f64, fatal: &str, hold: f64) -> i64 {
    let r = call(
        k,
        rn,
        json!({"type": "start", "command": python().to_string_lossy(),
        "args": ["-c", PRINTER, n.to_string(), gap.to_string(),
                 if fatal.is_empty() { "-" } else { fatal }, hold.to_string()]}),
    )
    .await;
    r["job_id"].as_i64().unwrap_or_else(|| panic!("{r}"))
}

async fn until(pred: impl Fn() -> bool, secs: f64, what: &str) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{what} never held within {secs}s");
}

async fn stop_all(k: &Arc<Kernel>, rn: &str) {
    let _ = call(k, rn, json!({"type": "stop"})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_match_is_sent_with_its_header_and_nothing_else() {
    let tmp = TempDir::new().unwrap();
    let (k, ear) = setup(&tmp).await;
    let (rn, en) = ids(&tmp);
    let jid = printer(&k, &rn, 6, 0.05, "2,4", 5.0).await;
    let r = call(
        &k,
        &rn,
        json!({"type": "monitor", "job_id": jid, "notify": en,
        "match": "FATAL", "label": "w fatal"}),
    )
    .await;
    let mid = r["monitor_id"].as_i64().unwrap_or_else(|| panic!("{r}"));
    assert_eq!(
        r,
        json!({"monitor_id": mid, "job_id": jid, "notify": en, "match": "FATAL",
        "every": null, "tail": 100, "label": "w fatal"})
    );
    until(
        || {
            ear.texts()
                .iter()
                .map(|t| t.matches("FATAL").count())
                .sum::<usize>()
                >= 2
        },
        6.0,
        "two FATALs",
    )
    .await;
    let joined = ear.texts().join("\n");
    assert!(
        joined.contains("FATAL step 2") && joined.contains("FATAL step 4"),
        "{joined}"
    );
    assert!(!joined.contains("INFO"), "only matching lines are sent");
    let first = ear.texts()[0].clone();
    assert!(
        first.starts_with(&format!(
            "[monitor {mid} · job {jid} · from the operator's call "
        )),
        "{first}"
    );
    assert!(
        first.lines().next().unwrap().contains("\"w fatal\""),
        "{first}"
    );
    stop_all(&k, &rn).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_throttles_and_tail_caps() {
    let tmp = TempDir::new().unwrap();
    let (k, ear) = setup(&tmp).await;
    let (rn, en) = ids(&tmp);
    let jid = printer(&k, &rn, 40, 0.025, "", 5.0).await;
    let r = call(
        &k,
        &rn,
        json!({"type": "monitor", "job_id": jid, "notify": en,
        "every": 0.4, "tail": 5}),
    )
    .await;
    assert!(r["monitor_id"].is_i64(), "{r}");
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let stamps = ear.stamps();
    assert!(
        (2..=5).contains(&stamps.len()),
        "{} deliveries for ~1 s at every=0.4",
        stamps.len()
    );
    for w in stamps.windows(2) {
        assert!(
            w[1].duration_since(w[0]).as_secs_f64() >= 0.35,
            "{stamps:?}"
        );
    }
    for t in ear.texts() {
        let lines = t
            .lines()
            .skip(1)
            .filter(|l| l.starts_with("INFO") || l.starts_with("FATAL"))
            .count();
        assert!(lines <= 5, "{t}");
    }
    assert!(
        ear.texts()
            .iter()
            .any(|t| t.contains("earlier lines dropped")),
        "tail cut something"
    );
    stop_all(&k, &rn).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_match_every_is_required_and_args_are_checked() {
    let tmp = TempDir::new().unwrap();
    let (k, _ear) = setup(&tmp).await;
    let (rn, en) = ids(&tmp);
    let jid = printer(&k, &rn, 1, 0.01, "", 3.0).await;
    let cases = [
        (json!({"notify": en}), "missing_arg", "every"),
        (
            json!({"notify": en, "match": "(", "every": 1}),
            "bad_arg",
            "match",
        ),
        (
            json!({"notify": en, "match": "x", "every": 0}),
            "bad_arg",
            "every",
        ),
        (
            json!({"notify": en, "match": "x", "tail": 0}),
            "bad_arg",
            "tail",
        ),
        (json!({"match": "x"}), "missing_arg", "notify"),
        (
            json!({"notify": "nobody", "match": "x"}),
            "not_found",
            "notify",
        ),
    ];
    for (extra, reason, arg) in cases {
        let mut p = json!({"type": "monitor", "job_id": jid});
        for (key, v) in extra.as_object().unwrap() {
            p[key] = v.clone();
        }
        let r = call(&k, &rn, p.clone()).await;
        assert!(r["reason"] == reason && r["arg"] == arg, "{p}: {r}");
    }
    let r = call(
        &k,
        &rn,
        json!({"type": "monitor", "job_id": 999_999, "notify": en, "match": "x"}),
    )
    .await;
    assert_eq!(r["reason"], "not_found", "{r}");
    stop_all(&k, &rn).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_and_unmonitor_end_a_monitor_and_the_job_keeps_running() {
    let tmp = TempDir::new().unwrap();
    let (k, ear) = setup(&tmp).await;
    let (rn, en) = ids(&tmp);
    let jid = printer(&k, &rn, 400, 0.02, "", 2.0).await;
    let a = call(
        &k,
        &rn,
        json!({"type": "monitor", "job_id": jid, "notify": en,
        "match": "INFO", "label": "a"}),
    )
    .await["monitor_id"]
        .as_i64()
        .unwrap();
    let b = call(
        &k,
        &rn,
        json!({"type": "monitor", "job_id": jid, "notify": en,
        "match": "INFO", "label": "b"}),
    )
    .await["monitor_id"]
        .as_i64()
        .unwrap();
    let listed = call(&k, &rn, json!({"type": "monitors"})).await;
    let ids: Vec<i64> = listed["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["monitor_id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{listed}");
    assert!(ids.contains(&a) && ids.contains(&b), "{listed}");
    let ps = call(&k, "kernel", json!({"type": "ps"})).await;
    let monitor_tasks: Vec<i64> = ps["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["verb"] == "monitor")
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert!(
        monitor_tasks.contains(&a) && monitor_tasks.contains(&b),
        "{ps}"
    );
    until(
        || {
            ear.texts().iter().any(|t| t.contains("\"a\""))
                && ear.texts().iter().any(|t| t.contains("\"b\""))
        },
        6.0,
        "both deliver",
    )
    .await;
    let out = call(&k, "kernel", json!({"type": "cancel", "task_id": a})).await;
    assert_eq!(out["cancelled"], true, "{out}");
    let out = call(&k, &rn, json!({"type": "unmonitor", "monitor_id": b})).await;
    assert_eq!(out, json!({"unmonitored": true, "monitor_id": b}));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let listed = call(&k, &rn, json!({"type": "monitors"})).await;
    assert_eq!(listed["monitors"], json!([]), "{listed}");
    let before = ear.texts().len();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        ear.texts().len(),
        before,
        "nothing more is sent after the cancel"
    );
    let st = call(&k, &rn, json!({"type": "status", "job_id": jid})).await;
    assert_eq!(
        st["jobs"][0]["status"], "running",
        "cancelling a monitor never touches its job"
    );
    stop_all(&k, &rn).await;
    let r = call(&k, &rn, json!({"type": "unmonitor", "monitor_id": a})).await;
    assert_eq!(r["reason"], "not_found", "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_monitor_ends_with_its_job_delivering_what_waited() {
    let tmp = TempDir::new().unwrap();
    let (k, ear) = setup(&tmp).await;
    let (rn, en) = ids(&tmp);
    let jid = printer(&k, &rn, 3, 0.01, "1", 0.3).await;
    let r = call(
        &k,
        &rn,
        json!({"type": "monitor", "job_id": jid, "notify": en, "every": 30}),
    )
    .await;
    assert!(r["monitor_id"].is_i64(), "{r}");
    until(
        || ear.texts().iter().any(|t| t.contains("job ended")),
        5.0,
        "the final delivery at job end",
    )
    .await;
    assert!(ear.texts().join("\n").contains("FATAL step 1"));
    until(|| true, 0.3, "").await;
    let listed = call(&k, &rn, json!({"type": "monitors"})).await;
    assert_eq!(listed["monitors"], json!([]), "{listed}");
}
