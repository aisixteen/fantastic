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

#[test]
fn password_ingress_is_refused_on_fs_and_process_legs() {
    // The process half (the fs half lives in fantastic-file): a leg with no wire
    // envelope can never present a token, so both spellings refuse at the write.
    let b = ExecBridgeBundle;
    for (field, spec) in [
        ("ingress_rule", json!("password_ingress")),
        (
            "ingress_rule",
            json!({"type": "password_ingress", "env": "X"}),
        ),
        ("auth", json!("password")),
    ] {
        let mut meta = Map::new();
        meta.insert(field.to_string(), spec);
        let e = b
            .validate_meta(&meta, &Map::new())
            .unwrap_or_else(|| panic!("{field} password must be refused"));
        assert!(e.contains("wire envelope"), "{e}");
        assert!(
            e.contains("allow_all_ingress"),
            "must teach the legal set: {e}"
        );
    }
    let mut ok = Map::new();
    ok.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
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
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "code", "{r}");
    assert_eq!(
        r["error"], "start: code must be absent — run a command instead",
        "{r}"
    );
}

/// EVERY refusal on `start` carries a CODE. Sibling argument errors used to split —
/// `args`/`env` answered `bad_arg` while `command`/`code`/the policy wall answered a
/// bare `{error}` — so nothing could tell a typo from a policy refusal without
/// matching prose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_start_refusal_carries_a_code() {
    let tmp = TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    meta.insert("allowed".to_string(), json!(["echo( .*)?"]));
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let ask = |p: Value| {
        let kernel = Arc::clone(&kernel);
        let a = a.clone();
        async move { kernel.send(vec![Item::new(a, p)]).await.remove(0).result }
    };

    let r = ask(json!({"type": "start"})).await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("missing_arg"), &json!("command")),
        "{r}"
    );
    let r = ask(json!({"type": "start", "command": "echo", "args": [{"no": 1}]})).await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("bad_arg"), &json!("args")),
        "{r}"
    );
    let r = ask(json!({"type": "start", "command": "echo", "env": {"K": ["no"]}})).await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("bad_arg"), &json!("env")),
        "{r}"
    );
    // A POLICY wall, not a malformed argument.
    let r = ask(json!({"type": "start", "command": "rm", "args": ["-rf", "/"]})).await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    assert!(
        r["hint"].as_str().is_some_and(|h| h.contains("allowed")),
        "{r}"
    );
}

/// `status`/`stop`/`interrupt` on a job_id naming nothing answer `not_found` with the
/// id. `stop`/`interrupt` used to answer `{killed:0}` — the same reply as "it had
/// already exited", so a typo read as success; `status` answered a bare `{error}` and
/// a caller branching on `reason == "not_found"` never matched on this runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_job_id_is_not_found_on_every_verb() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    for verb in ["status", "stop", "interrupt"] {
        let r = kernel
            .send(vec![Item::new(
                a.clone(),
                json!({"type": verb, "job_id": "deadbeef"}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], "not_found", "{verb}: {r}");
        assert_eq!(r["job_id"], "deadbeef", "{verb}: {r}");
    }
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
        // The write half of the 16 MiB bound: the client refuses to send a frame
        // the relay would close the socket over, and says what to do instead.
        "_MAX_FRAME",
        "frame exceeds 16 MiB",
        "write_stream",
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

fn inbox(kernel: &Arc<Kernel>, id: &AgentId) -> fantastic_kernel::inbox::InboxReceiver {
    let (tx, rx) = fantastic_kernel::inbox::inbox(kernel.inbox_bound);
    kernel.inboxes.insert(id.clone(), tx);
    rx
}

async fn until(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
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

/// PARITY WITH PYTHON: a program's sends carry the RUNNER's identity and are
/// gated by its grants. A sibling the runner holds no `can_call` for must refuse
/// (`unauthorized`). Before 2026-09-01 the relay forwarded with NO sender —
/// operator, ungated — and a spawned program could reach any agent (measured
/// live: E30 subai:rust, "privileged as the root").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_program_is_gated_as_its_runner_not_as_the_operator() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    // a SIBLING runner under core — no edge from `a` to it (baseline only)
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "create_agent", "parent_id": "core", "id": "sib",
                   "handler_module": "exec_bridge.tools",
                   "ingress_rule": "allow_all_ingress"}),
        )])
        .await
        .remove(0)
        .result;
    let mut rx = inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(
                &py,
                "r = kernel.send('sib', {'type': 'reflect'})\nprint('SIB', r.get('reason'))\n\
                 r2 = kernel.send('core', {'type': 'reflect'})\nprint('CORE', r2.get('id'))\n",
                true,
            ),
        )])
        .await
        .remove(0)
        .result;
    let done = until(&mut rx, |v| v["type"] == "job_done", 25)
        .await
        .expect("no job_done");
    let out = done["stdout"].as_str().unwrap();
    assert!(
        out.contains("SIB unauthorized"),
        "the sibling must refuse: {done}"
    );
    assert!(
        out.contains("CORE core"),
        "the parent (baseline edge) still answers: {done}"
    );
}

