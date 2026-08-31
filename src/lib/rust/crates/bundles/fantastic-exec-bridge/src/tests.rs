//! Unit tests for the exec_bridge bundle — the general-purpose COMMAND runner.
//!
//! Tests that must actually SPAWN find a real interpreter with `which` and use it
//! as the `command`, with a permissive `allowed=[".*"]` on the record standing in
//! for the operator. A program that wants a `kernel` global includes
//! [`CONNECTOR_SRC`] in its own source (the bridge prepends nothing now). When the
//! host has no python (rare on dev machines, common on minimal CI containers) those
//! tests return successfully — we don't fail CI on systems without Python. The gate
//! tests need no interpreter at all.

use super::*;
use fantastic_kernel::Agent;
use fantastic_kernel::Item;
use serde_json::Map;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

/// Find a real Python interpreter to use as the test `command`, or `None` to skip.
fn find_real_python() -> Option<PathBuf> {
    which::which("python3")
        .or_else(|_| which::which("python"))
        .ok()
}

/// Generate a unique-per-test agent id. The job table is process-global; sharing
/// one id across parallel tests would race.
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

/// A `start` payload that runs `code` under `py`. `with_connector` prepends the
/// vendored wire client, exactly as a program wanting a `kernel` global would.
fn py_start(py: &Path, code: &str, with_connector: bool) -> Value {
    let program = if with_connector {
        format!("{CONNECTOR_SRC}\n{code}")
    } else {
        code.to_string()
    };
    json!({
        "type": "start",
        "command": py.to_string_lossy(),
        "args": ["-u", "-c", program],
    })
}

/// A kernel + one exec_bridge agent with a permissive `allowed=[".*"]` (the
/// operator opening the leg to any command) and the ingress rule OPEN. The seal +
/// the allowlist refusal are covered by dedicated tests.
async fn mk_kernel(tmp: &TempDir) -> (Arc<Kernel>, AgentId) {
    let mut extra = Map::new();
    extra.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    extra.insert("allowed".to_string(), json!([".*"]));
    mk_kernel_with(tmp, extra).await
}

/// `mk_kernel`, but the agent record carries exactly the meta given.
async fn mk_kernel_with(tmp: &TempDir, extra: Map<String, Value>) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ExecBridgeBundle);
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
    let mut create = json!({
        "type": "create_agent", "parent_id": "kernel",
        "handler_module": HANDLER_MODULE,
        "id": pid,
    });
    let obj = create.as_object_mut().expect("object");
    for (k, v) in extra {
        obj.insert(k, v);
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("core"), create)])
        .await
        .remove(0)
        .result;
    (kernel, AgentId::from(pid.as_str()))
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("exec_bridge"));
}

/// The readme teaches the general-purpose COMMAND model, gated by the allowlist,
/// and is honest that it is not a sandbox.
#[test]
fn readme_states_the_command_model() {
    assert!(README.contains("COMMAND"), "{README}");
    assert!(README.contains("allowlist"), "{README}");
    assert!(README.contains("Not a sandbox"), "{README}");
}

// ─── the command gate: a REGEX allowlist ─────────────────────────

#[test]
fn shlex_join_quotes_like_python() {
    // Safe tokens pass through; spaces/special chars force single-quoting; an
    // inner quote is escaped `'"'"'`; an empty token becomes ''.
    assert_eq!(shlex_join(&["ls".into(), "-la".into()]), "ls -la");
    assert_eq!(shlex_join(&["echo".into(), "a b".into()]), "echo 'a b'");
    assert_eq!(shlex_join(&["x".into(), "".into()]), "x ''");
    assert_eq!(
        shlex_join(&["echo".into(), "it's".into()]),
        "echo 'it'\"'\"'s'"
    );
}

