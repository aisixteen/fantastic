//! py_agent + folder bundles — python `py_agent/tests/test_py_agent.py`, case for
//! case: real programs (python, speaking the canonical reference helper).

use super::*;
use fantastic_kernel::{Agent, Item};
use std::time::Duration;
use tempfile::TempDir;

/// The canonical reference helper a program imports (python's own file).
const HELPER: &str = include_str!(
    "../../../../../python/bundled_agents/io/py_agent/src/py_agent/fantastic_agent.py"
);

const ECHO: &str = r#"import os, sys, time
import fantastic_agent as fa

def echo(p):
    fa.emit({"type": "echoed", "text": p.get("text")})
    return {"echo": p.get("text"), "pid": os.getpid()}

def die(p):
    sys.stderr.write("dying on purpose\n")
    sys.stderr.flush()
    os._exit(3)

def chatty(p):
    sys.stdout.write("x" * 300000)
    sys.stdout.flush()
    return {"chatted": True}

def slow(p):
    time.sleep(30)
    return {"late": True}

fa.serve({"echo": echo, "die": die, "slow": slow, "chatty": chatty})
"#;

const VERBS: &str = "  echo: 'args: text:str. Returns {echo, pid}.'\n  die: 'No args. The program exits.'\n  slow: 'No args. Sleeps 30 s.'\n  chatty: 'No args. Writes 300 kB to stdout.'\n";

/// Write `bundles/<name>/` with the helper, `program` and a bundle.yaml.
fn bundle(tmp: &TempDir, name: &str, program: &str, verbs: &str) -> PathBuf {
    let folder = tmp.path().join(".fantastic").join("bundles").join(name);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("fantastic_agent.py"), HELPER).unwrap();
    std::fs::write(folder.join("agent.py"), program).unwrap();
    std::fs::write(
        folder.join("bundle.yaml"),
        format!(
            "base: py_agent\nsentence: The {name} agent.\ncommand: [python3, agent.py]\n\
             record: {{inherit_env: [PATH]}}\nverbs:\n{verbs}"
        ),
    )
    .unwrap();
    folder
}

fn kernel_for(tmp: &TempDir) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, PyAgentBundle);
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
    kernel.set_root(root);
    kernel
}

fn load(kernel: &Arc<Kernel>, tmp: &TempDir) {
    fantastic_core::folders::load_bundles(kernel, &tmp.path().join(".fantastic"));
}

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    kernel
        .send(vec![
            Item::new(AgentId::from(target), payload).with_timeout(20.0)
        ])
        .await
        .remove(0)
        .result
}

async fn create(kernel: &Arc<Kernel>, hm: &str, id: &str) -> Value {
    call(
        kernel,
        "kernel_state",
        json!({"type": "create_agent", "parent_id": "kernel_state", "handler_module": hm, "id": id}),
    )
    .await
}

async fn delete(kernel: &Arc<Kernel>, id: &str) {
    let _ = call(
        kernel,
        "kernel_state",
        json!({"type": "delete_agent", "id": id}),
    )
    .await;
}

