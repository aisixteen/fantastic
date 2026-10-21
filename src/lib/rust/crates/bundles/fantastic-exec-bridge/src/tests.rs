//! Unit tests for the exec_bridge bundle — the general-purpose COMMAND runner.
//!
//! Tests that must actually SPAWN find a real interpreter with `which` and use it
//! as the `command`, with a permissive `allowed=[".*"]` on the record standing in
//! for the operator. A program that wants a `kernel` global includes
//! [`CONNECTOR_SRC`] in its own source (the bridge prepends nothing now). When the
//! host has no python (rare on dev machines, common on minimal CI containers) those
//! tests return successfully — the suite doesn't fail on systems without Python. The gate
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
/// A test program's first line: its stdout/stderr end a line with `\n` on every
/// OS (Windows' text mode writes `\r\n`, and a job keeps the `\r` as data).
const LF_STDIO: &str =
    "import sys; sys.stdout.reconfigure(newline='\\n'); sys.stderr.reconfigure(newline='\\n')\n";

fn py_start(py: &Path, code: &str, with_connector: bool) -> Value {
    let program = if with_connector {
        format!("{LF_STDIO}{CONNECTOR_SRC}\n{code}")
    } else {
        format!("{LF_STDIO}{code}")
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
    // a bare command is found on the CHILD's PATH only
    extra.insert("inherit_env".to_string(), json!(["PATH"]));
    mk_kernel_with(tmp, extra).await
}

/// The environment a record that inherits nothing gives a child: only what the
/// OS needs in every process (`sys::OS_ENV`).
fn base_env() -> Map<String, Value> {
    child_env(&Map::new())
}

/// What a child spawned straight through `spawn_job` printed, `(stdout,
/// stderr)`, once it has exited — both pipes read together, so neither can fill
/// and stall it.
async fn output_of(child: &mut crate::sys::Child) -> (Vec<u8>, Vec<u8>) {
    use tokio::io::AsyncReadExt;
    let mut so = child.stdout.take().expect("piped stdout");
    let mut se = child.stderr.take().expect("piped stderr");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (o, e) = tokio::join!(so.read_to_end(&mut out), se.read_to_end(&mut err));
    o.expect("read stdout");
    e.expect("read stderr");
    child.wait().await.expect("wait");
    (out, err)
}

/// `mk_kernel`, but the agent record carries exactly the meta given.
async fn mk_kernel_with(tmp: &TempDir, extra: Map<String, Value>) -> (Arc<Kernel>, AgentId) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ExecBridgeBundle);
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
    let pid = agent_id_for(tmp);
    // WHERE a command runs is the record's, required — never guessed. A test that
    // names its own `cwd` (or refuses one) passes it in `extra`.
    let mut create = json!({
        "type": "create_agent", "parent_id": "kernel",
        "handler_module": HANDLER_MODULE,
        "id": pid,
        "cwd": tmp.path().to_string_lossy(),
    });
    let obj = create.as_object_mut().expect("object");
    for (k, v) in extra {
        obj.insert(k, v);
    }
    let _ = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
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
fn validate_meta_refuses_a_bad_allowed() {
    let b = ExecBridgeBundle;
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
    // THE GATE is advertised.
    assert_eq!(r["allowed"], json!(["ls( .*)?"]));
    assert!(r.get("wire").is_none() && r.get("verbs").is_none(), "{r}");
    assert!(ExecBridgeBundle.notes_doc().unwrap()["wire"]
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
        assert!(
            ExecBridgeBundle.verb_docs()[v].is_string(),
            "verb {v} missing from docs"
        );
    }
}

/// EVERY refusal on `start` carries a CODE — `args`/`env`, `command`/`code` and
/// the policy wall alike — so a typo is told from a policy refusal without
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
/// id — never `{killed:0}` (the same reply as "it had already exited", so a typo
/// would read as success) nor a bare `{error}` a caller branching on
/// `reason == "not_found"` cannot match.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_job_id_is_not_found_on_every_verb() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    for verb in ["status", "stop", "interrupt"] {
        let r = kernel
            .send(vec![Item::new(
                a.clone(),
                json!({"type": verb, "job_id": 999_999}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], "not_found", "{verb}: {r}");
        assert_eq!(r["job_id"], 999_999, "{verb}: {r}");
    }
}

/// One id space: the job's id IS its kernel task id — `ps` lists it, `cancel`
/// takes it. A string (even a stringified int) is a bad argument, never looked up:
/// the old random 8-hex ids came back from a model as ints or infinite floats
/// (`04203581`, `458e5912`) and 3.7% of jobs could not be addressed again
/// (python `test_a_job_id_is_the_jobs_task_id_and_nothing_else_names_it`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_id_is_the_jobs_task_id_and_nothing_else_names_it() {
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
    let jid = r["job_id"]
        .as_i64()
        .unwrap_or_else(|| panic!("an int job_id: {r}"));
    let ps = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ps"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(
        ps["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .any(|t| t["id"] == json!(jid) && t["agent"] == json!(a.as_str())),
        "the job's id must be a row of `ps`: {ps}"
    );
    for verb in ["status", "stop", "interrupt", "clear", "write", "output"] {
        let bad_arg = kernel
            .send(vec![Item::new(
                a.clone(),
                json!({"type": verb, "job_id": jid.to_string()}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(
            (&bad_arg["reason"], &bad_arg["arg"]),
            (&json!("bad_arg"), &json!("job_id")),
            "{verb}: {bad_arg}"
        );
    }
    let snap = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "status", "job_id": jid}),
        )])
        .await
        .remove(0)
        .result;
    let snap = &snap["jobs"][0];
    assert_eq!(snap["job_id"], json!(jid), "{snap}");
    assert!(
        snap.get("task_id").is_none(),
        "no second copy of the id: {snap}"
    );
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "stop", "job_id": jid}),
        )])
        .await;
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
# the OS's interrupt: SIGINT, or SIGBREAK (a Windows pipe job's CTRL_BREAK)
signal.signal(getattr(signal, "SIGBREAK", signal.SIGINT), h)
for _ in range(300):
    time.sleep(0.1)
"#;
    let r = kernel
        .send(vec![Item::new(pid.clone(), py_start(&py, code, false))])
        .await
        .remove(0)
        .result;
    assert!(r["job_id"].is_i64(), "start failed: {r}");
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
        json!(crate::jobs::printed(&pid, &done, "stdout"))
            .as_str()
            .unwrap_or("")
            .contains("CAUGHT"),
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
    assert!(r["job_id"].is_i64(), "start failed: {r}");
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
/// only `src/lib/rust/`. A full checkout runs it.
#[test]
fn connector_matches_python_source() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4) // src/lib above crates/bundles/<crate>
        .expect("src/lib above the crate")
        .join("python/bundled_agents/io/exec_bridge/src/exec_bridge/_connector.py");
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
    // The connector's own call: a stream socket at that number, on every OS.
    let code = r#"
import os, socket
fd = int(os.environ["FANTASTIC_CTRL_FD"])
s = socket.socket(fileno=fd)
print("fd", fd, "sock", s.type == socket.SOCK_STREAM, "self", os.environ["FANTASTIC_SELF_ID"], "env", os.environ.get("MYVAR"))
"#;
    let argv = vec![
        py.to_string_lossy().to_string(),
        "-u".to_string(),
        "-c".to_string(),
        code.to_string(),
    ];
    let mut env = base_env();
    env.insert("MYVAR".to_string(), json!("hello"));
    let mut sp = crate::spawn::spawn_job(&argv, "owner_agent", &env, ".").expect("spawn");
    let (out, _) = output_of(&mut sp.child).await;
    let text = String::from_utf8_lossy(&out);
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
    let mut sp = crate::spawn::spawn_job(&argv, "owner_agent", &base_env(), ".").expect("spawn");
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
    kernel.ensure_inbox(id)
}

async fn until(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    pred: impl Fn(&Value) -> bool,
    secs: u64,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(v)) if pred(&v.payload) => return Some(v.payload),
            Ok(Some(_)) => {}
            Ok(None) => return None,
            Err(_) => {}
        }
    }
    None
}