#[test]
fn command_permitted_fullmatches_and_refuses() {
    // Empty allowlist ⇒ a loud refusal telling the operator what to set.
    let e = command_permitted(&Map::new(), "ls").expect("empty allowlist must refuse");
    assert!(e.contains("allowed=["), "must teach the fix: {e}");

    // A matching line is permitted; the match is ANCHORED (fullmatch), so a
    // prefix-only pattern cannot smuggle a trailing `; rm -rf /`.
    let mut meta = Map::new();
    meta.insert("allowed".to_string(), json!(["ls( .*)?"]));
    assert!(command_permitted(&meta, "ls").is_none());
    assert!(command_permitted(&meta, "ls -la").is_none());
    let e = command_permitted(&meta, "ls; rm -rf /").expect("must refuse a non-fullmatch");
    assert!(e.contains("not allowed"), "{e}");
    let e = command_permitted(&meta, "rm -rf /").expect("unlisted command must refuse");
    assert!(e.contains("ls( .*)?"), "must name the patterns: {e}");

    // DOTALL: a multi-line arg (a `-c` program) is spanned by `.*`.
    let mut m2 = Map::new();
    m2.insert("allowed".to_string(), json!([".*"]));
    assert!(command_permitted(&m2, "python -c 'a\nb\nc'").is_none());
}

#[test]
fn validate_meta_refuses_withdrawn_fields_and_bad_allowed() {
    let b = ExecBridgeBundle;
    // The withdrawn code-runner fields are refused at the WRITE.
    for gone in [
        "code",
        "shim",
        "interpreter",
        "interpreters",
        "argv_template",
    ] {
        let mut meta = Map::new();
        meta.insert(gone.to_string(), json!("whatever"));
        let e = b
            .validate_meta(&meta, &Map::new())
            .unwrap_or_else(|| panic!("{gone} must be refused"));
        assert!(e.contains(gone), "refusal must name the field: {e}");
        assert!(
            e.contains("allowed"),
            "refusal must point to `allowed`: {e}"
        );
    }
    // `allowed` must be a non-empty list of compilable regexes.
    let mut empty = Map::new();
    empty.insert("allowed".to_string(), json!([]));
    assert!(b.validate_meta(&empty, &Map::new()).is_some());
    let mut broken = Map::new();
    broken.insert("allowed".to_string(), json!(["("]));
    let e = b
        .validate_meta(&broken, &Map::new())
        .expect("broken regex refused");
    assert!(e.contains("broken regex"), "{e}");
    // A good allowlist validates clean.
    let mut ok = Map::new();
    ok.insert("allowed".to_string(), json!(["ls( .*)?", ".*"]));
    assert!(b.validate_meta(&ok, &Map::new()).is_none());
}

#[tokio::test]
async fn reflect_advertises_the_allowlist_not_interpreters() {
    let tmp = TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("allowed".to_string(), json!(["ls( .*)?"]));
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let r = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], a.as_str());
    assert_eq!(
        r["sentence"],
        "Process edge of the io family — a general-purpose COMMAND runner, sealed by default, gated by the `allowed` regexes. Spawns commands in the background; a child may speak the kernel over a private control fd."
    );
    assert_eq!(r["running"], 0);
    // THE GATE is advertised; the retired interpreter/shim keys are gone.
    assert_eq!(r["allowed"], json!(["ls( .*)?"]));
    assert!(r.get("interpreters").is_none(), "{r}");
    assert!(r.get("shims").is_none(), "{r}");
    assert!(r["wire"]
        .as_str()
        .unwrap_or("")
        .contains("FANTASTIC_CTRL_FD"));
    for v in [
        "reflect",
        "start",
        "status",
        "clear",
        "interrupt",
        "stop",
        "boot",
    ] {
        assert!(r["verbs"][v].is_string(), "verb {v} missing from reflect");
    }
}

/// A `code` field is refused explicitly — this runs COMMANDS, not code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_refuses_a_code_field() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "start", "code": "x = 1"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r["error"], "exec_bridge: `code` is not a field — run a command instead",
        "{r}"
    );
}