/// `connector` hands over the reference wire client — a model pastes it instead of
/// re-deriving the socket framing (measured 2026-09-01: ten turns lost). Ungated
/// like reflect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_connector_verb_hands_over_the_reference_client() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "connector"}))])
        .await
        .remove(0)
        .result;
    let src = r["source"].as_str().expect("source");
    assert!(
        src.contains("FANTASTIC_CTRL_FD") && src.contains("def send("),
        "{r}"
    );
    assert!(r["howto"].as_str().unwrap().contains("can_call"));
}

/// A stream-speaking stand-in for `file_bridge`: `write_stream` keeps the blob,
/// `read_stream` hands it back as the reply BODY — the binary channel only.
struct BlobStore(std::sync::Mutex<Vec<u8>>);

#[async_trait::async_trait]
impl fantastic_kernel::bundle::Bundle for BlobStore {
    fn name(&self) -> &'static str {
        "blob"
    }
    async fn handle(
        &self,
        _id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
        Ok(Some(
            json!({"error": format!("text channel: {}", payload["type"])}),
        ))
    }
    async fn handle_binary(
        &self,
        _id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        _kernel: &Arc<Kernel>,
    ) -> Result<(fantastic_kernel::bundle::Reply, Vec<u8>), fantastic_kernel::bundle::BundleError>
    {
        match header["type"].as_str() {
            Some("write_stream") => {
                let n = blob.len();
                *self.0.lock().unwrap() = blob;
                Ok((Some(json!({"written": n})), Vec::new()))
            }
            Some("read_stream") => {
                let b = self.0.lock().unwrap().clone();
                Ok((Some(json!({"eof": true, "size": b.len()})), b))
            }
            other => Ok((Some(json!({"error": format!("{other:?}")})), Vec::new())),
        }
    }
}

/// `write_stream`/`read_stream` from a program: the bytes ride the fd as the
/// io_bridge binary frame (kind B) both ways — the program's payload bytes reach the
/// bundle's blob, the reply body comes back raw at `bytes`. Never base64, never
/// None (measured 2026-09-02, E30 subai:python: a `read_stream` reply was
/// unserializable on this wire and the worker got None).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_chunk_crosses_the_wire_raw() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ExecBridgeBundle);
    kernel
        .bundles
        .register("blob.tools", BlobStore(std::sync::Mutex::new(Vec::new())));
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
    let a = AgentId::from(agent_id_for(&tmp).as_str());
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "create_agent", "parent_id": "kernel", "id": a.as_str(),
                   "handler_module": HANDLER_MODULE,
                   "ingress_rule": "allow_all_ingress", "allowed": [".*"],
                   "can_call": {"blob": "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "create_agent", "parent_id": "core", "id": "blob",
                   "handler_module": "blob.tools",
                   "callable_by": {a.as_str(): "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let mut rx = inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(
                &py,
                "raw = bytes(range(256)) * 3\n\
                 w = kernel.send('blob', {'type': 'write_stream', 'path': 'b', 'bytes': raw})\n\
                 r = kernel.send('blob', {'type': 'read_stream', 'path': 'b', 'bytes': b''})\n\
                 print('WROTE', w['written'], 'READ', len(r['bytes']), r['bytes'] == raw, \
                 type(r['bytes']).__name__)\n",
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
        done["stdout"]
            .as_str()
            .unwrap()
            .contains("WROTE 768 READ 768 True bytes"),
        "bytes did not round-trip raw: {done}"
    );
}

// ─── the relay: denials, leaks, the frame bound ─────────────────
//
// The relay is the unit under test here, so these drive it over a socketpair the
// way a spawned program would — no subprocess, and no race between the child
// registering a callback and the frame coming back.

