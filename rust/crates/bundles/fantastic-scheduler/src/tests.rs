//! Unit tests for this bundle crate.
//!
//! All tests drive through `kernel.send`, with a `fantastic-file`
//! agent for persistence. That exercises the real wire path the
//! scheduler uses in production, not a stub.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;
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

async fn mk_kernel(tmp: &TempDir) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, SchedulerBundle);
    kernel
        .bundles
        .register("file.tools", fantastic_file::FileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let sch_id = scheduler_id_for(tmp);
    let file_id = format!("ff_{}", sch_id);
    // File agent rooted at the workdir.
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type":"create_agent",
                "handler_module":"file.tools",
                "id": file_id,
                "root": tmp.path().to_string_lossy(),
            }),
        )
        .await;
    // Scheduler bound to file_id.
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type":"create_agent",
                "handler_module":HANDLER_MODULE,
                "id": sch_id,
                "file_agent_id": file_id,
                "tick_sec": 0.1,
            }),
        )
        .await;
    (kernel, AgentId::from(sch_id.as_str()))
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
    let r = kernel.send(&sch.clone(), json!({"type": "reflect"})).await;
    assert_eq!(r["id"], sch.as_str());
    let expected_ff = format!("ff_{}", sch.as_str());
    assert_eq!(r["file_agent_id"], expected_ff);
    assert_eq!(r["paused"], false);
    // `create_agent` auto-fires boot (Python parity — see
    // lifecycle::create_from_payload). Scheduler.boot with
    // file_agent_id + tick_sec set starts the tick loop, so
    // running is true by reflect time.
    assert_eq!(r["running"], true);
}

#[tokio::test]
async fn boot_refuses_without_file_agent_id() {
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, SchedulerBundle);
    kernel
        .bundles
        .register("file.tools", fantastic_file::FileBundle);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    kernel
        .send(
            &AgentId::from("core"),
            json!({"type":"create_agent","handler_module":HANDLER_MODULE,"id":"sch2"}),
        )
        .await;
    let r = kernel
        .send(&AgentId::from("sch2"), json!({"type": "boot"}))
        .await;
    assert!(r["error"].as_str().unwrap().contains("file_agent_id"));
}

#[tokio::test]
async fn schedule_persists_and_lists() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(
            &sch.clone(),
            json!({
                "type": "schedule",
                "target": "core",
                "payload": {"type": "list_agents"},
                "interval_seconds": 5,
            }),
        )
        .await;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    assert!(sid.starts_with("sch_"));
    let listed = kernel.send(&sch.clone(), json!({"type": "list"})).await;
    let schedules = listed["schedules"].as_array().unwrap();
    assert!(schedules.iter().any(|s| s["id"] == sid));
    // schedules.json exists on disk via file agent.
    assert!(tmp
        .path()
        .join(format!(".fantastic/agents/{}/schedules.json", sch))
        .exists());
}

#[tokio::test]
async fn schedule_requires_target_and_payload_type() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(
            &sch.clone(),
            json!({"type": "schedule", "interval_seconds": 5}),
        )
        .await;
    assert!(r["error"].as_str().unwrap().contains("target"));
    let r2 = kernel
        .send(&sch.clone(), json!({"type": "schedule", "target": "core"}))
        .await;
    assert!(r2["error"].as_str().unwrap().contains("payload.type"));
}

#[tokio::test]
async fn unschedule_removes() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(
            &sch.clone(),
            json!({
                "type": "schedule",
                "target": "core",
                "payload": {"type": "list_agents"},
                "interval_seconds": 5,
            }),
        )
        .await;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    let removed = kernel
        .send(
            &sch.clone(),
            json!({"type": "unschedule", "schedule_id": sid}),
        )
        .await;
    assert_eq!(removed["removed"], true);
    let listed = kernel.send(&sch.clone(), json!({"type": "list"})).await;
    assert!(listed["schedules"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn pause_resume_flips_meta() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    kernel.send(&sch.clone(), json!({"type": "pause"})).await;
    let r = kernel.send(&sch.clone(), json!({"type": "reflect"})).await;
    assert_eq!(r["paused"], true);
    kernel.send(&sch.clone(), json!({"type": "resume"})).await;
    let r2 = kernel.send(&sch.clone(), json!({"type": "reflect"})).await;
    assert_eq!(r2["paused"], false);
}

#[tokio::test]
async fn tick_now_fires_immediately_and_appends_history() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    let r = kernel
        .send(
            &sch.clone(),
            json!({
                "type": "schedule",
                "target": "core",
                "payload": {"type": "list_agents"},
                "interval_seconds": 999_999,
            }),
        )
        .await;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    let fired = kernel
        .send(
            &sch.clone(),
            json!({"type": "tick_now", "schedule_id": sid}),
        )
        .await;
    assert_eq!(fired["fired"], true);
    let hist = kernel
        .send(&sch.clone(), json!({"type": "history", "limit": 10}))
        .await;
    let entries = hist["history"].as_array().unwrap();
    assert!(!entries.is_empty());
    assert_eq!(entries[0]["target"], "core");
}

#[tokio::test]
async fn tick_loop_fires_due_schedule() {
    let tmp = TempDir::new().unwrap();
    let (kernel, sch) = mk_kernel(&tmp).await;
    // Schedule with a 0-interval (loop will fire on first tick).
    let r = kernel
        .send(
            &sch.clone(),
            json!({
                "type": "schedule",
                "target": "core",
                "payload": {"type": "list_agents"},
                "interval_seconds": 1,
            }),
        )
        .await;
    let sid = r["schedule_id"].as_str().unwrap().to_string();
    let _ = kernel.send(&sch.clone(), json!({"type": "boot"})).await;
    // tick_sec is 0.1 on the test agent → wait ~1.5s for at least one fire.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let _ = kernel.send(&sch.clone(), json!({"type": "shutdown"})).await;
    let listed = kernel.send(&sch.clone(), json!({"type": "list"})).await;
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