/// A bare command is found on the CHILD's `PATH` only (python `proc.resolve`): no
/// `PATH` in the child env refuses naming it, and the kernel's own PATH is never
/// searched — std's own lookup would fall back to it, or to `/usr/bin:/bin`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_command_is_found_on_the_childs_path_only() {
    let tmp = TempDir::new().unwrap();
    let bin = TempDir::new().unwrap();
    crate::sys::echo_script(bin.path(), "ft_only_here", "found-by-child-path");
    let mut extra = Map::new();
    extra.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    extra.insert("allowed".to_string(), json!([".*"]));
    let (kernel, a) = mk_kernel_with(&tmp, extra.clone()).await;
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "start", "command": "echo", "args": ["x"]}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("missing_arg"), &json!("PATH")),
        "{r}"
    );

    let tmp2 = TempDir::new().unwrap();
    extra.insert(
        "env".to_string(),
        json!({"PATH": bin.path().to_string_lossy()}),
    );
    let (k2, b) = mk_kernel_with(&tmp2, extra).await;
    let mut rx = inbox(&k2, &b);
    let r = k2
        .send(vec![Item::new(
            b.clone(),
            json!({"type": "start", "command": "ft_only_here"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(r["job_id"].is_i64(), "{r}");
    let done = until(&mut rx, |p| p["type"] == "job_done", 10)
        .await
        .expect("job_done");
    // a console program on Windows ends its line with `\r\n`
    let printed = crate::jobs::printed(&b, &done, "stdout");
    assert_eq!(
        json!(printed.replace("\r\n", "\n")),
        json!("found-by-child-path\n"),
        "{done}"
    );
    // …and one only the kernel's PATH holds is not found.
    let r = k2
        .send(vec![Item::new(
            b.clone(),
            json!({"type": "start", "command": "echo", "args": ["x"]}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "transport_error", "{r}");
    assert!(r["error"].as_str().unwrap().contains("No such file"), "{r}");
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
    assert!(r["job_id"].is_i64(), "no job_id: {r}");
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
    assert!(json!(crate::jobs::printed(&a, &done, "stdout"))
        .as_str()
        .unwrap()
        .contains("o1"));
    assert!(json!(crate::jobs::printed(&a, &done, "stderr"))
        .as_str()
        .unwrap()
        .contains("e1"));
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
    fantastic_kernel::test_support::wire(&kernel, a.as_str(), "kernel_state", json!(["reflect"]));
    let mut rx = inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(
                &py,
                "r = kernel.send('kernel_state', {'type': 'reflect'})\nprint('GOT', r.get('id'))\n",
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
        json!(crate::jobs::printed(&a, &done, "stdout"))
            .as_str()
            .unwrap()
            .contains("GOT kernel_state"),
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
        .expect("program never received the message");
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
/// (`unauthorized`) — a relay forwarding with NO sender would be the operator,
/// ungated, and a spawned program could reach any agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_program_is_gated_as_its_runner_not_as_the_operator() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    // the root: an explicit edge, both ends
    fantastic_kernel::test_support::wire(&kernel, a.as_str(), "kernel_state", json!(["reflect"]));
    // a SIBLING runner under the root — no edge from `a` to it
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel_state", "id": "sib",
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
                 r2 = kernel.send('kernel_state', {'type': 'reflect'})\nprint('CORE', r2.get('id'))\n",
                true,
            ),
        )])
        .await
        .remove(0)
        .result;
    let done = until(&mut rx, |v| v["type"] == "job_done", 25)
        .await
        .expect("no job_done");
    let out = crate::jobs::printed(&a, &done, "stdout");
    let out = out.as_str();
    assert!(
        out.contains("SIB unauthorized"),
        "the sibling must refuse: {done}"
    );
    assert!(
        out.contains("CORE kernel_state"),
        "the root (an explicit edge) still answers: {done}"
    );
}

/// `connector` hands over the reference wire client — a model pastes it instead of
/// re-deriving the socket framing. Ungated like reflect.
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
        body: Option<fantastic_kernel::Blob>,
        _kernel: &Arc<Kernel>,
    ) -> Result<
        (
            fantastic_kernel::bundle::Reply,
            Option<fantastic_kernel::Blob>,
        ),
        fantastic_kernel::bundle::BundleError,
    > {
        let blob: Vec<u8> = body.map(|b| b.bytes).unwrap_or_default();
        match header["type"].as_str() {
            Some("write_stream") => {
                let n = blob.len();
                *self.0.lock().unwrap() = blob;
                Ok((Some(json!({"written": n})), None))
            }
            Some("read_stream") => {
                let b = self.0.lock().unwrap().clone();
                Ok((
                    Some(json!({"eof": true, "size": b.len()})),
                    Some(fantastic_kernel::Blob::key("bytes", b)),
                ))
            }
            other => Ok((Some(json!({"error": format!("{other:?}")})), None)),
        }
    }
}

/// `write_stream`/`read_stream` from a program: the bytes ride the fd as the
/// io_bridge binary frame (kind B) both ways — the program's payload bytes reach the
/// bundle's blob, the reply body comes back raw at `bytes`. Never base64, never
/// None: a `read_stream` reply must be serializable on this wire, or the worker
/// gets None.
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
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let a = AgentId::from(agent_id_for(&tmp).as_str());
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "id": a.as_str(),
                   "handler_module": HANDLER_MODULE,
                   "ingress_rule": "allow_all_ingress", "allowed": [".*"],
                   "cwd": tmp.path().to_string_lossy(),
                   "can_call": {"blob": "*"}}),
        )])
        .await
        .remove(0)
        .result;
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel_state", "id": "blob",
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
        json!(crate::jobs::printed(&a, &done, "stdout"))
            .as_str()
            .unwrap()
            .contains("WROTE 768 READ 768 True bytes"),
        "bytes did not round-trip raw: {done}"
    );
}

/// A TEXT send (kind T, no bytes in the call) whose reply holds bytes comes back
/// kind B: python's relay `encode_frame`s every reply dict, with no list of verbs,
/// so the program needs no `bytes: b''` to get its chunk raw. Swift's
/// `aTextSendsBytesReplyComesBackRaw` twin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_text_sends_bytes_reply_comes_back_raw() {
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
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let a = AgentId::from(agent_id_for(&tmp).as_str());
    for rec in [
        json!({"type": "create_agent", "parent_id": "kernel", "id": a.as_str(),
               "handler_module": HANDLER_MODULE,
               "ingress_rule": "allow_all_ingress", "allowed": [".*"],
               "cwd": tmp.path().to_string_lossy(),
               "can_call": {"blob": "*"}}),
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "blob",
               "handler_module": "blob.tools",
               "callable_by": {a.as_str(): "*"}}),
    ] {
        let r = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), rec)])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "{r}");
    }
    let mut rx = inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(
                &py,
                "raw = bytes(range(256))\n\
                 kernel.send('blob', {'type': 'write_stream', 'path': 'b', 'bytes': raw})\n\
                 r = kernel.send('blob', {'type': 'read_stream', 'path': 'b'})\n\
                 print('READ', r.get('error'), type(r.get('bytes')).__name__, \
                 r.get('bytes') == raw)\n",
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
        json!(crate::jobs::printed(&a, &done, "stdout"))
            .as_str()
            .unwrap()
            .contains("READ None bytes True"),
        "a text send's bytes reply was not raw: {done}"
    );
}

/// The same reply AS IT CROSSES the fd, from the real `file_bridge`: a kind-T
/// `send` of `read_stream` is answered by ONE kind-B frame whose `_binary_path` is
/// `["data", "bytes"]`, the header holding `null` in that slot and the raw body
/// beside it — python's `encode_frame` of `{op, rid, data: {..., bytes: b"…"}}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_text_read_stream_is_answered_by_one_binary_frame() {
    use tokio::io::AsyncReadExt;
    let tmp = TempDir::new().unwrap();
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ExecBridgeBundle);
    kernel
        .bundles
        .register("file_bridge.tools", fantastic_file::FileBundle::default());
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
    let served = tmp.path().join("served");
    std::fs::create_dir_all(&served).unwrap();
    let raw: Vec<u8> = (0..=255u8).collect();
    std::fs::write(served.join("blob.bin"), &raw).unwrap();
    let a = AgentId::from(agent_id_for(&tmp).as_str());
    for (rec, id) in [
        (
            json!({"type": "create_agent", "parent_id": "kernel", "id": a.as_str(),
                   "handler_module": HANDLER_MODULE,
                   "ingress_rule": "allow_all_ingress",
                   "cwd": tmp.path().to_string_lossy(),
                   "can_call": {"fb": "*"}}),
            a.as_str(),
        ),
        (
            json!({"type": "create_agent", "parent_id": "kernel_state", "id": "fb",
                   "handler_module": "file_bridge.tools", "allow": ["."],
                   "ingress_rule": "allow_all_ingress",
                   "root": served.to_string_lossy(),
                   "callable_by": {a.as_str(): "*"}}),
            "fb",
        ),
    ] {
        let r = kernel
            .send(vec![Item::new(AgentId::from("kernel_state"), rec)])
            .await
            .remove(0)
            .result;
        assert_eq!(r["id"], id, "{r}");
    }

    let (mut child, stop, relay) = wire(&kernel, &a, 9004);
    wire_write(
        &mut child,
        &json!({"op": "send", "rid": 7, "target": "fb",
                "payload": {"type": "read_stream", "path": "blob.bin"}}),
    )
    .await;
    let mut head = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), child.read_exact(&mut head))
        .await
        .expect("no frame within 5s")
        .expect("head");
    let mut body = vec![0u8; u32::from_be_bytes(head) as usize];
    child.read_exact(&mut body).await.expect("body");
    assert_eq!(body[0], b'B', "a bytes reply must cross as a binary frame");
    let f = fantastic_io_bridge::codec::decode_binary_frame(&body[1..]).expect("a frame");
    assert_eq!(
        f.path,
        Some(vec![
            fantastic_kernel::Step::from("data"),
            fantastic_kernel::Step::from("bytes")
        ])
    );
    assert_eq!(f.body, raw);
    assert_eq!(f.head["op"], "reply", "{}", f.head);
    assert_eq!(f.head["rid"], 7, "{}", f.head);
    assert_eq!(f.head["data"]["bytes"], Value::Null, "{}", f.head);
    assert_eq!(f.head["data"]["eof"], true, "{}", f.head);
    assert_eq!(f.head["data"]["size"], 256, "{}", f.head);

    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not stop")
        .expect("relay");
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
    job_id: i64,
) -> (
    crate::sys::CtrlStream,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    let (parent, child) = crate::sys::ctrl_pair().expect("socketpair");
    let parent = crate::sys::ctrl_stream(parent).expect("async parent end");
    let child = crate::sys::ctrl_stream(child).expect("async child end");
    let (rd, wr) = parent.into_split();
    let stop = Arc::new(tokio::sync::Notify::new());
    let relay = tokio::spawn(fantastic_kernel::test_support::as_agent(
        owner.clone(),
        crate::relay::serve(
            rd,
            wr,
            owner.clone(),
            TaskId(job_id),
            Arc::clone(kernel),
            Arc::clone(&stop),
            None,
        ),
    ));
    (child, stop, relay)
}