/// A relay serving one end of a socketpair, gated as `owner` exactly as `start`
/// gates it. Returns the PROGRAM's end, the stop signal, and the relay's handle.
#[allow(clippy::type_complexity)]
fn wire(
    kernel: &Arc<Kernel>,
    owner: &AgentId,
    job_id: &str,
) -> (
    tokio::net::UnixStream,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    let (parent, child) = tokio::net::UnixStream::pair().expect("socketpair");
    let (rd, wr) = parent.into_split();
    let stop = Arc::new(tokio::sync::Notify::new());
    let relay = tokio::spawn(fantastic_kernel::send::with_sender(
        owner.clone(),
        crate::relay::serve(
            rd,
            wr,
            owner.clone(),
            job_id.to_string(),
            Arc::clone(kernel),
            Arc::clone(&stop),
        ),
    ));
    (child, stop, relay)
}

async fn wire_write(child: &mut tokio::net::UnixStream, v: &Value) {
    use tokio::io::AsyncWriteExt;
    let body = [b"T".as_slice(), &serde_json::to_vec(v).unwrap()].concat();
    child
        .write_all(&[&(body.len() as u32).to_be_bytes()[..], &body].concat())
        .await
        .expect("write");
}

async fn wire_read(child: &mut tokio::net::UnixStream) -> Value {
    use tokio::io::AsyncReadExt;
    let mut head = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), child.read_exact(&mut head))
        .await
        .expect("no frame within 5s")
        .expect("head");
    let mut body = vec![0u8; u32::from_be_bytes(head) as usize];
    child.read_exact(&mut body).await.expect("body");
    serde_json::from_slice(&body[1..]).expect("json")
}

/// `kernel.watch` is gated exactly like a send and can REFUSE. The refusal used to
/// be discarded: the sub was registered anyway, so the program held a subscription
/// that simply never fired — deaf, with nothing to diagnose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_watch_reaches_the_program_and_registers_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    // A sibling the runner holds no edge to: watch must refuse.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("core"),
            json!({"type": "create_agent", "parent_id": "kernel", "id": "secret",
                   "handler_module": HANDLER_MODULE, "ingress_rule": "allow_all_ingress"}),
        )])
        .await;

    let (mut child, stop, relay) = wire(&kernel, &a, "j1");
    wire_write(&mut child, &json!({"op": "watch", "src": "secret"})).await;
    let ev = wire_read(&mut child).await;
    assert_eq!(ev["op"], "event", "{ev}");
    assert_eq!(ev["src"], "secret", "{ev}");
    assert_eq!(ev["payload"]["reason"], "unauthorized", "{ev}");

    // …and NOTHING was registered. No proxy inbox row — and no WATCHER row on the
    // source either, which an emit proves from outside the crate: register the
    // proxy's inbox ourselves, and a leaked watcher fans the payload into it.
    let proxy = AgentId::from("_xbw_j1_secret");
    assert!(!kernel.inboxes.contains_key(&proxy), "proxy inbox leaked");
    let mut spy = inbox(&kernel, &proxy);
    kernel
        .emit(&AgentId::from("secret"), json!({"type": "ping"}))
        .await;
    assert!(spy.try_recv().is_none(), "the watcher row leaked");

    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not stop")
        .expect("relay");
}

/// Teardown SIGNALS the relay instead of aborting it, so the release block after
/// its read loop actually runs: the watcher rows, the INBOX rows and the drain
/// tasks all go back. Aborting skipped that block, and every `_xbw_*`/`_xbself_*`
/// id the job held stayed live — `emit` kept copying payloads into channels nobody
/// drained, one leak per (job, watched src), forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_watching_program_leaves_no_proxy_or_watcher() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let (mut child, stop, relay) = wire(&kernel, &a, "j2");
    // `$self` is a baseline edge, so both of these are permitted.
    wire_write(&mut child, &json!({"op": "watch", "src": a.as_str()})).await;
    wire_write(&mut child, &json!({"op": "onmessage"})).await;

    let proxies = [
        AgentId::from(format!("_xbw_j2_{a}").as_str()),
        AgentId::from("_xbself_j2"),
    ];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline
        && !proxies.iter().all(|p| kernel.inboxes.contains_key(p))
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for p in &proxies {
        assert!(kernel.inboxes.contains_key(p), "{p} was never registered");
    }

    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not stop")
        .expect("relay");

    for p in &proxies {
        assert!(!kernel.inboxes.contains_key(p), "{p} leaked past teardown");
    }
    // The WATCHER rows went too: re-register both proxy inboxes and emit on the
    // owner — a leaked watcher would fan the payload straight into them.
    let mut spies: Vec<_> = proxies.iter().map(|p| inbox(&kernel, p)).collect();
    kernel.emit(&a, json!({"type": "ping"})).await;
    for (p, spy) in proxies.iter().zip(spies.iter_mut()) {
        assert!(spy.try_recv().is_none(), "{p} left a watcher row");
    }
}

