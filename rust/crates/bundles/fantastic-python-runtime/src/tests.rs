//! Unit tests for the python_runtime bundle.
//!
//! Tests requiring a real Python interpreter probe `which python3` /
//! `which python`. When neither is present (rare on dev machines,
//! common on minimal CI containers) the test prints "skipping" and
//! returns successfully — we don't want to fail CI on systems without
//! Python.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;
use std::path::PathBuf;
use tempfile::TempDir;

/// Find a real Python interpreter via the resolution ladder's PATH
/// branches. Returns `None` when neither python3 nor python is on PATH.
fn find_real_python() -> Option<PathBuf> {
    which::which("python3")
        .or_else(|_| which::which("python"))
        .ok()
}

/// Generate a unique-per-test agent id. The IN_FLIGHT static is
/// process-global; sharing one id across parallel tests would race.
fn agent_id_for(tmp: &TempDir) -> String {
    format!(
        "py_{}",
        tmp.path()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('.', "_")
    )
}

async fn mk_kernel(tmp: &TempDir) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, PythonRuntimeBundle);
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
    let pid = agent_id_for(tmp);
    kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": HANDLER_MODULE,
                "id": pid,
            }),
        )
        .await;
    (kernel, AgentId::from(pid.as_str()))
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("python_runtime"));
}

#[tokio::test]
async fn reflect_reports_resolved_interpreter() {
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;
    let r = kernel.send(&pid, json!({"type": "reflect"})).await;
    assert_eq!(r["id"], pid.as_str());
    assert_eq!(r["sentence"], "Python subprocess runner.");
    assert_eq!(r["in_flight"], 0);
    // `python` is either a string path (if PATH has python3/python or
    // FANTASTIC_PYTHON is set) or an {error: ...} object.
    assert!(r["python"].is_string() || r["python"].is_object());
    // Verbs surface.
    for v in ["reflect", "exec", "interrupt", "stop", "boot"] {
        assert!(r["verbs"][v].is_string(), "verb {v} missing from reflect");
    }
}