async fn wire_write(child: &mut crate::sys::CtrlStream, v: &Value) {
    use tokio::io::AsyncWriteExt;
    let body = [b"T".as_slice(), &serde_json::to_vec(v).unwrap()].concat();
    child
        .write_all(&[&(body.len() as u32).to_be_bytes()[..], &body].concat())
        .await
        .expect("write");
}

async fn wire_read(child: &mut crate::sys::CtrlStream) -> Value {
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

/// `kernel.watch` is gated exactly like a send and can REFUSE. A discarded
/// refusal with the sub registered anyway would leave the program a subscription
/// that never fires — deaf, with nothing to diagnose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_watch_reaches_the_program_and_registers_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    // A sibling the runner holds no edge to: watch must refuse.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "id": "secret",
                   "handler_module": HANDLER_MODULE, "ingress_rule": "allow_all_ingress"}),
        )])
        .await;

    let (mut child, stop, relay) = wire(&kernel, &a, 9001);
    wire_write(&mut child, &json!({"op": "watch", "src": "secret"})).await;
    let ev = wire_read(&mut child).await;
    assert_eq!(ev["op"], "event", "{ev}");
    assert_eq!(ev["src"], "secret", "{ev}");
    assert_eq!(ev["payload"]["reason"], "unauthorized", "{ev}");

    // …and NOTHING was registered. No proxy inbox row — and no WATCHER row on the
    // source either, which an emit proves from outside the crate: register the
    // proxy's inbox ourselves, and a leaked watcher fans the payload into it.
    let proxy = AgentId::from("_xbw_9001_secret");
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
    let (mut child, stop, relay) = wire(&kernel, &a, 9002);
    // `$self` is a baseline edge, so both of these are permitted.
    wire_write(&mut child, &json!({"op": "watch", "src": a.as_str()})).await;
    wire_write(&mut child, &json!({"op": "onmessage"})).await;

    let proxies = [
        AgentId::from(format!("_xbw_9002_{a}").as_str()),
        AgentId::from("_xbself_9002"),
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

/// The seal covers every verb but `reflect`/`connector`, MAIL
/// INCLUDED. The substrate's fanout reaches the program's `_xbself_` proxy BEFORE
/// the handler, so the relay's inbox drain drops what the gate refuses: a sealed
/// leg's program receives nothing, and receives again once the leg is reopened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sealed_leg_refuses_mail_and_relays_nothing() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let (mut child, stop, relay) = wire(&kernel, &a, 9006);
    wire_write(&mut child, &json!({"op": "onmessage"})).await;
    let proxy = AgentId::from("_xbself_9006");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !kernel.inboxes.contains_key(&proxy) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        kernel.inboxes.contains_key(&proxy),
        "the owner proxy never registered"
    );
    let rule = |r: &str| {
        Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "update_agent", "id": a.as_str(), "ingress_rule": r}),
        )
    };
    let up = kernel.send(vec![rule("deny_ingress")]).await.remove(0);
    assert_eq!(up.result["updated"], true, "{}", up.result);
    for p in [
        json!({"type": "status"}),
        json!({"type": "probe_mail", "x": "TEST-VALUE-MAIL"}),
    ] {
        let out = kernel
            .send(vec![Item::new(a.clone(), p.clone())])
            .await
            .remove(0)
            .result;
        assert_eq!(out["reason"], "unauthorized", "{p} → {out}");
    }
    kernel.send(vec![rule("allow_all_ingress")]).await;
    kernel
        .send(vec![Item::new(a.clone(), json!({"type": "after"}))])
        .await;
    // the first frame down the fd is the mail sent AFTER the reopen
    assert_eq!(
        wire_read(&mut child).await,
        json!({"op": "inbox", "payload": {"type": "after"}})
    );
    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not stop")
        .expect("relay");
}

/// The fanout mirrors every payload sent to the leg into the owner
/// inbox, so a program started by one caller saw another caller's `write`
/// keystrokes, paste bytes and drive calls aimed at that caller's job. A drive
/// verb whose int `job_id` names ANOTHER job is dropped; one with no `job_id`
/// (bulk) or naming this job is forwarded, and so is one whose id names no job
/// (a string). Other mail is unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_inbox_drops_a_drive_verb_naming_another_job() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let (mut child, stop, relay) = wire(&kernel, &a, 9008);
    wire_write(&mut child, &json!({"op": "onmessage"})).await;
    let proxy = AgentId::from("_xbself_9008");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !kernel.inboxes.contains_key(&proxy) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        kernel.inboxes.contains_key(&proxy),
        "the owner proxy never registered"
    );
    for p in [
        json!({"type": "write", "job_id": 9009, "data": "TEST-VALUE-KEYS\n"}),
        json!({"type": "resize", "job_id": 9009, "cols": 10, "rows": 5}),
        json!({"type": "paste_image", "job_id": 9009, "bytes": "TEST-VALUE-PNG"}),
        json!({"type": "stop", "job_id": 9009}),
        json!({"type": "interrupt", "job_id": 9009}),
        json!({"type": "clear", "job_id": 9009}),
        // the bulk forms name no job — forwarded
        json!({"type": "stop"}),
        json!({"type": "interrupt"}),
        json!({"type": "clear"}),
        json!({"type": "write", "job_id": "9008", "data": "str"}),
        json!({"type": "write", "job_id": 9008, "data": "own"}),
        json!({"type": "mail", "n": 1}),
    ] {
        kernel.send(vec![Item::new(a.clone(), p)]).await;
    }
    // every frame down the fd, in order: none of them names job 9009
    for want in [
        json!({"type": "stop"}),
        json!({"type": "interrupt"}),
        json!({"type": "clear"}),
        json!({"type": "write", "job_id": "9008", "data": "str"}),
        json!({"type": "write", "job_id": 9008, "data": "own"}),
        json!({"type": "mail", "n": 1}),
    ] {
        assert_eq!(
            wire_read(&mut child).await,
            json!({"op": "inbox", "payload": want})
        );
    }
    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not stop")
        .expect("relay");
}

/// Python `test_a_malformed_ctrl_frame_is_skipped_never_fatal` — JSON
/// that is no envelope, or a `src` that is no string, is skipped; the relay keeps
/// serving and the next call is answered. Python ended the relay task here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_ctrl_frame_is_skipped_never_fatal() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let (mut child, stop, relay) = wire(&kernel, &a, 9007);
    for junk in [
        json!([1]),
        json!("str"),
        json!({"op": "watch", "src": ["x"]}),
        json!({"op": "watch", "src": {"a": 1}}),
        json!({"op": "unwatch", "src": ["x"]}),
        json!({"op": "unwatch", "src": {"a": 1}}),
    ] {
        wire_write(&mut child, &junk).await;
    }
    wire_write(
        &mut child,
        &json!({"op": "send", "rid": "r1", "target": a.as_str(), "payload": {"type": "status"}}),
    )
    .await;
    let reply = wire_read(&mut child).await;
    assert_eq!(reply["op"], "reply", "{reply}");
    assert_eq!(reply["rid"], "r1", "{reply}");
    assert!(reply["data"]["jobs"].is_array(), "{reply}");
    assert!(!relay.is_finished(), "a malformed frame ended the relay");
    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not stop")
        .expect("relay");
}

