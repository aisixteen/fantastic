//! Unit tests for ssh_runner.
//!
//! These tests do NOT assume a real SSH server. They either exercise
//! verbs that don't touch the network (reflect / get_webapp / unknown
//! verb), or drive `start` against a guaranteed-unreachable host
//! (TEST-NET-1) and assert a clean error reply within a bounded
//! deadline. Tests that need the `ssh` binary skip if it's absent.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;
use tempfile::TempDir;

fn agent_id_for(tmp: &TempDir) -> String {
    format!(
        "sr_{}",
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

async fn mk_kernel(tmp: &TempDir, extra: Value) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, SshRunnerBundle);
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
    let id = agent_id_for(tmp);
    let mut payload = json!({
        "type": "create_agent",
        "handler_module": HANDLER_MODULE,
        "id": id,
    });
    if let (Some(p), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            p.insert(k.clone(), v.clone());
        }
    }
    kernel.send(&AgentId::from("core"), payload).await;
    (kernel, AgentId::from(id.as_str()))
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("ssh_runner"));
}

#[tokio::test]
async fn reflect_shape_when_not_running() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(
        &tmp,
        json!({
            "host": "myhost",
            "remote_path": "/srv/proj",
            "remote_cmd": "/srv/.venv/bin/fantastic",
            "remote_port": 8888,
            "local_port": 18181,
            "entry_path": "canvas_id/",
        }),
    )
    .await;
    let r = kernel.send(&id, json!({"type": "reflect"})).await;
    assert_eq!(r["id"], id.as_str());
    assert_eq!(r["host"], "myhost");
    assert_eq!(r["remote_path"], "/srv/proj");
    assert_eq!(r["remote_port"], 8888);
    assert_eq!(r["local_port"], 18181);
    assert_eq!(r["entry_path"], "canvas_id/");
    assert_eq!(r["tunnel_alive"], false);
    assert!(r["tunnel_pid"].is_null());
    for v in [
        "reflect",
        "boot",
        "start",
        "stop",
        "restart",
        "status",
        "get_webapp",
    ] {
        assert!(r["verbs"][v].is_string(), "verb {v} missing from reflect");
    }
}

#[tokio::test]
async fn boot_is_noop() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(
        &tmp,
        json!({"host": "h", "remote_path": "/p", "remote_cmd": "fantastic_kernel", "remote_port": 8888, "local_port": 18181}),
    )
    .await;
    let r = kernel.send(&id, json!({"type": "boot"})).await;
    assert!(r.is_null(), "boot should be null reply, got {r}");
}

#[tokio::test]
async fn get_webapp_when_local_port_set() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(
        &tmp,
        json!({
            "host": "myhost",
            "local_port": 18181,
            "entry_path": "tw_abc/",
            "display_name": "Remote Project",
        }),
    )
    .await;
    let r = kernel.send(&id, json!({"type": "get_webapp"})).await;
    assert_eq!(r["url"], "http://localhost:18181/tw_abc/");
    assert_eq!(r["default_width"], 800);
    assert_eq!(r["default_height"], 600);
    assert_eq!(r["title"], "Remote Project");
}

#[tokio::test]
async fn get_webapp_requires_local_port() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, json!({"host": "myhost"})).await;
    let r = kernel.send(&id, json!({"type": "get_webapp"})).await;
    assert!(
        r["error"]
            .as_str()
            .unwrap_or("")
            .contains("local_port required"),
        "{r}",
    );
}

#[tokio::test]
async fn start_fails_cleanly_without_host() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, json!({})).await;
    let r = kernel.send(&id, json!({"type": "start"})).await;
    assert!(
        r["error"].as_str().unwrap_or("").contains("required"),
        "start without host should error, got {r}",
    );
}

#[tokio::test]
async fn stop_requires_host_and_remote_path() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, json!({})).await;
    let r = kernel.send(&id, json!({"type": "stop"})).await;
    assert!(
        r["error"]
            .as_str()
            .unwrap_or("")
            .contains("host + remote_path required"),
        "{r}",
    );
}

#[tokio::test]
async fn unknown_verb_errors() {
    let tmp = TempDir::new().unwrap();
    let (kernel, id) = mk_kernel(&tmp, json!({})).await;
    let r = kernel.send(&id, json!({"type": "garbage"})).await;
    assert!(
        r["error"].as_str().unwrap_or("").contains("unknown type"),
        "{r}",
    );
}

#[tokio::test]
async fn unreachable_host_start_fails_within_deadline() {
    // Skip gracefully if `ssh` isn't on PATH.
    if which::which("ssh").is_err() {
        eprintln!("skipping unreachable_host_start_fails_within_deadline — no `ssh` on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    // TEST-NET-1 (RFC 5737) — guaranteed unreachable. ssh -o BatchMode=yes
    // means no interactive auth prompt, and the OS-level routing failure
    // surfaces fast.
    let (kernel, id) = mk_kernel(
        &tmp,
        json!({
            "host": "192.0.2.1",
            "remote_path": "/srv/proj",
            "remote_cmd": "/srv/.venv/bin/fantastic",
            "remote_port": 8888,
            "local_port": 18182,
        }),
    )
    .await;
    let r = tokio::time::timeout(
        Duration::from_secs(30),
        kernel.send(&id, json!({"type": "start"})),
    )
    .await
    .expect("start should not hang on unreachable host");
    assert!(
        r.get("error").is_some(),
        "expected clean error from unreachable host, got {r}",
    );
}