/// A length prefix over the bound is a WIRE VIOLATION — the framing itself is
/// unusable, so there is nothing to reply TO and the socket closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_frame_closes_the_control_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let (mut child, _stop, relay) = wire(&kernel, &a, "j3");
    child
        .write_all(&(16 * 1024 * 1024u32 + 1).to_be_bytes())
        .await
        .expect("write");
    let mut byte = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(5), child.read(&mut byte))
        .await
        .expect("the relay never closed")
        .expect("read");
    assert_eq!(n, 0, "the relay must close on a wire violation");
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not exit")
        .expect("relay");
}

/// A cancelled job's row must not sit at `stopping` forever: `clear` refuses that
/// label BY DESIGN, so the row was immortal and the job could never be forgotten.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_job_finalises_to_stopped_and_clear_removes_it() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(&py, "import time; time.sleep(30)", false),
        )])
        .await
        .remove(0)
        .result;
    let job_id = r["job_id"].as_str().expect("job_id").to_string();
    let task_id = {
        let g = jobs::jobs();
        g.get(&job_id).expect("row").task.to_string()
    };
    let c = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "cancel", "task_id": task_id}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(c["cancelled"], true, "{c}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let done = jobs::jobs()
            .get(&job_id)
            .is_some_and(|j| j.status == "stopped");
        if done || tokio::time::Instant::now() > deadline {
            assert!(done, "the cancelled row never left `stopping`");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let cleared = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "clear"}))])
        .await
        .remove(0)
        .result;
    assert!(cleared["cleared"].as_u64().unwrap_or(0) >= 1, "{cleared}");
    let st = kernel
        .send(vec![Item::new(a, json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["jobs"].as_array().map(Vec::len), Some(0), "{st}");
}

/// `kernel.args` / FANTASTIC_ARGS was advertised by every runtime's howto and set
/// by no spawner — a program written from the docs read `{}` and KeyError'd. A
/// command runner takes its arguments as ARGV.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_connector_has_no_args() {
    assert!(!CONNECTOR_SRC.contains("FANTASTIC_ARGS"));
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(a, json!({"type": "connector"}))])
        .await
        .remove(0)
        .result;
    let howto = r["howto"].as_str().expect("howto");
    assert!(
        !howto.contains("`kernel.self_id`, `kernel.args`"),
        "{howto}"
    );
    assert!(howto.contains("ARGV"), "{howto}");
}

/// The other half of the same guarantee, through the REAL path: `stop` →
/// `teardown` → the relay's own cleanup. Teardown used to `abort()` the relay
/// task, which drops `serve` at its await point and skips the release entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_releases_the_proxies_a_real_job_held() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(
                &py,
                "import threading\n\
                 kernel.watch(kernel.self_id, lambda p: None)\n\
                 kernel.on_message(lambda p: None)\n\
                 threading.Event().wait()\n",
                true,
            ),
        )])
        .await
        .remove(0)
        .result;
    let job_id = r["job_id"].as_str().expect("job_id").to_string();
    let proxies = [
        AgentId::from(format!("_xbw_{job_id}_{a}").as_str()),
        AgentId::from(format!("_xbself_{job_id}").as_str()),
    ];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline
        && !proxies.iter().all(|p| kernel.inboxes.contains_key(p))
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for p in &proxies {
        assert!(kernel.inboxes.contains_key(p), "{p} was never registered");
    }

    let _ = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "stop"}))])
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline
        && proxies.iter().any(|p| kernel.inboxes.contains_key(p))
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for p in &proxies {
        assert!(!kernel.inboxes.contains_key(p), "{p} leaked past `stop`");
    }
}