/// A length prefix over the bound is a WIRE VIOLATION — the framing itself is
/// unusable, so there is nothing to reply TO and the socket closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_frame_closes_the_control_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let (mut child, _stop, relay) = wire(&kernel, &a, 9003);
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
    // The job's id IS its task id: `cancel` takes it as it is.
    let job_id = TaskId(r["job_id"].as_i64().expect("job_id"));
    let task_id = job_id.get();
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
            .get(&(a.clone(), job_id))
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
/// `teardown` → the relay's own cleanup. Aborting the relay task would drop
/// `serve` at its await point and skip the release entirely.
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
    let job_id = r["job_id"].as_i64().expect("job_id");
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

#[test]
fn a_typeless_rule_object_seals_like_python() {
    // python's `parse_spec` reads `{env: 'X'}` as no rule at all, which is a
    // plain SEAL — never an `{error}` without a `reason` nothing could branch on.
    let mut meta = Map::new();
    meta.insert("ingress_rule".into(), json!({"env": "X"}));
    let denied = gate(&meta, &AgentId::from("px"), "start").expect("a type-less rule seals");
    assert_eq!(denied["reason"], "unauthorized", "{denied}");
    assert_eq!(denied["error"], "edge sealed by policy");
    // …and the WRITE is accepted, as python accepts it.
    assert!(ExecBridgeBundle.validate_meta(&meta, &Map::new()).is_none());
}

#[test]
fn reflect_reports_egress_posture() {
    // Every io leg's reflect answers python's `describe`: the two rule
    // names and `sealed`, in that order, at the end of the reply.
    let kernel = Kernel::new();
    let id = AgentId::from("px");
    let root = fantastic_kernel::Agent::new(
        id.clone(),
        Some(HANDLER_MODULE.to_string()),
        None,
        Map::new(),
        std::path::PathBuf::new(),
        false,
    );
    let kernel = Arc::new(kernel);
    kernel.register(Arc::clone(&root));
    let r = reflect_reply(&id, &kernel);
    let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        &keys[keys.len() - 3..],
        ["ingress_rule", "egress_rule", "sealed"]
    );
    assert_eq!(r["ingress_rule"], "deny_ingress");
    assert_eq!(r["egress_rule"], "silent_egress");
    assert_eq!(r["sealed"], true);
}

// ─── python parity ───────────────────────────────────────────────
//
// Each test below pins a text, a coercion or an ordering to the canonical python
// (`PB/io/exec_bridge/src/exec_bridge/tools.py`). Where python's own behaviour is a
// quirk, the quirk is what is pinned.

/// The vendored readme is python's, byte for byte.
///
/// Skips (loudly) when the python tree is absent, like
/// `connector_matches_python_source`: the container's rust stage copies only
/// `src/lib/rust/`.
#[test]
fn readme_matches_python_source() {
    let py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4) // src/lib above crates/bundles/<crate>
        .expect("src/lib above the crate")
        .join("python/bundled_agents/io/exec_bridge/src/exec_bridge/readme.md");
    let Ok(src) = std::fs::read_to_string(&py) else {
        eprintln!("skipping: python tree not present at {}", py.display());
        return;
    };
    assert_eq!(src, README, "readme has drifted from {}", py.display());
}

/// Reflect's prose blocks are python's, verbatim.
#[tokio::test]
async fn docs_texts_match_python_verbatim() {
    let notes = ExecBridgeBundle.notes_doc().unwrap();
    assert_eq!(
        notes
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["wire", "inbox", "tasks"]
    );
    assert_eq!(
        notes["inbox"],
        "Any verb this bundle does not answer is MAIL for the running program, delivered to its on_message and ACKed {queued:true}. With no live job it is refused — a script that only defines something exits before its first message arrives."
    );
    assert_eq!(
        ExecBridgeBundle.emits_doc().unwrap()["progress"],
        "{type:'progress', job_id, n, line, stream} — one per output line"
    );
}

/// `verbs` is python's `VERBS` order, each entry the FIRST LINE of that
/// verb's docstring. Several are truncated mid-sentence; that is the python text.
#[tokio::test]
async fn verb_docs_are_python_first_docstring_lines() {
    let docs = ExecBridgeBundle.verb_docs();
    let verbs = docs.as_object().expect("verbs object");
    assert_eq!(
        verbs.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "start",
            "run",
            "status",
            "stop",
            "interrupt",
            "clear",
            "write",
            "resize",
            "ack",
            "output",
            "tail",
            "grep",
            "paste_image",
            "connector",
            "monitor",
            "monitors",
            "unmonitor",
            "reflect",
            "boot"
        ]
    );
    assert_eq!(
        verbs["start"],
        "args: command:str (req — the executable ALONE, e.g. `grep`; on a PTY start, absent = the child's $SHELL, and with no `args` the record's whole shell line: `$SHELL -l` under shell=\"login\"), args:list[str]? (its arguments, one item each — `<args><item>-rn</item><item>x</item></args>`), pty:{cols,rows}? (run on a terminal). The FULL line (command + args) must fullmatch one of the `allowed` regexes, or be the shell line the record's `shell` permits."
    );
    assert_eq!(
        verbs["run"],
        "args: command:str (req), args?, cwd?, wait_s:float? (default 60, max 600), tail:int? (default 200) — start, wait, and return the output in ONE reply: {job_id, status, exit_code, lines}. Use run for short commands; a job still running at wait_s comes back running (continue with status wait_s / tail). Read files with files.read, not run."
    );
    assert_eq!(
        verbs["clear"],
        "args: job_id:int? — drop finished jobs (one, or all non-running) from the RAM"
    );
    assert_eq!(
        verbs["boot"],
        "No-op. Nothing auto-restarts: a command is started by a caller, not by the record."
    );
    assert_eq!(
        verbs["reflect"],
        "Identity + running job count + auth posture. No args."
    );
}

/// Reflect reports the record's `cwd` as written (null when unset — there
/// is no process default any more) and the `inherit_env` it runs with; `allowed`
/// items go through `str()`.
#[tokio::test]
async fn reflect_coerces_like_python() {
    let tmp = TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    meta.insert("cwd".to_string(), json!("/wd"));
    meta.insert("inherit_env".to_string(), json!(["PATH"]));
    meta.insert("allowed".to_string(), json!(["ls", 7, true]));
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let r = reflect_reply(&a, &kernel);
    assert_eq!(r["cwd"], "/wd");
    assert_eq!(r["inherit_env"], json!(["PATH"]));
    // `str(7)` and `str(True)` — not JSON's `7`/`true`.
    assert_eq!(r["allowed"], json!(["ls", "7", "True"]));

    let tmp2 = TempDir::new().unwrap();
    let mut m2 = Map::new();
    m2.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    m2.insert("cwd".to_string(), Value::Null);
    let (k2, b) = mk_kernel_with(&tmp2, m2).await;
    let r2 = reflect_reply(&b, &k2);
    assert_eq!(r2["cwd"], Value::Null);
    assert_eq!(r2["inherit_env"], json!([]));
}

/// `shutdown` is not a verb here: python's `VERBS` has no such key, so it is
/// MAIL like any other unowned type — never a `null` that reads as a successful
/// lifecycle call.
#[tokio::test]
async fn shutdown_is_mail_not_a_verb() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "shutdown"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r["error"],
        "exec_bridge: unknown type 'shutdown' (no running job to receive it)"
    );
}

/// Every refusal quotes with python's `repr`, and an ABSENT `type` is `None`.
#[tokio::test]
async fn refusal_texts_use_python_repr() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    // `unknown type {t!r}` over the RAW value — an ABSENT type is `None`. Driven
    // through the bundle directly: `kernel.send` refuses a typeless payload before
    // any handler sees it.
    let r = fantastic_kernel::bundle::Bundle::handle(&ExecBridgeBundle, &a, &json!({}), &kernel)
        .await
        .expect("handle")
        .expect("reply");
    assert_eq!(
        r["error"],
        "exec_bridge: unknown type None (no running job to receive it)"
    );
    // `no job {jid!r}`.
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "status", "job_id": 424_242}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], "exec_bridge: no job 424242");
    assert_eq!(r["reason"], "not_found");
    // `command not allowed: {line!r} — this leg permits: {pats}` — the line repr'd,
    // the pattern list printed as python prints a `list[str]`.
    let mut meta = Map::new();
    meta.insert("allowed".to_string(), json!(["ls( .*)?"]));
    assert_eq!(
        command_permitted(&meta, "rm -rf /").expect("unlisted"),
        "exec_bridge: command not allowed: 'rm -rf /' — this leg permits: ['ls( .*)?']"
    );
}