/// A command that matches no `allowed` pattern is refused, naming the patterns, and
/// nothing is spawned. And a SEALED leg (no ingress_rule) denies `start` before the
/// allowlist is even consulted, while `reflect` still answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_refuses_unlisted_command_and_sealed_leg_denies() {
    let tmp = TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    meta.insert("allowed".to_string(), json!(["ls( .*)?"])); // only ls
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "start", "command": "/bin/rm", "args": ["-rf", "/"]}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        r["error"].as_str().unwrap_or("").contains("not allowed"),
        "an unlisted command must refuse naming the patterns: {r}"
    );
    assert!(r["job_id"].is_null(), "nothing may be spawned: {r}");
    let st = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(
        st["jobs"].as_array().map(Vec::len),
        Some(0),
        "a refused start left a job row: {st}"
    );

    // SEALED BY DEFAULT — a fresh leg (no ingress_rule) denies every owned verb
    // except reflect, and the denial teaches the fix.
    let tmp2 = TempDir::new().unwrap();
    let (k2, sealed) = mk_kernel_with(&tmp2, Map::new()).await;
    let d = k2
        .send(vec![Item::new(
            sealed.clone(),
            json!({"type": "start", "command": "ls"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(d["reason"], json!("unauthorized"), "{d}");
    assert!(
        d["hint"]
            .as_str()
            .unwrap_or("")
            .contains("ingress_rule=allow_all_ingress"),
        "the denial must teach how to open: {d}"
    );
    let refl = k2
        .send(vec![Item::new(sealed.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(refl["sealed"], json!(true), "{refl}");
    assert!(
        refl["allowed"].is_array(),
        "discovery must work sealed: {refl}"
    );
}

/// SIGINT reaches the job — the program catches it and exits cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_sends_sigint_to_the_job() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &pid);
    let code = r#"import signal,time,sys
def h(s,f):
    print("CAUGHT"); sys.stdout.flush(); sys.exit(0)
signal.signal(signal.SIGINT, h)
time.sleep(30)
"#;
    let r = kernel
        .send(vec![Item::new(pid.clone(), py_start(&py, code, false))])
        .await
        .remove(0)
        .result;
    assert!(r["job_id"].is_string(), "start failed: {r}");
    tokio::time::sleep(Duration::from_millis(800)).await;

    let n = kernel
        .send(vec![Item::new(pid.clone(), json!({"type": "interrupt"}))])
        .await
        .remove(0)
        .result;
    assert!(
        n["interrupted"].as_u64().unwrap_or(0) >= 1,
        "interrupt returned {n}"
    );

    let done = until(&mut rx, |v| v["type"] == "job_done", 20)
        .await
        .expect("no job_done after interrupt");
    assert!(
        done["stdout"].as_str().unwrap_or("").contains("CAUGHT"),
        "handler did not run: {done}"
    );
}

/// SIGKILL reaches the job, and the row does not stay stuck `running`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_sigkills_the_job() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &pid);
    let r = kernel
        .send(vec![Item::new(
            pid.clone(),
            py_start(&py, "import time; time.sleep(30)", false),
        )])
        .await
        .remove(0)
        .result;
    assert!(r["job_id"].is_string(), "start failed: {r}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let n = kernel
        .send(vec![Item::new(pid.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
    assert!(n["killed"].as_u64().unwrap_or(0) >= 1, "stop returned {n}");

    until(&mut rx, |v| v["type"] == "job_done", 20)
        .await
        .expect("no job_done after stop");
    let st = kernel
        .send(vec![Item::new(pid.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    let status = st["jobs"][0]["status"].as_str().unwrap_or("");
    assert_ne!(status, "running", "row left stuck running: {st}");
}

// ─── the vendored connector ─────────────────────────────────────

/// The vendored copy must be byte-identical to python's. A copy without this test
/// is worse than no copy: drift would surface as a program that runs on one kernel
/// and mysteriously fails on another.
///
/// Skips (loudly) when the python tree is absent — the container's rust stage copies
/// only `src/lib/rust/`. CI checks out the whole repo, so it runs there.
#[test]
fn connector_matches_python_source() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../..")
        .join("src/lib/python/bundled_agents/io/exec_bridge/src/exec_bridge/_connector.py");
    let Ok(src) = std::fs::read_to_string(&py) else {
        eprintln!("skipping: python tree not present at {}", py.display());
        return;
    };
    let open = "CONNECTOR_SRC = r\"\"\"";
    let i = src
        .find(open)
        .expect("python _connector.py has no CONNECTOR_SRC")
        + open.len();
    let j = src.rfind("\"\"\"").expect("unterminated CONNECTOR_SRC");
    assert_eq!(
        &src[i..j],
        CONNECTOR_SRC,
        "vendored connector has drifted from {}. Re-vendor it: the child half of the \
         wire is shared by every runtime and must not fork.",
        py.display()
    );
}

/// Runs everywhere, including a rust-only checkout where the test above skips.
#[test]
fn connector_carries_the_wire_contract() {
    for needle in [
        "FANTASTIC_CTRL_FD",
        "FANTASTIC_SELF_ID",
        "_st.pack(\">I\"",
        "_o._exit(0)",
    ] {
        assert!(
            CONNECTOR_SRC.contains(needle),
            "vendored connector is missing {needle:?}"
        );
    }
}

// ─── the control-socket handoff ─────────────────────────────────

/// THE decisive test for the durable design: does the control socket actually reach
/// the child at the advertised number? `spawn_job` now takes the per-call env block
/// (no `FANTASTIC_ARGS`), so this drives it directly with an empty env.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_child_receives_the_control_socket_on_the_advertised_fd() {
    let Some(py) = find_real_python() else {
        eprintln!("skipping: no python3");
        return;
    };
    let code = r#"
import os, stat
fd = int(os.environ["FANTASTIC_CTRL_FD"])
print("fd", fd, "sock", stat.S_ISSOCK(os.fstat(fd).st_mode), "self", os.environ["FANTASTIC_SELF_ID"], "env", os.environ.get("MYVAR"))
"#;
    let argv = vec![
        py.to_string_lossy().to_string(),
        "-u".to_string(),
        "-c".to_string(),
        code.to_string(),
    ];
    let mut env = Map::new();
    env.insert("MYVAR".to_string(), json!("hello"));
    let sp = crate::spawn::spawn_job(&argv, "owner_agent", &env, None).expect("spawn");
    let out = sp.child.wait_with_output().await.expect("wait");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("sock True"),
        "control fd is not a socket: {text}"
    );
    assert!(
        text.contains("self owner_agent"),
        "self_id not delivered: {text}"
    );
    assert!(
        text.contains("env hello"),
        "per-call env not delivered: {text}"
    );
}

/// The connector's lifeline: when the parent drops the control socket the child must
/// reap ITSELF (`_o._exit(0)`), even parked forever. The program includes the
/// connector in its own source, exactly as a parked durable tool would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_child_self_reaps_when_the_control_socket_closes() {
    let Some(py) = find_real_python() else {
        eprintln!("skipping: no python3");
        return;
    };
    let program = format!("{CONNECTOR_SRC}\nimport threading\nthreading.Event().wait()\n");
    let argv = vec![
        py.to_string_lossy().to_string(),
        "-u".to_string(),
        "-c".to_string(),
        program,
    ];
    let mut sp = crate::spawn::spawn_job(&argv, "owner_agent", &Map::new(), None).expect("spawn");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        sp.child.try_wait().expect("try_wait").is_none(),
        "child exited early"
    );

    drop(sp.ctrl); // the only thing holding the socket open
    let status = tokio::time::timeout(Duration::from_secs(10), sp.child.wait())
        .await
        .expect("child did not self-reap within 10s")
        .expect("wait");
    assert_eq!(status.code(), Some(0), "self-reap should exit 0");
}

// ─── durable jobs, end to end ───────────────────────────────────

fn inbox(kernel: &Arc<Kernel>, id: &AgentId) -> tokio::sync::mpsc::Receiver<Value> {
    let (tx, rx) = tokio::sync::mpsc::channel(kernel.inbox_bound);
    kernel.inboxes.insert(id.clone(), tx);
    rx
}

async fn until(
    rx: &mut tokio::sync::mpsc::Receiver<Value>,
    pred: impl Fn(&Value) -> bool,
    secs: u64,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(v)) if pred(&v) => return Some(v),
            Ok(Some(_)) => {}
            Ok(None) => return None,
            Err(_) => {}
        }
    }
    None
}

/// `start` returns AT ONCE — that is the contract. It also RECORDS what ran:
/// `command`/`args` surface in `status` so a caller can re-issue verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_returns_a_job_id_immediately_and_records_the_invocation() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let t0 = std::time::Instant::now();
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(&py, "import time; time.sleep(30)", false),
        )])
        .await
        .remove(0)
        .result;
    assert!(r["job_id"].is_string(), "no job_id: {r}");
    assert_eq!(r["status"], "running");
    assert!(t0.elapsed() < Duration::from_secs(5), "start blocked");
    let st = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["jobs"][0]["command"], json!(py.to_string_lossy()));
    assert_eq!(st["jobs"][0]["args"][0], json!("-u"));
    let _ = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