#[tokio::test]
async fn exec_captures_stdout() {
    let Some(_real) = find_real_python() else {
        eprintln!("skipping exec_captures_stdout — no Python on PATH");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;
    let r = kernel
        .send(
            &pid,
            json!({"type": "exec", "code": "print('hi')", "timeout": 10.0}),
        )
        .await;
    assert_eq!(r["timed_out"], false, "reply was {r}");
    assert_eq!(r["exit_code"], 0, "reply was {r}");
    assert!(
        r["stdout"].as_str().unwrap_or("").contains("hi"),
        "stdout missing 'hi': {r}",
    );
    assert_eq!(r["stderr"], "");
}

#[tokio::test]
async fn exec_timeout_fires() {
    let Some(_real) = find_real_python() else {
        eprintln!("skipping exec_timeout_fires — no Python on PATH");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;
    let t0 = std::time::Instant::now();
    let r = kernel
        .send(
            &pid,
            json!({
                "type": "exec",
                "code": "import time\ntime.sleep(5)",
                "timeout": 0.2,
            }),
        )
        .await;
    let elapsed = t0.elapsed();
    assert_eq!(r["timed_out"], true, "reply was {r}");
    assert_ne!(r["exit_code"], 0, "reply was {r}");
    assert!(elapsed.as_secs_f64() < 3.0, "timeout escaped ({elapsed:?})");
}

#[tokio::test]
async fn interrupt_sends_sigint() {
    let Some(_real) = find_real_python() else {
        eprintln!("skipping interrupt_sends_sigint — no Python on PATH");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;

    // Install SIGINT handler that prints CAUGHT then exits. Stored as
    // a raw string so rustfmt doesn't reflow whitespace into the
    // Python source (which is indentation-sensitive).
    let sigint_code = r#"import signal,time,sys
def h(s,f):
    print("CAUGHT"); sys.stdout.flush(); sys.exit(0)
signal.signal(signal.SIGINT, h)
time.sleep(5)
"#;
    let exec_kernel = Arc::clone(&kernel);
    let exec_pid = pid.clone();
    let task = tokio::spawn(async move {
        exec_kernel
            .send(
                &exec_pid,
                json!({
                    "type": "exec",
                    "code": sigint_code,
                    "timeout": 10.0,
                }),
            )
            .await
    });

    // Wait until the subprocess registers itself in IN_FLIGHT.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let rfl = kernel.send(&pid, json!({"type": "reflect"})).await;
        if rfl["in_flight"].as_u64().unwrap_or(0) >= 1 {
            break;
        }
    }
    // Give Python a moment to install the signal handler.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let n = kernel.send(&pid, json!({"type": "interrupt"})).await;
    assert!(
        n["interrupted"].as_u64().unwrap_or(0) >= 1,
        "interrupt returned {n}",
    );

    let r = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("exec didn't finish in time")
        .expect("task panicked");
    let stdout = r["stdout"].as_str().unwrap_or("");
    assert!(stdout.contains("CAUGHT"), "stdout missing CAUGHT: {r}");
}

#[tokio::test]
async fn stop_sigkills() {
    let Some(_real) = find_real_python() else {
        eprintln!("skipping stop_sigkills — no Python on PATH");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;

    let exec_kernel = Arc::clone(&kernel);
    let exec_pid = pid.clone();
    let task = tokio::spawn(async move {
        exec_kernel
            .send(
                &exec_pid,
                json!({
                    "type": "exec",
                    "code": "import time\ntime.sleep(30)",
                    "timeout": 60.0,
                }),
            )
            .await
    });

    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let rfl = kernel.send(&pid, json!({"type": "reflect"})).await;
        if rfl["in_flight"].as_u64().unwrap_or(0) >= 1 {
            break;
        }
    }

    let k = kernel.send(&pid, json!({"type": "stop"})).await;
    assert!(k["killed"].as_u64().unwrap_or(0) >= 1, "stop returned {k}",);

    let r = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("exec didn't finish in time")
        .expect("task panicked");
    // After SIGKILL, exit_code is non-zero (signal-killed processes
    // typically surface as -1 from `Command`'s wait_with_output on Unix
    // since the status has no `code()`).
    assert_ne!(r["exit_code"], 0, "reply was {r}");
}

#[test]
fn resolve_python_ladder() {
    // Clear FANTASTIC_PYTHON for the duration of this test so the env
    // branch doesn't shadow earlier branches. SAFETY: tests run in
    // separate processes per `cargo test`, so global env mutation is
    // fine — we restore on drop.
    struct EnvGuard {
        prior: Option<String>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Restore the prior value (or remove if it was unset).
            // SAFETY: single-threaded test scope.
            unsafe {
                match &self.prior {
                    Some(v) => std::env::set_var("FANTASTIC_PYTHON", v),
                    None => std::env::remove_var("FANTASTIC_PYTHON"),
                }
            }
        }
    }
    let _guard = EnvGuard {
        prior: std::env::var("FANTASTIC_PYTHON").ok(),
    };
    // SAFETY: single-threaded test scope.
    unsafe {
        std::env::remove_var("FANTASTIC_PYTHON");
    }

    // Branch 1: payload.python wins absolutely.
    let p = resolve_python(&Map::new(), &json!({"python": "/explicit/payload/python"})).unwrap();
    assert_eq!(p, PathBuf::from("/explicit/payload/python"));

    // Branch 2: payload.venv → <venv>/bin/python.
    let tmp = TempDir::new().unwrap();
    let venv = tmp.path().join("venv1");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    let py = venv.join("bin/python");
    std::fs::write(&py, "#!/bin/sh\nexit 0\n").unwrap();
    let p = resolve_python(&Map::new(), &json!({"venv": venv.to_string_lossy()})).unwrap();
    assert_eq!(p, py);

    // Branch 3: record.python.
    let mut meta = Map::new();
    meta.insert("python".to_string(), json!("/explicit/record/python"));
    let p = resolve_python(&meta, &json!({})).unwrap();
    assert_eq!(p, PathBuf::from("/explicit/record/python"));

    // Branch 4: record.venv (when no record.python).
    let venv2 = tmp.path().join("venv2");
    std::fs::create_dir_all(venv2.join("bin")).unwrap();
    let py2 = venv2.join("bin/python3");
    std::fs::write(&py2, "#!/bin/sh\nexit 0\n").unwrap();
    let mut meta = Map::new();
    meta.insert("venv".to_string(), json!(venv2.to_string_lossy()));
    let p = resolve_python(&meta, &json!({})).unwrap();
    assert_eq!(p, py2);

    // Branch 5: FANTASTIC_PYTHON env (when no record fields).
    // SAFETY: single-threaded test scope.
    unsafe {
        std::env::set_var("FANTASTIC_PYTHON", "/from/env/python");
    }
    let p = resolve_python(&Map::new(), &json!({})).unwrap();
    assert_eq!(p, PathBuf::from("/from/env/python"));
    // SAFETY: single-threaded test scope.
    unsafe {
        std::env::remove_var("FANTASTIC_PYTHON");
    }

    // Branches 6/7: which python3 / python — only assertable if a real
    // one's on PATH. We assert SOME path comes back when one exists,
    // and the error branch fires otherwise.
    let real = find_real_python();
    let p = resolve_python(&Map::new(), &json!({}));
    match (real, p) {
        (Some(_), Ok(found)) => {
            // The resolver returned the same interpreter `which` does.
            // We don't pin the exact path — `which` might return either
            // python3 or python; assert it's executable-shaped.
            assert!(found.is_absolute(), "expected absolute path: {found:?}");
        }
        (None, Err(e)) => {
            // Branch 8: error.
            assert!(e.contains("no Python interpreter resolved"), "{e}");
        }
        (Some(_), Err(e)) => panic!("Python on PATH but resolver errored: {e}"),
        (None, Ok(p)) => panic!("no Python on PATH but resolver returned {p:?}"),
    }
}

#[tokio::test]
async fn unknown_verb_errors() {
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;
    let r = kernel.send(&pid, json!({"type": "garbage"})).await;
    assert!(
        r["error"].as_str().unwrap_or("").contains("unknown type"),
        "{r}",
    );
}

#[tokio::test]
async fn exec_requires_code() {
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;
    let r = kernel.send(&pid, json!({"type": "exec"})).await;
    assert!(r["error"].as_str().unwrap_or("").contains("code"), "{r}");
    let r2 = kernel.send(&pid, json!({"type": "exec", "code": ""})).await;
    assert!(r2["error"].as_str().unwrap_or("").contains("code"), "{r2}");
}