/// `validate_meta`'s texts are python's up to the engine tail after `': '`
/// (that tail is the regex engine's own).
#[test]
fn validate_meta_texts_match_python() {
    let b = ExecBridgeBundle;
    let mut empty = Map::new();
    empty.insert("allowed".to_string(), json!([]));
    assert_eq!(
        b.validate_meta(&empty, &Map::new()).expect("refused"),
        "allowed must be a non-empty list of regexes, e.g. <allowed><item>ls( .*)?</item></allowed>"
    );
    let mut broken = Map::new();
    broken.insert("allowed".to_string(), json!(["("]));
    let e = b.validate_meta(&broken, &Map::new()).expect("refused");
    assert!(
        e.starts_with("allowed contains a broken regex '(': "),
        "{e}"
    );
    // The spec is repr'd, not JSON-printed.
    let mut pw = Map::new();
    pw.insert("auth".to_string(), json!("password"));
    assert_eq!(
        b.validate_meta(&pw, &Map::new()).expect("refused"),
        "auth='password' is not applicable to the process edge: a leg with no wire \
         envelope can never present a token — legal values: allow_all_ingress, deny_ingress"
    );
}

/// Only an UNKNOWN RULE NAME is refused. A falsy spec and a type-less rule
/// object are a plain seal (python resolves them to `DenyInbound()` with no
/// `unknown`), and `password_ingress` resolves fine here — it is the stem check that
/// refuses it on this edge.
#[test]
fn validate_meta_refuses_only_unknown_ingress_rules() {
    let b = ExecBridgeBundle;
    for silent in [json!(""), json!(null), json!({"env": "X"})] {
        let mut meta = Map::new();
        meta.insert("ingress_rule".to_string(), silent.clone());
        assert!(
            b.validate_meta(&meta, &Map::new()).is_none(),
            "{silent} must seal silently"
        );
    }
    let mut bogus = Map::new();
    bogus.insert("ingress_rule".to_string(), json!({"type": "nope"}));
    assert_eq!(
        b.validate_meta(&bogus, &Map::new()).expect("refused"),
        "ingress_rule={'type': 'nope'} is not a rule this bundle implements — \
         legal values: allow_all_ingress, deny_ingress"
    );
}

/// `allowed` is compiled in PYTHON's `re` dialect: named groups,
/// backreferences and lookaround all work. `regex` rejects every one of them at
/// compile time, which silently turned a working python policy into a leg that
/// permitted nothing.
#[test]
fn allowed_accepts_python_regex_syntax() {
    let b = ExecBridgeBundle;
    for pat in [
        r"(?P<exe>ls)( .*)?",
        r"(?P<q>['\x22])x(?P=q)",
        r"echo (?!secret).*",
        r"(\w+) \1",
    ] {
        let mut meta = Map::new();
        meta.insert("allowed".to_string(), json!([pat]));
        assert!(
            b.validate_meta(&meta, &Map::new()).is_none(),
            "python accepts {pat}"
        );
    }
    let mut named = Map::new();
    named.insert("allowed".to_string(), json!([r"(?P<exe>ls)( .*)?"]));
    assert!(command_permitted(&named, "ls -la").is_none());
    assert!(command_permitted(&named, "rm").is_some());
    let mut backref = Map::new();
    backref.insert("allowed".to_string(), json!([r"(\w+) \1"]));
    assert!(command_permitted(&backref, "go go").is_none());
    assert!(command_permitted(&backref, "go stop").is_some());
}

/// `job_id` is an INT or absent (python `_job_arg`): an absent key or `null` means
/// ALL of this agent's jobs; any other non-int — `""`, `[]`, a string, a float — is
/// a bad argument, never a silent "all"; an int that names nothing is not found,
/// echoed raw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_id_is_an_int_or_absent() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "status", "job_id": null}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["jobs"], json!([]), "null job_id means ALL: {r}");
    for not_int in [json!(""), json!([]), json!("7"), json!(7.5), json!(true)] {
        let r = kernel
            .send(vec![Item::new(
                a.clone(),
                json!({"type": "status", "job_id": not_int}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(
            (&r["reason"], &r["arg"]),
            (&json!("bad_arg"), &json!("job_id")),
            "{not_int}: {r}"
        );
    }
    // A non-finite float — the kernel's `Infinity` token, what a model's
    // `<job_id>458e5912</job_id>` becomes — is not an int either.
    let mut inf = Map::new();
    inf.insert("type".to_string(), json!("status"));
    inf.insert(
        "job_id".to_string(),
        fantastic_kernel::coerce_scalar("458e5912"),
    );
    let r = kernel
        .send(vec![Item::new(a.clone(), Value::Object(inf))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "status", "job_id": 0}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "not_found", "0 is an id, not ALL: {r}");
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "stop", "job_id": 7}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], "exec_bridge: no job 7");
    assert_eq!(r["job_id"], json!(7), "the raw value is echoed: {r}");
}

/// `start` coerces exactly as python does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_coerces_args_env_and_cwd_like_python() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let call = |p: Value| {
        let kernel = Arc::clone(&kernel);
        let a = a.clone();
        async move { kernel.send(vec![Item::new(a, p)]).await.remove(0).result }
    };
    // A command of nothing but whitespace is MISSING, not a spawn of "".
    let r = call(json!({"type": "start", "command": "  \t "})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert_eq!(r["arg"], "command", "{r}");
    // An explicit null `args` is not a list.
    let r = call(json!({"type": "start", "command": "ls", "args": null})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "args", "{r}");
    // `env` and `cwd` are the RECORD's: any per-call value refuses (py `is not
    // None`), even an empty one; only an explicit null is "absent".
    let Some(py) = find_real_python() else {
        return;
    };
    for (arg, val) in [
        ("env", json!({})),
        ("env", json!(["A=1"])),
        ("cwd", json!("/")),
    ] {
        let r = call(json!({"type": "start", "command": "ls", arg: val})).await;
        assert_eq!(
            (&r["reason"], &r["arg"]),
            (&json!("bad_arg"), &json!(arg)),
            "{r}"
        );
    }
    // A wake-up is a monitor's: `notify`/`match`/`every` on `start` refuse naming
    // `monitor`, never dropped silently.
    for (arg, val) in [
        ("notify", json!("ai")),
        ("match", json!("WAKE")),
        ("every", json!(5)),
    ] {
        let r = call(json!({"type": "start", "command": "ls", arg: val})).await;
        assert_eq!(
            (&r["reason"], &r["arg"]),
            (&json!("bad_arg"), &json!(arg)),
            "{r}"
        );
        assert!(
            r["error"].as_str().unwrap_or("").contains("`monitor` it"),
            "{r}"
        );
    }
    let r = call(json!({"type": "start", "command": py.to_string_lossy(),
                        "args": ["-c", "pass"], "env": null, "cwd": null}))
    .await;
    assert!(r["job_id"].is_i64(), "an explicit null is absent: {r}");
    // Bools are ints in python, so they are legal argv items — as `True`/`False`.
    let r = call(json!({"type": "start", "command": py.to_string_lossy(),
                        "args": ["-c", "import sys; print(sys.argv[1:])", true, 2, 1.5]}))
    .await;
    assert!(r["job_id"].is_i64(), "{r}");
    let st = call(json!({"type": "status", "job_id": r["job_id"].clone()})).await;
    assert_eq!(
        st["jobs"][0]["args"],
        json!(["-c", "import sys; print(sys.argv[1:])", "True", "2", "1.5"])
    );
}

/// `status` lists this agent's jobs in START order (python's dict is
/// insertion-ordered).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_lists_jobs_in_start_order() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut started = Vec::new();
    for _ in 0..3 {
        let r = kernel
            .send(vec![Item::new(
                a.clone(),
                py_start(&py, "import time; time.sleep(30)", false),
            )])
            .await
            .remove(0)
            .result;
        started.push(r["job_id"].as_i64().expect("job_id"));
    }
    let st = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    let listed: Vec<i64> = st["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .filter_map(|j| j["job_id"].as_i64())
        .collect();
    assert_eq!(listed, started, "status must list jobs in start order");
    let _ = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

/// The id in a `status` snapshot is an INT — the job's task id, the
/// number `cancel task_id=<id>` takes, not a quoted copy of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_job_id_is_the_integer_task_id() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(&py, "import time; time.sleep(30)", false),
        )])
        .await
        .remove(0)
        .result;
    let st = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert!(
        st["jobs"][0]["job_id"].is_i64(),
        "job_id must be an int: {st}"
    );
    let _ = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