/// One `progress` per line, and `n` is ONE sequence across stdout AND stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_streams_per_line_with_a_shared_counter() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(
                &py,
                "import sys\nprint('o1')\nprint('e1', file=sys.stderr)\nprint('o2')\n",
                false,
            ),
        )])
        .await
        .remove(0)
        .result;
    let done = until(&mut rx, |v| v["type"] == "job_done", 20)
        .await
        .expect("no job_done");
    assert_eq!(done["ok"], true, "job failed: {done}");
    assert!(done["stdout"].as_str().unwrap().contains("o1"));
    assert!(done["stderr"].as_str().unwrap().contains("e1"));
}

/// The connector round-trip: a program that includes the wire client reaches ANOTHER
/// agent by id and gets a reply. This is the whole point of the control socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_program_can_send_to_another_agent_and_read_the_reply() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(
                &py,
                "r = kernel.send('core', {'type': 'reflect'})\nprint('GOT', r.get('id'))\n",
                true,
            ),
        )])
        .await
        .remove(0)
        .result;
    let done = until(&mut rx, |v| v["type"] == "job_done", 25)
        .await
        .expect("no job_done");
    assert!(
        done["stdout"].as_str().unwrap().contains("GOT core"),
        "connector send did not round-trip: {done}"
    );
}

/// An unknown verb is MAIL for the running program, ACKed `{queued:true}` — and
/// delivered exactly once (the fanout already did it; re-emitting would double it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_verb_is_queued_to_the_running_program() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    let code = r#"
import threading
seen = []
def _on(m):
    if m.get('type') == 'ping':
        seen.append(1)
        kernel.emit({'type': 'pong', 'count': len(seen)})
kernel.on_message(_on)
threading.Event().wait()
"#;
    let _ = kernel
        .send(vec![Item::new(a.clone(), py_start(&py, code, true))])
        .await
        .remove(0)
        .result;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let ack = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "ping"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(ack["queued"], true, "not queued: {ack}");
    let pong = until(&mut rx, |v| v["type"] == "pong", 20)
        .await
        .expect("program never received the turn");
    assert_eq!(pong["count"], 1, "delivered more than once: {pong}");
    let _ = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

/// No running job ⇒ a loud error, never a silent drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_verb_without_a_job_errors_loudly() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "ping"}))])
        .await
        .remove(0)
        .result;
    assert!(
        r["error"].as_str().unwrap_or("").contains("no running job"),
        "{r}"
    );
}
