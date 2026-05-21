//! Unit tests for the local_runner bundle.
//!
//! These tests must not assume a real `fantastic` install — they
//! either drive verbs that don't spawn anything (`reflect`, `status`
//! on a never-started project), or substitute a placeholder long-
//! running process via `remote_cmd` set to a real binary on the
//! system (`/bin/sleep`) and synthesize a `lock.json` themselves to
//! exercise the status/stop paths.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;
use tempfile::TempDir;

fn agent_id_for(tmp: &TempDir) -> String {
    format!(
        "lr_{}",
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

async fn mk_kernel(tmp: &TempDir, project: &Path) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, LocalRunnerBundle);
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
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": id,
                "remote_path": project.to_string_lossy(),
            }),
        )
        .await;
    (kernel, AgentId::from(id.as_str()))
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("local_runner"));
}

#[tokio::test]
async fn reflect_reports_record_and_idle() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let (kernel, id) = mk_kernel(&tmp, &project).await;
    let r = kernel.send(&id, json!({"type": "reflect"})).await;
    assert_eq!(r["id"], id.as_str());
    assert_eq!(r["running"], false);
    assert_eq!(r["pid"], Value::Null);
    assert_eq!(r["port"], Value::Null);
    assert_eq!(r["remote_path"], project.to_string_lossy().as_ref());
    // Verbs surface.
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
async fn status_reports_running_when_lock_present() {
    // Spawn a placeholder long-running process and write its pid to a
    // synthetic `.fantastic/lock.json`. status should pick it up.
    let sleep_bin = match which::which("sleep") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("skipping status_reports_running_when_lock_present — no /bin/sleep");
            return;
        }
    };
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join(".fantastic/agents/web_test")).unwrap();
    // Synthesize a web agent record carrying a port.
    std::fs::write(
        project.join(".fantastic/agents/web_test/agent.json"),
        json!({
            "id": "web_test",
            "handler_module": "web.tools",
            "port": 18181,
        })
        .to_string(),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(&sleep_bin)
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pid = child.id().expect("sleep has pid");

    // Write lock.json with the live pid.
    std::fs::write(
        project.join(".fantastic/lock.json"),
        json!({"pid": pid}).to_string(),
    )
    .unwrap();

    let (kernel, id) = mk_kernel(&tmp, &project).await;
    let r = kernel.send(&id, json!({"type": "status"})).await;
    assert_eq!(r["running"], true, "status was {r}");
    assert_eq!(r["pid"], pid as i64, "status was {r}");
    assert_eq!(r["port"], 18181, "status was {r}");

    // get_webapp builds the URL.
    let g = kernel.send(&id, json!({"type": "get_webapp"})).await;
    assert_eq!(g["url"], "http://localhost:18181/");
    assert_eq!(g["default_width"], 800);
    assert_eq!(g["default_height"], 600);

    // Tear down: kill the placeholder + sweep.
    #[cfg(unix)]
    {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    let _ = child.wait().await;
}

#[tokio::test]
async fn stop_clears_stale_lock_when_pid_dead() {
    // Lock with a definitely-dead pid → stop returns success +
    // removes the lock.
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join(".fantastic")).unwrap();
    std::fs::write(
        project.join(".fantastic/lock.json"),
        // 1 is init on Unix — we can't kill it; but it IS alive. So
        // use a very high pid that's unlikely to be in use; pid_alive
        // returns false, falling into the "nothing to stop, sweep
        // stale" branch.
        json!({"pid": 0x7fff_fff0_i64}).to_string(),
    )
    .unwrap();

    let (kernel, id) = mk_kernel(&tmp, &project).await;
    let r = kernel.send(&id, json!({"type": "stop"})).await;
    assert_eq!(r["stopped"], true, "stop returned {r}");
    // Lock file is gone.
    assert!(
        !project.join(".fantastic/lock.json").exists(),
        "stale lock not swept",
    );
}