/// A NAMED `clear` follows the bare form's rule: a `running` (or `stopping`) row
/// is refused — the row is the only handle `stop`, `cancel`, `delete` and
/// shutdown kill a process through, and without it the process would outlive all
/// four. Once the job is finished, the named clear drops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_clear_refuses_a_running_job() {
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
    let jid = r["job_id"].as_i64().expect("job_id");
    let clear = || async {
        kernel
            .send(vec![Item::new(
                a.clone(),
                json!({"type": "clear", "job_id": jid}),
            )])
            .await
            .remove(0)
            .result
    };
    // A BARE clear leaves it — it is running.
    let bare = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "clear"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(bare["cleared"], 0, "a running row is not swept: {bare}");
    let named = clear().await;
    assert_eq!(
        serde_json::to_string(&named).unwrap(),
        format!(
            "{{\"error\":\"clear: job_id must be a finished job (job {jid} is running — \
             `stop` it, then clear)\",\"reason\":\"bad_arg\",\"arg\":\"job_id\"}}"
        )
    );
    assert!(
        jobs::jobs().contains_key(&(a.clone(), TaskId(jid))),
        "the row stays"
    );
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "stop", "job_id": jid}),
        )])
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let done = jobs::jobs()
            .get(&(a.clone(), TaskId(jid)))
            .is_some_and(|j| j.status == "stopped");
        if done || tokio::time::Instant::now() > deadline {
            assert!(done, "the stopped row never left `stopping`");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(clear().await, json!({"cleared": 1}));
    let st = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["jobs"], json!([]));
}

/// Output is collected RAW: the newline stays on each line, a lone `\r` is
/// data (`rstrip("\n")` only), a final line with no newline gets none added, and an
/// invalid byte is REPLACED while the pipe keeps draining.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_is_collected_raw_like_python() {
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
                "import sys\nsys.stdout.buffer.write(b'a\\r\\nb\\xff')\n",
                false,
            ),
        )])
        .await
        .remove(0)
        .result;
    let done = until(&mut rx, |v| v["type"] == "job_done", 20)
        .await
        .expect("no job_done");
    // the ring keeps LINES: `\r` stays, a last line without `\n` gets one
    assert_eq!(
        json!(crate::jobs::printed(&a, &done, "stdout")),
        "a\r\nb\u{fffd}\n",
        "{done}"
    );
}

/// A line past asyncio's 64 KiB default is read whole: python raises the
/// StreamReader limit to 16 MiB, so a program's long line does not end the pipe
/// with `[pump error]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_overlong_line_is_read_whole() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(&py, "print('x' * 200000)\n", false),
        )])
        .await
        .remove(0)
        .result;
    let done = until(&mut rx, |v| v["type"] == "job_done", 25)
        .await
        .expect("no job_done");
    let out = crate::jobs::printed(&a, &done, "stdout");
    let out = out.as_str();
    assert!(
        out.starts_with(&"x".repeat(200_000)),
        "stdout len {}",
        out.len()
    );
    assert_eq!(json!(crate::jobs::printed(&a, &done, "stderr")), "");
    assert!(done.get("error").is_none(), "{done}");
}

/// A signal death reports `exit_code = -signal`, `ok:false`. `ExitStatus::
/// code()` answers `None` for a signal, which read as "still running" everywhere
/// liveness is `exit_code.is_none()` and lost the cause of death from `job_done`.
/// (Windows has no signals: a killed job reports the code its Job Object was
/// terminated with — `sys::KILLED_EXIT_CODE`, `-9` on unix.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_job_reports_negative_signal_exit_code() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(&py, "import time; time.sleep(30)", false),
        )])
        .await
        .remove(0)
        .result;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
    let done = until(&mut rx, |v| v["type"] == "job_done", 20)
        .await
        .expect("no job_done");
    assert_eq!(done["exit_code"], crate::sys::KILLED_EXIT_CODE, "{done}");
    assert_eq!(done["ok"], false, "{done}");
}

/// Liveness is "the OS process has not exited", not "the pipes reached
/// EOF". A program that closes stdout/stderr and keeps running is still LIVE: its
/// mail is queued and reflect counts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_is_process_exit_not_pipe_eof() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(
                &py,
                "import os, sys, time\nsys.stdout.flush()\nos.close(1)\nos.close(2)\ntime.sleep(4)\n",
                false,
            ),
        )])
        .await
        .remove(0)
        .result;
    // Give both pipes time to hit EOF while the process is still sleeping.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        reflect_reply(&a, &kernel)["running"],
        1,
        "reflect counts a live process whose pipes are closed"
    );
    let mail = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "ping"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(mail["queued"], true, "mail must reach a live job: {mail}");
    let st = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["jobs"][0]["exit_code"], json!(null), "{st}");
    let _ = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

/// A spawn failure reads exactly as python's `OSError`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_failure_text_matches_python() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "start", "command": "nope"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(
        r["error"],
        "exec_bridge: spawn failed: [Errno 2] No such file or directory: 'nope'"
    );
    assert_eq!(r["reason"], "transport_error");
}

/// A MISSING `cwd` is named, not the command — CPython's `noexec:chdir` text.
/// `No such file or directory: 'python3'` for a leg whose cwd does not exist
/// would send the model hunting an interpreter that is there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_cwd_is_named_not_the_command() {
    let tmp = TempDir::new().unwrap();
    let gone = tmp.path().join("no_such_dir");
    let mut extra = Map::new();
    extra.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    extra.insert("allowed".to_string(), json!([".*"]));
    extra.insert("cwd".to_string(), json!(gone.to_string_lossy()));
    let (kernel, a) = mk_kernel_with(&tmp, extra).await;
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "start", "command": "/bin/echo", "args": ["x"]}),
        )])
        .await
        .remove(0)
        .result;
    // `[Errno 2] No such file or directory` — chdir's ENOENT; on Windows
    // CreateProcessW's ERROR_DIRECTORY, `[Errno 20] Not a directory`
    assert_eq!(
        r["error"],
        json!(format!(
            "exec_bridge: spawn failed: {}",
            fantastic_bundle::os_error_str(
                crate::sys::MISSING_CWD_ERRNO,
                Some(&gone.to_string_lossy())
            )
        )),
        "{r}"
    );
    assert_eq!(r["reason"], "transport_error");
}

/// The two control vars are written OVER the record's `env`, so a program
/// cannot claim another agent's identity or point the control fd somewhere else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_env_cannot_override_the_control_vars() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    meta.insert("allowed".to_string(), json!([".*"]));
    // the operator's record `env` cannot claim the control vars either
    meta.insert(
        "env".to_string(),
        json!({"FANTASTIC_SELF_ID": "root", "FANTASTIC_CTRL_FD": "0", "MINE": "kept"}),
    );
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let mut rx = inbox(&kernel, &a);
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({
                "type": "start",
                "command": py.to_string_lossy(),
                "args": ["-u", "-c", "import os; print('ID', os.environ['FANTASTIC_SELF_ID'], 'FD', os.environ['FANTASTIC_CTRL_FD'])"],
            }),
        )])
        .await
        .remove(0)
        .result;
    let done = until(&mut rx, |v| v["type"] == "job_done", 25)
        .await
        .expect("no job_done");
    let out = crate::jobs::printed(&a, &done, "stdout");
    let out = out.as_str();
    assert!(out.contains(&format!("ID {a} ")), "{done}");
    assert!(
        !out.contains("FD 0"),
        "the caller chose the control fd: {done}"
    );
}

/// The ingress gate never takes a credential from the RECORD. A token is a
/// wire-envelope fact and the process edge has no envelope; reading one off the
/// record would let a leg satisfy its own password rule with a secret it stores.
#[test]
fn the_record_auth_token_is_never_a_credential() {
    let env = "FANTASTIC_GROUP_TOKEN_EXEC_A4_16";
    std::env::set_var(env, "s3cret");
    let mut meta = Map::new();
    meta.insert(
        "ingress_rule".to_string(),
        json!({"type": "password_ingress", "env": env}),
    );
    meta.insert("auth_token".to_string(), json!("s3cret"));
    let denied = gate(&meta, &AgentId::from("px"), "start")
        .expect("a record-held token must not satisfy the rule");
    assert_eq!(denied["reason"], "unauthorized", "{denied}");
    assert_eq!(
        denied["error"], "invalid or missing group token",
        "{denied}"
    );
    std::env::remove_var(env);
}