fn alive(pid: i64) -> bool {
    // SAFETY: signal 0 only probes the pid.
    unsafe { libc_kill(pid as i32, 0) == 0 }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

async fn gone(pid: i64) -> bool {
    for _ in 0..50 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_instance_runs_its_program_and_answers_the_bundles_verbs() {
    let tmp = TempDir::new().unwrap();
    bundle(&tmp, "rs_echoer", ECHO, VERBS);
    let kernel = kernel_for(&tmp);
    load(&kernel, &tmp);
    let r = create(&kernel, "rs_echoer", "rs_e1").await;
    assert_eq!(r["id"], "rs_e1", "{r}");
    let watcher = AgentId::from("_t_watch");
    let mut rx = kernel.ensure_inbox(&watcher);
    assert!(kernel
        .watch(&AgentId::from("rs_e1"), watcher)
        .await
        .is_none());
    let out = call(&kernel, "rs_e1", json!({"type": "echo", "text": "hi"})).await;
    assert_eq!(out["echo"], "hi", "{out}");
    assert_ne!(
        out["pid"].as_i64(),
        Some(i64::from(std::process::id() as i32))
    );
    let mut seen = false;
    for _ in 0..40 {
        if let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            if item.payload["type"] == "echoed" && item.payload["text"] == "hi" {
                seen = true;
                break;
            }
        }
    }
    assert!(
        seen,
        "the program's emit must reach the instance's watchers"
    );
    let rf = call(&kernel, "rs_e1", json!({"type": "reflect"})).await;
    assert_eq!(rf["base"], "py_agent", "{rf}");
    assert_eq!(rf["program"], "running", "{rf}");
    assert!(rf["security"].as_str().unwrap().contains("OS rights"));
    delete(&kernel, "rs_e1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docs_and_the_catalog_read_bundle_yaml() {
    let tmp = TempDir::new().unwrap();
    bundle(&tmp, "rs_documented", ECHO, VERBS);
    let kernel = kernel_for(&tmp);
    load(&kernel, &tmp);
    let d = call(
        &kernel,
        "kernel",
        json!({"type": "docs", "of": "rs_documented"}),
    )
    .await;
    assert_eq!(d["verbs"]["echo"], "args: text:str", "{d}");
    let r = call(
        &kernel,
        "kernel",
        json!({"type": "reflect", "tree": "none", "bundles": "all"}),
    )
    .await;
    let row = r["bundles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "rs_documented")
        .cloned()
        .unwrap();
    assert_eq!(row["base"], "py_agent", "{row}");
    assert_eq!(row["handler_module"], "rs_documented");
    assert_eq!(row["sentence"], "The rs_documented agent.");
    assert_eq!(row["folder"], ".fantastic/bundles/rs_documented");
    assert!(row.get("error").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_program_that_serves_other_verbs_than_declared_stops_saying_so() {
    let tmp = TempDir::new().unwrap();
    bundle(
        &tmp,
        "rs_liar",
        ECHO,
        &format!("{VERBS}  extra: 'No args.'\n"),
    );
    let kernel = kernel_for(&tmp);
    load(&kernel, &tmp);
    create(&kernel, "rs_liar", "rs_l1").await;
    let out = call(&kernel, "rs_l1", json!({"type": "echo", "text": "x"})).await;
    assert_eq!(out["reason"], "transport_closed", "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("must match"),
        "{out}"
    );
    let rf = call(&kernel, "rs_l1", json!({"type": "reflect"})).await;
    assert_eq!(rf["program"], "stopped");
    assert!(rf["program_error"].as_str().unwrap().contains("must match"));
    delete(&kernel, "rs_l1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exit_settles_the_call_and_nothing_respawns() {
    let tmp = TempDir::new().unwrap();
    bundle(&tmp, "rs_mortal", ECHO, VERBS);
    let kernel = kernel_for(&tmp);
    load(&kernel, &tmp);
    create(&kernel, "rs_mortal", "rs_m1").await;
    let out = call(&kernel, "rs_m1", json!({"type": "die"})).await;
    assert_eq!(out["reason"], "transport_closed", "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("exited (code 3)"),
        "{out}"
    );
    let rf = call(&kernel, "rs_m1", json!({"type": "reflect"})).await;
    assert!(
        rf["stderr_tail"]
            .as_str()
            .unwrap_or("")
            .contains("dying on purpose"),
        "{rf}"
    );
    let again = call(&kernel, "rs_m1", json!({"type": "echo", "text": "x"})).await;
    assert_eq!(again["reason"], "transport_closed");
    assert!(
        again["hint"].as_str().unwrap().contains("reload_bundle"),
        "{again}"
    );
    delete(&kernel, "rs_m1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reload_restarts_the_instance_and_answers_the_inflight_call() {
    let tmp = TempDir::new().unwrap();
    let folder = bundle(&tmp, "rs_reloaded", ECHO, VERBS);
    let kernel = kernel_for(&tmp);
    load(&kernel, &tmp);
    create(&kernel, "rs_reloaded", "rs_r1").await;
    let first = call(&kernel, "rs_r1", json!({"type": "echo", "text": "a"})).await;
    let k = Arc::clone(&kernel);
    let pending = tokio::spawn(async move { call(&k, "rs_r1", json!({"type": "slow"})).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(
        folder.join("agent.py"),
        ECHO.replace(
            r#""echo": p.get("text")"#,
            r#""echo": "v2:" + p.get("text")"#,
        ),
    )
    .unwrap();
    // what the root's `reload_bundle` does: re-load, then shutdown + boot each instance
    assert!(fantastic_core::folders::load_bundle(
        &kernel,
        &tmp.path().join(".fantastic"),
        "rs_reloaded"
    )
    .is_none());
    let a = kernel
        .agents
        .get(&AgentId::from("rs_r1"))
        .map(|e| Arc::clone(e.value()))
        .unwrap();
    kernel
        .bookkeeping(&a, json!({"type": "shutdown", "reason": "reloaded"}), None)
        .await;
    kernel.bookkeeping(&a, json!({"type": "boot"}), None).await;
    let late = pending.await.unwrap();
    assert_eq!(late["reason"], "transport_closed", "{late}");
    assert!(
        late["error"].as_str().unwrap().contains("reloaded"),
        "{late}"
    );
    let second = call(&kernel, "rs_r1", json!({"type": "echo", "text": "a"})).await;
    assert_eq!(second["echo"], "v2:a", "{second}");
    assert_ne!(second["pid"], first["pid"]);
    delete(&kernel, "rs_r1").await;
}

/// python `test_a_failed_reload_keeps_the_previous_load_running`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_reload_keeps_the_previous_load_running() {
    let tmp = TempDir::new().unwrap();
    let folder = bundle(&tmp, "rs_kept", ECHO, VERBS);
    let kernel = kernel_for(&tmp);
    load(&kernel, &tmp);
    create(&kernel, "rs_kept", "rs_kp1").await;
    let first = call(&kernel, "rs_kp1", json!({"type": "echo", "text": "a"})).await;
    std::fs::write(folder.join("bundle.yaml"), "base: py_agent\n").unwrap(); // sentence gone
    let err =
        fantastic_core::folders::load_bundle(&kernel, &tmp.path().join(".fantastic"), "rs_kept")
            .expect("refused");
    assert!(err.contains("sentence"), "{err}");
    // the live instance runs the previous load, untouched; the catalog keeps it too
    let again = call(&kernel, "rs_kp1", json!({"type": "echo", "text": "b"})).await;
    assert_eq!(again, json!({"echo": "b", "pid": first["pid"]}));
    let row = kernel
        .bundles
        .folder_rows()
        .into_iter()
        .find(|r| r["name"] == "rs_kept")
        .unwrap();
    assert_eq!(row["base"], "py_agent", "{row}");
    assert!(row.get("error").is_none(), "{row}");
    // and delete still reaches the program
    delete(&kernel, "rs_kp1").await;
    assert!(gone(first["pid"].as_i64().unwrap()).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_kills_the_program() {
    let tmp = TempDir::new().unwrap();
    bundle(&tmp, "rs_killed", ECHO, VERBS);
    let kernel = kernel_for(&tmp);
    load(&kernel, &tmp);
    create(&kernel, "rs_killed", "rs_k1").await;
    let pid = call(&kernel, "rs_k1", json!({"type": "echo", "text": "x"})).await["pid"]
        .as_i64()
        .unwrap();
    delete(&kernel, "rs_k1").await;
    assert!(gone(pid).await, "program {pid} still alive after delete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_programs_task_kills_the_program() {
    let tmp = TempDir::new().unwrap();
    bundle(&tmp, "rs_cancelled", ECHO, VERBS);
    let kernel = kernel_for(&tmp);
    load(&kernel, &tmp);
    create(&kernel, "rs_cancelled", "rs_c1").await;
    let pid = call(&kernel, "rs_c1", json!({"type": "echo", "text": "x"})).await["pid"]
        .as_i64()
        .unwrap();
    let ps = call(&kernel, "kernel", json!({"type": "ps"})).await;
    let prog = ps["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["agent"] == "rs_c1" && t["verb"] == "program")
        .cloned()
        .unwrap();
    let _ = call(
        &kernel,
        "kernel",
        json!({"type": "cancel", "task_id": prog["id"]}),
    )
    .await;
    assert!(gone(pid).await, "program {pid} still alive after cancel");
    let out = call(&kernel, "rs_c1", json!({"type": "echo", "text": "x"})).await;
    assert_eq!(out["reason"], "transport_closed", "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("cancelled"),
        "{out}"
    );
    delete(&kernel, "rs_c1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_that_cannot_load_is_listed_with_its_error() {
    let tmp = TempDir::new().unwrap();
    let cases = [
        ("nobase", "sentence: x\n", "base: required"),
        (
            "badbase",
            "base: no_such_agent\nsentence: x\n",
            "known: py_agent",
        ),
        (
            "nocmd",
            "base: py_agent\nsentence: x\nverbs: {a: 'b'}\n",
            "command: required",
        ),
        (
            "noverbs",
            "base: py_agent\nsentence: x\ncommand: [python3, a.py]\n",
            "verbs: required",
        ),
        (
            "py_agent",
            "base: py_agent\nsentence: x\n",
            "built-in bundle",
        ),
    ];
    for (name, yaml, _) in cases {
        let folder = tmp.path().join(".fantastic").join("bundles").join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("bundle.yaml"), yaml).unwrap();
    }
    let kernel = kernel_for(&tmp);
    load(&kernel, &tmp);
    let r = call(
        &kernel,
        "kernel",
        json!({"type": "reflect", "tree": "none", "bundles": "all"}),
    )
    .await;
    for (name, _, fragment) in cases {
        let row = r["bundles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["name"] == name && b.get("folder").is_some())
            .cloned()
            .unwrap_or_else(|| panic!("no row for {name}: {r}"));
        assert!(
            row["error"].as_str().unwrap().contains(fragment),
            "{name}: {row}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn py_agent_itself_is_a_base_not_an_agent() {
    let tmp = TempDir::new().unwrap();
    let kernel = kernel_for(&tmp);
    let r = create(&kernel, HANDLER_MODULE, "rs_p0").await;
    assert_eq!(r["reason"], "invalid_meta", "{r}");
    assert!(r["error"].as_str().unwrap().contains("is a base"), "{r}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chatty_program_does_not_block_on_its_own_output() {
    let tmp = TempDir::new().unwrap();
    bundle(&tmp, "rs_chatty", ECHO, VERBS);
    let kernel = kernel_for(&tmp);
    load(&kernel, &tmp);
    create(&kernel, "rs_chatty", "rs_ch1").await;
    let out = call(&kernel, "rs_ch1", json!({"type": "chatty"})).await;
    assert_eq!(out, json!({"chatted": true}), "{out}");
    delete(&kernel, "rs_ch1").await;
}

/// A `bundles/<name>` that links OUTSIDE the running dir is refused by the clamp,
/// never loaded (python reads through `fs`; audit 2026-10-14).
#[tokio::test]
async fn a_bundle_folder_linking_outside_is_refused() {
    let tmp = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let real = bundle(&tmp, "rs_escape_src", ECHO, VERBS);
    let away = outside.path().join("rs_escape");
    std::fs::rename(&real, &away).unwrap();
    std::os::unix::fs::symlink(&away, tmp.path().join(".fantastic/bundles/rs_escape")).unwrap();
    let kernel = kernel_for(&tmp);
    let err =
        fantastic_core::folders::load_bundle(&kernel, &tmp.path().join(".fantastic"), "rs_escape")
            .expect("refused");
    assert!(err.contains("escapes root"), "{err}");
    assert!(kernel.bundles.get("rs_escape").is_none());
}