#[tokio::test]
async fn start_lifecycle_with_placeholder_binary() {
    // Use /bin/sleep as the "fantastic" binary. It won't write a
    // lock.json so start will eventually return the "lock.json never
    // appeared" error — but it WILL be spawned + reaped properly.
    let sleep_bin = match which::which("sleep") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("skipping start_lifecycle_with_placeholder_binary — no /bin/sleep");
            return;
        }
    };
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, LocalRunnerBundle);
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
    let id = agent_id_for(&tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": id,
                "remote_path": project.to_string_lossy(),
                // /bin/sleep — absolute path, bypasses `which`.
                "remote_cmd": sleep_bin.to_string_lossy(),
            }),
        )
        .await;
    let id = AgentId::from(id.as_str());

    // Spawn a separate task for `start` so we can stop it from the
    // outside — start's poll waits up to 30s for lock.json which
    // never appears (sleep doesn't write one).
    let start_kernel = Arc::clone(&kernel);
    let start_id = id.clone();
    let start_task =
        tokio::spawn(async move { start_kernel.send(&start_id, json!({"type": "start"})).await });

    // Give start a moment to spawn the child.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The placeholder is alive; record its pid via our cache.
    let runner = RUNNERS.get_or_init_for(&id);
    let spawned_pid = {
        let slot = runner.child.lock().await;
        slot.as_ref().and_then(|c| c.id())
    };
    assert!(
        spawned_pid.is_some(),
        "start should have spawned a child via RUNNERS",
    );
    let spawned_pid = spawned_pid.unwrap();
    assert!(pid_alive(spawned_pid as i32), "placeholder pid not alive");

    // stop will SIGTERM the placeholder via the cache fallback (no
    // lock.json was written).
    let stop_result = kernel.send(&id, json!({"type": "stop"})).await;
    assert_eq!(stop_result["stopped"], true, "stop reply: {stop_result}");

    // Reaping happened in stop; placeholder is gone.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !pid_alive(spawned_pid as i32),
        "placeholder pid still alive after stop",
    );

    // start_task should now resolve with the "lock never appeared" error.
    let r = tokio::time::timeout(Duration::from_secs(35), start_task)
        .await
        .expect("start didn't finish in time")
        .expect("start task panicked");
    // start returned its error (lock never appeared) — but we've
    // already torn down the child, so the test passes.
    assert!(
        r.get("error").is_some() || r.get("started").is_some(),
        "unexpected start reply: {r}",
    );
}

#[tokio::test]
async fn unknown_verb_errors() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let (kernel, id) = mk_kernel(&tmp, &project).await;
    let r = kernel.send(&id, json!({"type": "garbage"})).await;
    assert!(
        r["error"].as_str().unwrap_or("").contains("unknown type"),
        "{r}",
    );
}

#[test]
fn resolve_bin_ladder_remote_cmd_wins() {
    let mut meta = Map::new();
    meta.insert("remote_cmd".to_string(), json!("/abs/path/to/fantastic"));
    let p = resolve_fantastic_bin(&meta).unwrap();
    assert_eq!(p, PathBuf::from("/abs/path/to/fantastic"));
}

#[test]
fn resolve_bin_ladder_env_var() {
    struct EnvGuard {
        prior: Option<String>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: single-threaded test scope.
            unsafe {
                match &self.prior {
                    Some(v) => std::env::set_var("FANTASTIC_BIN", v),
                    None => std::env::remove_var("FANTASTIC_BIN"),
                }
            }
        }
    }
    let _guard = EnvGuard {
        prior: std::env::var("FANTASTIC_BIN").ok(),
    };
    // SAFETY: single-threaded test scope.
    unsafe {
        std::env::set_var("FANTASTIC_BIN", "/env/fantastic");
    }
    let p = resolve_fantastic_bin(&Map::new()).unwrap();
    assert_eq!(p, PathBuf::from("/env/fantastic"));
}