/// The relay coerces like python: a FALSY payload becomes `{}` and a FALSY
/// emit target becomes the OWNER (the one-arg `emit(payload)` form).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_coerces_falsy_payload_and_target() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut owner_rx = inbox(&kernel, &a);
    let (mut child, stop, relay) = wire(&kernel, &a, 9004);
    // A null payload is dispatched as `{}`, not as null.
    wire_write(
        &mut child,
        &json!({"op": "send", "rid": 1, "target": a.as_str(), "payload": null}),
    )
    .await;
    let reply = wire_read(&mut child).await;
    assert_eq!(reply["op"], "reply", "{reply}");
    // The kernel refuses a typeless payload, and WHICH refusal says what it got: a
    // non-object is `send: payload required`, an empty OBJECT `send: payload.type
    // required`. The second one is the proof that `null` became `{}`.
    assert_eq!(reply["data"]["arg"], "payload.type", "{reply}");

    // An emit lands on the OWNER; a falsy payload is `{}`.
    wire_write(&mut child, &json!({"op": "emit"})).await;
    let ev = until(&mut owner_rx, |_| true, 5)
        .await
        .expect("no emit reached the owner");
    assert_eq!(ev, json!({}), "a falsy payload must become {{}}: {ev}");
    // A `target` names nothing: the owner is the only place a program emits.
    wire_write(
        &mut child,
        &json!({"op": "emit", "target": "elsewhere", "payload": {"type": "ping"}}),
    )
    .await;
    let ev = until(&mut owner_rx, |v| v["type"] == "ping", 5)
        .await
        .expect("the emit did not land on the owner");
    assert_eq!(ev["type"], "ping");

    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not stop")
        .expect("relay");
}

/// A program's subscriptions hold KERNEL TASKS: `watch` forks a
/// `watch_drain`, `on_message` an `inbox_drain`, so `ps` lists them and `cancel`
/// reaches them — never detached `tokio::spawn`s, invisible and unkillable.
///
/// Each fork is also a MESSAGE: its task id IS the id of a `task` row
/// on the runner, so `rows_read` explains a `ps` row that has already ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watching_program_holds_drain_task_rows() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let (mut child, stop, relay) = wire(&kernel, &a, 9005);
    wire_write(&mut child, &json!({"op": "watch", "src": a.as_str()})).await;
    wire_write(&mut child, &json!({"op": "onmessage"})).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut verbs: Vec<String> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        verbs = kernel
            .tasks
            .rows()
            .iter()
            .filter_map(|r| r["verb"].as_str().map(str::to_string))
            .collect();
        if verbs.iter().any(|v| v == "watch_drain") && verbs.iter().any(|v| v == "inbox_drain") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(verbs.iter().any(|v| v == "watch_drain"), "{verbs:?}");
    assert!(verbs.iter().any(|v| v == "inbox_drain"), "{verbs:?}");
    // Each is OWNED by the runner, so `ps` attributes it and `cancel` can reach it.
    for row in kernel.tasks.rows() {
        if row["verb"] == "watch_drain" || row["verb"] == "inbox_drain" {
            assert_eq!(row["agent"], a.as_str(), "{row}");
            assert!(row["id"].is_i64(), "{row}");
        }
    }

    // And each one is a ROW, under the id `ps` reported.
    let store: Vec<_> = fantastic_kernel::test_support::rows_about(&kernel, &a)
        .into_iter()
        .filter(|r| r.get("kind") == Some(&json!("task")))
        .collect();
    for verb in ["watch_drain", "inbox_drain"] {
        let row = store
            .iter()
            .find(|r| r.get("verb") == Some(&json!(verb)))
            .unwrap_or_else(|| panic!("no `{verb}` task row: {store:?}"));
        let id = row["id"].as_i64().expect("a row id");
        assert!(
            kernel.tasks.rows().iter().any(|t| t["id"] == json!(id)),
            "the task id is the row id for {verb}"
        );
    }

    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not stop")
        .expect("relay");
    // …and the release after the read loop takes both rows back out.
    let left: Vec<String> = kernel
        .tasks
        .rows()
        .iter()
        .filter_map(|r| r["verb"].as_str().map(str::to_string))
        .filter(|v| v == "watch_drain" || v == "inbox_drain")
        .collect();
    assert!(left.is_empty(), "drain rows leaked past teardown: {left:?}");
}

/// python `secrets.token_hex(4)` — 8 lowercase hex characters from the OS
/// CSPRNG, naming a pasted image's scratch file (a JOB's id is its task id).
#[test]
fn paste_names_are_pythons_token_hex_4_and_random() {
    let ids: Vec<String> = (0..64).map(|_| crate::rand_hex8()).collect();
    for id in &ids {
        assert_eq!(id.len(), 8, "{id}");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{id}"
        );
    }
    let uniq: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(uniq.len(), ids.len(), "job ids repeated: {ids:?}");
    let mut sorted = ids.clone();
    sorted.sort();
    assert_ne!(sorted, ids, "job ids ascend like a counter: {ids:?}");
}

/// python `_serve_ctrl` relays a program's `send` UNTOUCHED — one
/// `kernel.send([{target, payload}])`: no `_call_stack` tag, no cycle/depth
/// refusal of its own. A kind-B frame is the SAME batch item carrying its body:
/// a pid and a call row like any other execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_forwards_a_send_untouched_and_a_binary_one_through_the_batch() {
    use tokio::io::AsyncWriteExt;
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let tap = AgentId::from("tap_relay");
    let mut tap_rx = kernel.ensure_inbox(&tap);
    assert!(kernel.watch(&a, tap.clone()).await.is_none());
    let (mut child, stop, relay) = wire(&kernel, &a, 9006);

    // A program calling its OWN runner — a "cycle" by the ai_core guard's
    // measure — is relayed like any send.
    wire_write(
        &mut child,
        &json!({"op": "send", "rid": 1, "target": a.as_str(), "payload": {"type": "status"}}),
    )
    .await;
    let reply = wire_read(&mut child).await;
    assert_eq!(reply["rid"], 1, "{reply}");
    assert!(reply["data"].get("error").is_none(), "{reply}");
    let mirrored = tap_rx.try_recv().expect("the send was mirrored");
    assert_eq!(
        mirrored.payload,
        json!({"type": "status"}),
        "relayed untouched"
    );

    // Kind B: `[len | 'B' | io_bridge binary frame]`.
    let head = json!({"op": "send", "rid": 2, "target": a.as_str(),
                      "payload": {"type": "status", "bytes": null}});
    let path = [
        fantastic_kernel::Step::from("payload"),
        fantastic_kernel::Step::from("bytes"),
    ];
    let frame = fantastic_io_bridge::codec::encode_binary_frame(&head, &path, b"xyz");
    let body = [b"B".as_slice(), &frame].concat();
    child
        .write_all(&[&(body.len() as u32).to_be_bytes()[..], &body].concat())
        .await
        .expect("write");
    let reply = wire_read(&mut child).await;
    assert_eq!(reply["rid"], 2, "{reply}");
    let row = kernel
        .rows
        .all()
        .into_iter()
        .rev()
        .find(|r| r.get("kind") == Some(&json!("call")) && r.get("verb") == Some(&json!("status")))
        .expect("the binary send is a ledgered call");
    assert_eq!(row["payload"]["bytes"], json!({"len": 3}), "{row:?}");

    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .expect("relay did not stop")
        .expect("relay");
}

/// With no running job the unknown type is a BARE
/// `{error}` carrying the "no running job" tail; with one it is mail, queued.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "frob"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"exec_bridge: unknown type 'frob' (no running job to receive it)","reason":"unhandled","verb":"frob"}"#
    );
    let r = fantastic_kernel::bundle::Bundle::handle(&ExecBridgeBundle, &a, &json!({}), &kernel)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        r#"{"error":"exec_bridge: unknown type None (no running job to receive it)","reason":"unhandled","verb":null}"#
    );
}

/// The three `arg` refusals carry `error, reason, hint,
/// arg` IN THAT ORDER (key order is wire — `Value` equality ignores it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arg_refusals_are_byte_exact_in_key_order() {
    let ask = |k: Arc<Kernel>, a: AgentId, p: Value| async move {
        let r = k.send(vec![Item::new(a, p)]).await.remove(0).result;
        serde_json::to_string(&r).unwrap()
    };
    let open = || {
        let mut extra = Map::new();
        extra.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
        extra.insert("allowed".to_string(), json!([".*"]));
        extra
    };
    // no PATH in the child env for a bare command
    let tmp = TempDir::new().unwrap();
    let (k, a) = mk_kernel_with(&tmp, open()).await;
    assert_eq!(
        ask(
            Arc::clone(&k),
            a.clone(),
            json!({"type": "start", "command": "echo"})
        )
        .await,
        format!(
            "{{\"error\":\"exec_bridge: 'echo' is a bare command and this leg gives the child \
             no PATH to find it in — the kernel's own PATH is not the child's\",\
             \"reason\":\"missing_arg\",\"hint\":\"update_agent {a} inherit_env=[\\\"PATH\\\"] \
             (or env={{\\\"PATH\\\": …}}), or start an absolute path\",\"arg\":\"PATH\"}}"
        )
    );
    // a PTY start with no command and no SHELL (the OS's shell var) in the child env
    let v = crate::SHELL_VAR;
    assert_eq!(
        ask(
            Arc::clone(&k),
            a.clone(),
            json!({"type": "start", "pty": {"cols": 80, "rows": 24}})
        )
        .await,
        format!(
            "{{\"error\":\"exec_bridge: start named no command and the child's environment has \
             no {v} to run — a PTY start without `command` runs the shell the record passes \
             down, never a guessed one\",\"reason\":\"missing_arg\",\"hint\":\"update_agent {a} \
             inherit_env=[\\\"{v}\\\"] (or env={{\\\"{v}\\\": …}}), or name `command`\",\
             \"arg\":\"{v}\"}}"
        )
    );
    // a leg with no cwd
    let tmp2 = TempDir::new().unwrap();
    let mut extra = open();
    extra.insert("cwd".to_string(), Value::Null);
    let (k2, b) = mk_kernel_with(&tmp2, extra).await;
    assert_eq!(
        ask(
            k2,
            b.clone(),
            json!({"type": "start", "command": "/bin/sh", "args": ["-c", "true"]})
        )
        .await,
        format!(
            "{{\"error\":\"exec_bridge: this leg names no `cwd` — where a command runs is the \
             operator's decision, never guessed\",\"reason\":\"missing_arg\",\
             \"hint\":\"update_agent {b} cwd=<dir>\",\"arg\":\"cwd\"}}"
        )
    );
}

// `_binary_path` as a list of steps on the guest wire — python
// `test_binary_path_steps.py` (e)'s twins.
#[path = "tests_binary_steps.rs"]
mod tests_binary_steps;

/// `tail` / `grep` read a RUNNING pipe job's output so far:
/// `status` holds stdout/stderr only once the job ends. Order holds within a
/// stream only (stdout and stderr are read concurrently).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tail_and_grep_read_a_running_job() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    let started = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(
                &py,
                "import sys, time\nfor i in range(6):\n    print(f'step {i} ok', flush=True)\n\
                 print('ERROR disk full', file=sys.stderr, flush=True)\nprint('step 6 ok', flush=True)\n\
                 time.sleep(30)\n",
                false,
            ),
        )])
        .await
        .remove(0)
        .result;
    let jid = started["job_id"].clone();
    let _ = until(&mut rx, |v| v["type"] == "progress" && v["n"] == 8, 20)
        .await
        .expect("8 lines never printed");
    let ask = |p: Value| {
        let k = Arc::clone(&kernel);
        let a = a.clone();
        async move { k.send(vec![Item::new(a, p)]).await.remove(0).result }
    };
    let t = ask(json!({"type": "tail", "job_id": jid, "lines": 2, "stream": "stdout"})).await;
    assert_eq!(t["status"], "running", "{t}");
    assert_eq!(t["printed"], 8, "{t}");
    let lines: Vec<&str> = t["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["line"].as_str().unwrap())
        .collect();
    assert_eq!(lines, ["step 5 ok", "step 6 ok"], "{t}");
    let g = ask(json!({"type": "grep", "job_id": jid, "match": "ERROR|step [45]"})).await;
    assert_eq!(g["total"], 3, "{g}");
    assert_eq!(g["scanned"], 8, "{g}");
    let bad = ask(json!({"type": "grep", "job_id": jid, "match": "("})).await;
    assert_eq!(bad["arg"], "match", "{bad}");
    let bad = ask(json!({"type": "tail", "job_id": jid, "lines": 0})).await;
    assert_eq!(bad["arg"], "lines", "{bad}");
    let _ = ask(json!({"type": "stop", "job_id": jid})).await;
}

/// A broken `allowed` pattern permits nothing, even one the fullmatch wrapper
/// would balance into match-everything (python `re.fullmatch` raises).
/// `status` with `wait_s` returns once the job is no longer running — no polling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_wait_returns_when_the_job_ends() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let started = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(&py, "import time; time.sleep(0.5)", false),
        )])
        .await
        .remove(0)
        .result;
    let jid = started["job_id"].as_i64().expect("job_id");
    let t0 = std::time::Instant::now();
    let st = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "status", "job_id": jid, "wait_s": 10}),
        )])
        .await
        .remove(0)
        .result;
    let took = t0.elapsed();
    assert_ne!(st["jobs"][0]["status"], "running", "{st}");
    assert!(took < std::time::Duration::from_secs(5), "took {took:?}");
}

/// A `wait_s` shorter than the job's life gives up at the deadline and still
/// reports the job running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_wait_gives_up_at_the_deadline() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let started = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(&py, "import time; time.sleep(30)", false),
        )])
        .await
        .remove(0)
        .result;
    let jid = started["job_id"].as_i64().expect("job_id");
    let t0 = std::time::Instant::now();
    let st = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "status", "job_id": jid, "wait_s": 0.5}),
        )])
        .await
        .remove(0)
        .result;
    let took = t0.elapsed();
    assert_eq!(st["jobs"][0]["status"], "running", "{st}");
    assert!(took < std::time::Duration::from_secs(3), "took {took:?}");
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "stop", "job_id": jid}),
        )])
        .await
        .remove(0)
        .result;
}

/// A `wait_s` that is not a number in 0..=120, or has no job to wait for, is a
/// bad argument.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_wait_refuses_a_bad_value() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    // a real job, so the range and type checks are what refuse — not the
    // missing job_id
    let started = kernel
        .send(vec![Item::new(
            a.clone(),
            py_start(&py, "import time; time.sleep(30)", false),
        )])
        .await
        .remove(0)
        .result;
    let jid = started["job_id"].as_i64().expect("job_id");
    for bad in [json!(-1), json!(500), json!("x")] {
        let r = kernel
            .send(vec![Item::new(
                a.clone(),
                json!({"type": "status", "job_id": jid, "wait_s": bad}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], "bad_arg", "{r}");
        assert_eq!(r["arg"], "wait_s", "{r}");
    }
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "status", "wait_s": 1}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "wait_s", "{r}");
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "stop", "job_id": jid}),
        )])
        .await
        .remove(0)
        .result;
}

#[test]
fn an_unbalanced_allowed_pattern_permits_nothing() {
    let mut meta = Map::new();
    meta.insert("allowed".into(), json!(["ls)|(?:.*"]));
    assert!(command_permitted(&meta, "rm -rf /").is_some());
    assert!(command_permitted(&meta, "ls").is_some());
}

/// `run` — start, wait, output in ONE reply: a short program comes back done,
/// with its exit code and its printed lines.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_returns_the_output_in_one_reply() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let program = format!("{LF_STDIO}import sys; print(\"hello\"); sys.exit(3)");
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({
                "type": "run",
                "command": py.to_string_lossy(),
                "args": ["-u", "-c", program],
                "wait_s": 30,
            }),
        )])
        .await
        .remove(0)
        .result;
    let jid = r["job_id"].as_i64().expect("job_id");
    assert_ne!(r["status"], "running", "{r}");
    assert_eq!(r["exit_code"], 3, "{r}");
    assert!(
        r["lines"]
            .as_array()
            .map(|l| l
                .iter()
                .any(|row| row["line"].as_str().is_some_and(|s| s.contains("hello"))))
            .unwrap_or(false),
        "{r}"
    );
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "clear", "job_id": jid}),
        )])
        .await;
}

/// A `wait_s` shorter than the job's life gives up at the deadline and answers
/// `running` with the job_id, so the caller can keep watching it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_returns_running_at_the_deadline() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let program = format!("{LF_STDIO}import time; time.sleep(30)");
    let t0 = std::time::Instant::now();
    let r = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({
                "type": "run",
                "command": py.to_string_lossy(),
                "args": ["-u", "-c", program],
                "wait_s": 0.5,
            }),
        )])
        .await
        .remove(0)
        .result;
    let took = t0.elapsed();
    let jid = r["job_id"].as_i64().expect("job_id");
    assert_eq!(r["status"], "running", "{r}");
    assert!(took < Duration::from_secs(3), "took {took:?}");
    let _ = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "stop", "job_id": jid}),
        )])
        .await;
}

/// `run` refuses a `wait_s` out of 0..=600 or not a number, and a `tail` below 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_refuses_bad_arguments() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let base = json!({
        "type": "run",
        "command": py.to_string_lossy(),
        "args": ["-u", "-c", "print(1)"],
    });
    for (arg, bad) in [
        ("wait_s", json!(700)),
        ("wait_s", json!("x")),
        ("tail", json!(0)),
    ] {
        let mut payload = base.clone();
        payload
            .as_object_mut()
            .unwrap()
            .insert(arg.to_string(), bad);
        let r = kernel
            .send(vec![Item::new(a.clone(), payload)])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], "bad_arg", "{r}");
        assert_eq!(r["arg"], arg, "{r}");
    }
}
