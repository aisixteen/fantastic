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
use fantastic_kernel::Parse;
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

pub(crate) use crate::test_support::started;

/// A kernel + one exec_bridge agent with a permissive `allowed=[".*"]` (the
/// operator opening the leg to any command). The `can` gate and the allowlist
/// refusal are covered by dedicated tests.
async fn mk_kernel(tmp: &TempDir) -> (Arc<Kernel>, AgentId) {
    let mut extra = Map::new();
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
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
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
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    (kernel, AgentId::from(pid.as_str()))
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
fn validate_meta_refuses_a_broken_allowed_regex() {
    let b = ExecBridgeBundle;
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

/// The record's declaration judges what a type can say: `allowed` is a non-empty list
/// of text, `cwd` non-empty text, `env` a map of scalars, `inherit_env` a list of names.
#[tokio::test]
async fn the_record_declaration_shapes_the_fields() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    for (field, value, arg) in [
        ("allowed", json!([]), "allowed"),
        ("allowed", json!("ls"), "allowed"),
        ("allowed", json!([7]), "allowed[0]"),
        ("cwd", json!(""), "cwd"),
        ("env", json!(["A=1"]), "env"),
        ("env", json!({"A": ["no"]}), "env.A"),
        ("inherit_env", json!([""]), "inherit_env[0]"),
        ("inherit_env", json!("PATH"), "inherit_env"),
    ] {
        let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
                AgentId::from("kernel_state"),
                json!({"type": "update_agent", "id": a.as_str(), field: value.clone()}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], "bad_arg", "{field}={value}: {r}");
        assert_eq!(r["arg"], json!(arg), "{field}={value}: {r}");
    }
}

#[tokio::test]
async fn reflect_advertises_the_allowlist_not_interpreters() {
    let tmp = TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("allowed".to_string(), json!(["ls( .*)?"]));
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], a.as_str());
    assert_eq!(
        r["sentence"], crate::MAN
    );
    assert_eq!(r["running"], 0);
    // THE GATE is advertised.
    assert_eq!(r["allowed"], json!(["ls( .*)?"]));
    // THE VERBS are rendered from the declaration: builtin ones, in declared order.
    let names: Vec<&str> = r["verbs"].as_object().expect("verbs").keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "shutdown", "start", "run", "status", "stop", "interrupt", "clear", "write", "resize",
            "ack", "output", "tail", "grep", "paste_image", "connector", "monitor", "monitors",
            "unmonitor", "declare_verbs", "reflect"
        ],
        "{r}"
    );
    assert!(
        r["verbs"]["start"].as_str().unwrap().starts_with("args: command:string matching /\\S/?"),
        "{r}"
    );
}

/// EVERY refusal on `start` carries a CODE — `args`/`env`, `command`/`code` and
/// the policy wall alike — so a typo is told from a policy refusal without
/// matching prose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_start_refusal_carries_a_code() {
    let tmp = TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("allowed".to_string(), json!(["echo( .*)?"]));
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let ask = |p: Value| {
        let kernel = Arc::clone(&kernel);
        let a = a.clone();
        async move { fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a, p)]).await.remove(0).result }
    };

    let r = ask(json!({"type": "start"})).await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("missing_arg"), &json!("command | pty")),
        "{r}"
    );
    let r = ask(json!({"type": "start", "command": "echo", "args": [{"no": 1}]})).await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("bad_arg"), &json!("args[0]")),
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
        let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
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
    let r = started(&kernel, &a.clone(), py_start(&py, "import time; time.sleep(30)", false)).await;
    let jid = r["job_id"]
        .as_i64()
        .unwrap_or_else(|| panic!("an int job_id: {r}"));
    let ps = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
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
        let bad_arg = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
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
    let snap = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
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
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "stop", "job_id": jid}),
        )])
        .await;
}

/// A command that matches no `allowed` pattern is refused, naming the patterns, and
/// nothing is spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_refuses_unlisted_command() {
    let tmp = TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("allowed".to_string(), json!(["ls( .*)?"])); // only ls
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let r = started(&kernel, &a.clone(), json!({"type": "start", "command": "/bin/rm", "args": ["-rf", "/"]})).await;
    assert!(
        r["error"].as_str().unwrap_or("").contains("not allowed"),
        "an unlisted command must refuse naming the patterns: {r}"
    );
    assert!(r["job_id"].is_null(), "nothing may be spawned: {r}");
    let st = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(
        st["jobs"].as_array().map(Vec::len),
        Some(0),
        "a refused start left a job row: {st}"
    );
}

/// Two in-kernel callers of the bridge `a`, siblings of it under the root:
/// `stranger` holds no line on it, `holder` holds `call: ["*"]`.
fn callers(kernel: &Arc<Kernel>, a: &AgentId) {
    for (id, can) in [
        ("stranger", json!({})),
        ("holder", json!({a.as_str(): {"call": ["*"]}})),
    ] {
        fantastic_kernel::test_support::register(
            kernel,
            Agent::new(
                AgentId::from(id),
                None,
                Some(AgentId::from("kernel_state")),
                json!({"can": can}).as_object().cloned().unwrap(),
                std::path::PathBuf::new(),
                false,
            ),
        );
    }
}

/// `payload` to `a`, sent AS the agent `who` (judged by its lines).
async fn send_as(kernel: &Arc<Kernel>, who: &str, a: &AgentId, payload: Value) -> Value {
    fantastic_kernel::test_support::as_agent(
        AgentId::from(who),
        fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(a.clone(), payload)]),
    )
    .await
    .remove(0)
    .result
}

/// Only the caller's `can` lines decide who drives the bridge: a caller with no
/// line gets the unknown answer (the same as a missing id) for every verb,
/// `reflect` included, and nothing is spawned; a caller with a line runs the job.
/// The bridge's record holds no rule of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_a_line_on_the_bridge_lets_an_agent_run_a_job() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    callers(&kernel, &a);
    let unknown = fantastic_kernel::refusal::unknown_agent(a.as_str());
    for p in [
        json!({"type": "start", "command": "echo", "args": ["x"]}),
        json!({"type": "status"}),
        json!({"type": "reflect"}),
    ] {
        let r = send_as(&kernel, "stranger", &a, p.clone()).await;
        assert_eq!(r, unknown, "{p} → {r}");
    }
    let st = send_as(&kernel, "holder", &a, json!({"type": "status"})).await;
    assert_eq!(st["jobs"].as_array().map(Vec::len), Some(0), "the stranger spawned: {st}");
    let mut rx = inbox(&kernel, &a);
    let r = send_as(&kernel, "holder", &a, json!({"type": "start", "command": "echo", "args": ["x"]})).await;
    assert!(!r["job_id"].is_null(), "the holder's start must run: {r}");
    let done = until(&mut rx, |v| v["type"] == "job_done", 25)
        .await
        .expect("no job_done");
    assert!(crate::jobs::printed(&a, &done, "stdout").contains('x'), "{done}");
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
print("READY"); sys.stdout.flush()
for _ in range(300):
    time.sleep(0.1)
"#;
    let r = started(&kernel, &pid.clone(), py_start(&py, code, false)).await;
    assert!(r["job_id"].is_i64(), "start failed: {r}");
    // the handler is in once the job says so: an interrupt before it kills python
    ready(&mut rx).await;

    let n = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(pid.clone(), json!({"type": "interrupt"}))])
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
    let r = started(&kernel, &pid.clone(), py_start(&py, SAY_READY_THEN_SLEEP, false)).await;
    assert!(r["job_id"].is_i64(), "start failed: {r}");
    ready(&mut rx).await;

    let n = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(pid.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
    assert!(n["killed"].as_u64().unwrap_or(0) >= 1, "stop returned {n}");

    until(&mut rx, |v| v["type"] == "job_done", 20)
        .await
        .expect("no job_done after stop");
    let st = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(pid.clone(), json!({"type": "status"}))])
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

/// A job that says `READY` once it runs, then sleeps.
const SAY_READY_THEN_SLEEP: &str = "import sys, time\nprint('READY')\nsys.stdout.flush()\ntime.sleep(30)\n";

/// Until the job printed its `READY` line: it runs, its setup done.
async fn ready(rx: &mut fantastic_kernel::inbox::InboxReceiver) {
    until(rx, |v| v["type"] == "progress" && v["line"] == "READY", 20)
        .await
        .expect("the job never said READY");
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

/// Waits until each of the running program's `drains` (`inbox_drain` for its
/// `_xbself_` proxy, `watch_drain` for a watch) is a live task row. `serve` registers
/// a drain only after `ensure_inbox` and `kernel.watch` both succeeded, so its
/// presence proves what is sent now reaches the program — a proxy INBOX alone does
/// not (it is registered before the watch).
async fn wait_drains(kernel: &Arc<Kernel>, owner: &AgentId, drains: &[&str], secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let rows = kernel.tasks.rows();
        if drains.iter().all(|d| {
            rows.iter()
                .any(|r| r["verb"] == *d && r["agent"] == owner.as_str())
        }) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{drains:?} never all registered for {owner}: {rows:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Waits until the running program has declared its verbs (mail is refused by name
/// before that).
async fn wait_declared(owner: &AgentId, secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while crate::mail::declared(owner).is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{owner} never declared its verbs"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
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
    extra.insert("allowed".to_string(), json!([".*"]));
    let (kernel, a) = mk_kernel_with(&tmp, extra.clone()).await;
    let r = started(&kernel, &a.clone(), json!({"type": "start", "command": "echo", "args": ["x"]})).await;
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
    let r = started(&k2, &b.clone(), json!({"type": "start", "command": "ft_only_here"})).await;
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
    let r = started(&k2, &b.clone(), json!({"type": "start", "command": "echo", "args": ["x"]})).await;
    assert_eq!(r["reason"], "transport_error", "{r}");
    assert!(r["error"].as_str().unwrap().contains("No such file"), "{r}");
}

/// `start` announces its job AT ONCE (`job_started`; its settle is the job's end). It
/// also RECORDS what ran: `command`/`args` surface in `status` so a caller can re-issue
/// verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_announces_its_job_at_once_and_records_the_invocation() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let t0 = std::time::Instant::now();
    let r = started(&kernel, &a.clone(), py_start(&py, "import time; time.sleep(30)", false)).await;
    assert!(r["job_id"].is_i64(), "no job_id: {r}");
    assert_eq!(r["status"], "running");
    assert!(t0.elapsed() < Duration::from_secs(5), "start blocked");
    let st = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["jobs"][0]["command"], json!(py.to_string_lossy()));
    assert_eq!(st["jobs"][0]["args"][0], json!("-u"));
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "stop"}))])
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
    let _ = started(&kernel, &a.clone(), py_start(
                &py,
                "import sys\nprint('o1')\nprint('e1', file=sys.stderr)\nprint('o2')\n",
                false,
            )).await;
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
    fantastic_kernel::test_support::grant(&kernel, a.as_str(), "kernel_state", json!({"call": ["reflect"]}));
    let mut rx = inbox(&kernel, &a);
    let _ = started(&kernel, &a.clone(), py_start(
                &py,
                "r = kernel.send('kernel_state', {'type': 'reflect'})\nprint('GOT', r.get('id'))\n",
                true,
            )).await;
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

/// A verb the program DECLARED is MAIL for it, ACKed `{queued:true}` — and
/// delivered exactly once (the fanout already did it; re-emitting would double it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_verb_is_queued_to_the_running_program() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    // A slow cold start: the program registers its handler ~2.5 s after spawn.
    let code = r#"
import threading, time
kernel.send(kernel.self_id, {'type': 'declare_verbs', 'verbs': [{'name': 'ping', 'description': 'a ping', 'inputSchema': {'type': 'object'}}]})
time.sleep(2.5)
seen = []
def _on(m):
    if m.get('type') == 'ping':
        seen.append(1)
        kernel.emit({'type': 'pong', 'count': len(seen)})
kernel.on_message(_on)
threading.Event().wait()
"#;
    let _ = started(&kernel, &a.clone(), py_start(&py, code, true)).await;
    wait_drains(&kernel, &a, &["inbox_drain"], 30).await;
    let ack = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "ping"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(ack["queued"], true, "not queued: {ack}");
    let pong = until(&mut rx, |v| v["type"] == "pong", 20)
        .await
        .expect("program never received the message");
    assert_eq!(pong["count"], 1, "delivered more than once: {pong}");
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

/// The ack is not delivery. Mail sent before the program has registered its
/// `on_message` handler is ACKed `{queued:true}` and dropped: no `_xbself_`
/// watcher exists yet, so the fanout mirrors it to nobody. This pins the
/// deterministic cause behind the slow-start flake of
/// `a_declared_verb_is_queued_to_the_running_program`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_sent_before_on_message_registers_is_not_delivered_and_the_ack_does_not_prove_delivery(
) {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    let code = r#"
import threading, time
kernel.send(kernel.self_id, {'type': 'declare_verbs', 'verbs': [{'name': 'ping', 'description': 'a ping', 'inputSchema': {'type': 'object'}}]})
time.sleep(2.5)
seen = []
def _on(m):
    if m.get('type') == 'ping':
        seen.append(1)
        kernel.emit({'type': 'pong', 'count': len(seen)})
kernel.on_message(_on)
threading.Event().wait()
"#;
    let _ = started(&kernel, &a.clone(), py_start(&py, code, true)).await;
    // Sent as soon as the program declared `ping`: it is still in its 2.5 s startup sleep.
    wait_declared(&a, 30).await;
    let ack = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "ping"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(ack["queued"], true, "not queued: {ack}");
    let rows = kernel.tasks.rows();
    assert!(
        !rows.iter().any(|r| r["verb"] == "inbox_drain"),
        "the drain registered too early for this test: {rows:?}"
    );
    assert!(
        until(&mut rx, |v| v["type"] == "pong", 1).await.is_none(),
        "a ping sent before on_message was delivered"
    );
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

/// A verb nobody declared is refused by name, with the list the agent answers: there
/// is no "accept any verb" mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undeclared_verb_is_refused_by_name() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "ping"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "ping", "{r}");
    assert!(
        r["error"].as_str().unwrap_or("").contains("answers: shutdown, start"),
        "{r}"
    );
}

/// Only a running program declares verbs, and never one the agent answers itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declare_verbs_needs_a_running_job_and_a_free_name() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "declare_verbs", "verbs": [{"name": "ping", "description": "a ping", "inputSchema": {"type": "object"}}]}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "not_found", "{r}");
    assert!(crate::mail::declared(&a).is_empty());
    // a name the agent answers is refused by the shared registry
    let list = json!([{"name": "start", "description": "x", "inputSchema": {"type": "object"}}]);
    let r = crate::mail::declare_for_test_result(&a, &list).expect_err("reserved");
    assert!(r["error"].as_str().unwrap().contains("already a verb"), "{r}");
}

/// PARITY WITH PYTHON: a program's sends carry the RUNNER's identity and are
/// gated by its lines. A sibling the runner holds no line on must refuse (the
/// unknown answer) — a relay forwarding with NO sender would be the operator,
/// ungated, and a spawned program could reach any agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_program_is_gated_as_its_runner_not_as_the_operator() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    // two SIBLING runners under the root: `a` holds a line on `ok_sib` only (a line
    // on the root would reach both — a line reaches the target's whole tree)
    for id in ["sib", "ok_sib"] {
        let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
                AgentId::from("kernel_state"),
                json!({"type": "create_agent", "parent_id": "kernel_state", "id": id,
                       "handler_module": "exec_bridge.tools"}),
            )])
            .await
            .remove(0)
            .result;
    }
    fantastic_kernel::test_support::grant(&kernel, a.as_str(), "ok_sib", json!({"call": ["reflect"]}));
    let mut rx = inbox(&kernel, &a);
    let _ = started(&kernel, &a.clone(), py_start(
                &py,
                "r = kernel.send('sib', {'type': 'reflect'})\nprint('SIB', r.get('reason'))\n\
                 r2 = kernel.send('ok_sib', {'type': 'reflect'})\nprint('CORE', r2.get('id'))\n",
                true,
            )).await;
    let done = until(&mut rx, |v| v["type"] == "job_done", 25)
        .await
        .expect("no job_done");
    let out = crate::jobs::printed(&a, &done, "stdout");
    let out = out.as_str();
    assert!(
        out.contains("SIB unknown_agent"),
        "the sibling must refuse: {done}"
    );
    assert!(
        out.contains("CORE ok_sib"),
        "the sibling it holds a line on still answers: {done}"
    );
}

/// `connector` hands over the reference wire client — a model pastes it instead of
/// re-deriving the socket framing. Ungated like reflect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_connector_verb_hands_over_the_reference_client() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "connector"}))])
        .await
        .remove(0)
        .result;
    let src = r["source"].as_str().expect("source");
    assert!(
        src.contains("FANTASTIC_CTRL_FD") && src.contains("def send("),
        "{r}"
    );
}

/// A stream-speaking stand-in for `file_bridge`: `write_stream` keeps the blob,
/// `read_stream` hands it back as the reply BODY — the binary channel only.
struct BlobStore(std::sync::Mutex<Vec<u8>>);

fantastic_kernel::stand_in_verbs!(blob_verbs {
    WriteStreamV "write_stream" { path: Option<String>, bytes: Option<String> },
    ReadStreamV "read_stream" { path: Option<String>, bytes: Option<String> },
});

#[async_trait::async_trait]
impl Handler for BlobStore {
    type Call = blob_verbs::Call;
    fn name(&self) -> &str {
        "blob"
    }
    fn man(&self) -> &'static str {
        "A test stand-in."
    }
    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        blob_verbs::VERBS
    }
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        blob_verbs::dynamic_verbs(agent)
    }
    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        fantastic_kernel::fixture_record!()
    }
    async fn handle(
        &self,
        _id: &AgentId,
        call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
        Ok(Some(
            json!({"error": format!("text channel: {}", call.payload()["type"])}),
        ))
    }
    async fn handle_binary(
        &self,
        _id: &AgentId,
        call: Self::Call,
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
        match call {
            blob_verbs::Call::WriteStreamV(_) => {
                let n = blob.len();
                *self.0.lock().unwrap() = blob;
                Ok((Some(json!({"written": n})), None))
            }
            blob_verbs::Call::ReadStreamV(_) => {
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
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let a = AgentId::from(agent_id_for(&tmp).as_str());
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "id": a.as_str(),
                   "handler_module": HANDLER_MODULE, "allowed": [".*"],
                   "cwd": tmp.path().to_string_lossy(),
                   "can": {"blob": {"call": ["*"]}}}),
        )])
        .await
        .remove(0)
        .result;
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel_state", "id": "blob",
                   "handler_module": "blob.tools"}),
        )])
        .await
        .remove(0)
        .result;
    let mut rx = inbox(&kernel, &a);
    let _ = started(&kernel, &a.clone(), py_start(
                &py,
                "raw = bytes(range(256)) * 3\n\
                 w = kernel.send('blob', {'type': 'write_stream', 'path': 'b', 'bytes': raw})\n\
                 r = kernel.send('blob', {'type': 'read_stream', 'path': 'b', 'bytes': b''})\n\
                 print('WROTE', w['written'], 'READ', len(r['bytes']), r['bytes'] == raw, \
                 type(r['bytes']).__name__)\n",
                true,
            )).await;
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
/// so the program needs no `bytes: b''` to get its chunk raw.
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
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let a = AgentId::from(agent_id_for(&tmp).as_str());
    for rec in [
        json!({"type": "create_agent", "parent_id": "kernel", "id": a.as_str(),
               "handler_module": HANDLER_MODULE, "allowed": [".*"],
               "cwd": tmp.path().to_string_lossy(),
               "can": {"blob": {"call": ["*"]}}}),
        json!({"type": "create_agent", "parent_id": "kernel_state", "id": "blob",
               "handler_module": "blob.tools"}),
    ] {
        let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(AgentId::from("kernel_state"), rec)])
            .await
            .remove(0)
            .result;
        assert!(r.get("error").is_none(), "{r}");
    }
    let mut rx = inbox(&kernel, &a);
    let _ = started(&kernel, &a.clone(), py_start(
                &py,
                "raw = bytes(range(256))\n\
                 kernel.send('blob', {'type': 'write_stream', 'path': 'b', 'bytes': raw})\n\
                 r = kernel.send('blob', {'type': 'read_stream', 'path': 'b'})\n\
                 print('READ', r.get('error'), type(r.get('bytes')).__name__, \
                 r.get('bytes') == raw)\n",
                true,
            )).await;
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
    let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
        state_dir: tmp.path().join(".fantastic"),
        work_dir: tmp.path().to_path_buf(),
    });
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
    fantastic_kernel::test_support::register(&kernel, Arc::clone(&root));
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
                   "cwd": tmp.path().to_string_lossy(),
                   "can": {"fb": {"call": ["*"]}}}),
            a.as_str(),
        ),
        (
            json!({"type": "create_agent", "parent_id": "kernel_state", "id": "fb",
                   "handler_module": "file_bridge.tools", "allow": ["."],
                   "root": served.to_string_lossy()}),
            "fb",
        ),
    ] {
        let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(AgentId::from("kernel_state"), rec)])
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
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "id": "secret",
                   "handler_module": HANDLER_MODULE}),
        )])
        .await;

    let (mut child, stop, relay) = wire(&kernel, &a, 9001);
    wire_write(&mut child, &json!({"op": "watch", "src": "secret"})).await;
    let ev = wire_read(&mut child).await;
    assert_eq!(ev["op"], "event", "{ev}");
    assert_eq!(ev["src"], "secret", "{ev}");
    // a denial is the unknown answer
    assert_eq!(ev["payload"]["reason"], "unknown_agent", "{ev}");

    // …and NOTHING was registered. No proxy inbox row — and no WATCHER row on the
    // source either, which an emit proves from outside the crate: register the
    // proxy's inbox ourselves, and a leaked watcher fans the payload into it.
    let proxy = AgentId::from("_xbw_9001_secret");
    assert!(!kernel.inboxes.contains_key(&proxy), "proxy inbox leaked");
    let mut spy = inbox(&kernel, &proxy);
    fantastic_kernel::send::as_operator(kernel
        .emit(&AgentId::from("secret"), json!({"type": "ping"})))
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
    // the runner's own record is in its own tree, so both of these are permitted.
    wire_write(&mut child, &json!({"op": "watch", "src": a.as_str()})).await;
    wire_write(&mut child, &json!({"op": "onmessage"})).await;

    let proxies = [
        AgentId::from(format!("_xbw_9002_{a}").as_str()),
        AgentId::from("_xbself_9002"),
    ];
    // both subscriptions fully up (their drains run), not just their inboxes
    wait_drains(&kernel, &a, &["watch_drain", "inbox_drain"], 5).await;
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
    fantastic_kernel::send::as_operator(kernel.emit(&a, json!({"type": "ping"}))).await;
    for (p, spy) in proxies.iter().zip(spies.iter_mut()) {
        assert!(spy.try_recv().is_none(), "{p} left a watcher row");
    }
}

/// Mail is a call like any other: a caller with no line on the bridge gets the
/// unknown answer and its mail never reaches the program (the kernel refuses it
/// before the fanout); a caller with a line has its mail delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_without_a_line_never_reaches_the_program() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    callers(&kernel, &a);
    crate::mail::declare_for_test(&a, &["probe_mail", "after"]);
    let (mut child, stop, relay) = wire(&kernel, &a, 9006);
    wire_write(&mut child, &json!({"op": "onmessage"})).await;
    // the owner proxy's drain runs: mail sent from here reaches the program (its
    // inbox alone is registered before the watch that feeds it)
    wait_drains(&kernel, &a, &["inbox_drain"], 5).await;
    assert!(
        kernel.inboxes.contains_key(&AgentId::from("_xbself_9006")),
        "the owner proxy never registered"
    );
    let unknown = fantastic_kernel::refusal::unknown_agent(a.as_str());
    for p in [
        json!({"type": "status"}),
        json!({"type": "probe_mail", "x": "TEST-VALUE-MAIL"}),
        json!({"type": "probe_mail", "x": "TEST-VALUE-MAIL-2"}),
    ] {
        let out = send_as(&kernel, "stranger", &a, p.clone()).await;
        assert_eq!(out, unknown, "{p} → {out}");
    }
    send_as(&kernel, "holder", &a, json!({"type": "after"})).await;
    // the first frame down the fd is the holder's mail
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

/// A hand-registered owner inbox with NO drain task: the test pulls the queued items
/// itself, so the drain's timing is the test's.
fn manual_owner_inbox(
    kernel: &Arc<Kernel>,
    owner: &AgentId,
    job: i64,
) -> fantastic_kernel::inbox::InboxReceiver {
    let proxy = AgentId::from(format!("_xbself_{job}").as_str());
    let rx = kernel.ensure_inbox(&proxy);
    crate::relay::register_owner_inbox_for_test(owner, TaskId(job), &proxy);
    rx
}

async fn send_to(kernel: &Arc<Kernel>, owner: &AgentId, p: Value) -> Value {
    fantastic_kernel::test_support::send_settled(kernel, vec![Item::new(owner.clone(), p)])
        .await
        .remove(0)
        .result
}

/// A mail no line permits is never queued for the program: the owner inbox holds
/// only the mail of a caller with a line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_mail_is_never_queued_for_the_program() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    callers(&kernel, &a);
    crate::mail::declare_for_test(&a, &["probe_mail", "after"]);
    let mut rx = manual_owner_inbox(&kernel, &a, 9101);
    let proxy = AgentId::from("_xbself_9101");
    assert!(fantastic_kernel::send::as_operator(kernel.watch(&a, proxy)).await.is_none());
    let out = send_as(&kernel, "stranger", &a, json!({"type": "probe_mail"})).await;
    assert_eq!(out, fantastic_kernel::refusal::unknown_agent(a.as_str()), "{out}");
    send_as(&kernel, "holder", &a, json!({"type": "after"})).await;
    let first = rx.recv().await.expect("after queued");
    assert_eq!(first.payload, json!({"type": "after"}));
    assert_eq!(
        crate::relay::drain_verdict(&first.payload, &a),
        Some(json!({"op": "inbox", "payload": {"type": "after"}}))
    );
}

/// A verb the agent answers itself (`write`) is no mail: the fanout mirrors it to the
/// owner inbox, and the drain never hands it to the program.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_builtin_verb_is_never_mail() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    crate::mail::declare_for_test(&a, &["probe_mail"]);
    let mut rx = manual_owner_inbox(&kernel, &a, 9104);
    assert!(fantastic_kernel::send::as_operator(kernel
        .watch(&a, AgentId::from("_xbself_9104")))
        .await
        .is_none());
    let w = json!({"type": "write", "job_id": 1, "data": "keys\n"});
    send_to(&kernel, &a, w).await;
    let it = rx.recv().await.expect("write queued");
    assert_eq!(crate::relay::drain_verdict(&it.payload, &a), None);
}

/// The fanout mirrors every payload sent to the leg into the owner inbox; the program
/// is handed only the mail it declared. Calls of the verbs the agent answers itself
/// (a caller's `write` keystrokes, paste bytes, drive calls), for this job or any
/// other, never reach it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_inbox_hands_the_program_only_its_declared_mail() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    crate::mail::declare_for_test(&a, &["mail"]);
    let (mut child, stop, relay) = wire(&kernel, &a, 9008);
    wire_write(&mut child, &json!({"op": "onmessage"})).await;
    // the owner proxy's drain runs: mail sent from here reaches the program (its
    // inbox alone is registered before the watch that feeds it)
    wait_drains(&kernel, &a, &["inbox_drain"], 5).await;
    assert!(
        kernel.inboxes.contains_key(&AgentId::from("_xbself_9008")),
        "the owner proxy never registered"
    );
    for p in [
        json!({"type": "write", "job_id": 9009, "data": "TEST-VALUE-KEYS\n"}),
        json!({"type": "resize", "job_id": 9009, "cols": 10, "rows": 5}),
        json!({"type": "paste_image", "job_id": 9009, "bytes": "TEST-VALUE-PNG"}),
        json!({"type": "stop", "job_id": 9009}),
        json!({"type": "interrupt", "job_id": 9009}),
        json!({"type": "clear", "job_id": 9009}),
        // the bulk forms and this job's own are calls of the agent too
        json!({"type": "stop"}),
        json!({"type": "interrupt"}),
        json!({"type": "clear"}),
        json!({"type": "write", "job_id": 9008, "data": "own"}),
        json!({"type": "mail", "n": 1}),
    ] {
        fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), p)]).await;
    }
    // the one frame down the fd: the declared mail
    for want in [json!({"type": "mail", "n": 1})] {
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
    let r = started(&kernel, &a.clone(), py_start(&py, "import time; time.sleep(30)", false)).await;
    // The job's id IS its task id: `cancel` takes it as it is.
    let job_id = TaskId(r["job_id"].as_i64().expect("job_id"));
    let task_id = job_id.get();
    let c = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
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
    let cleared = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "clear"}))])
        .await
        .remove(0)
        .result;
    assert!(cleared["cleared"].as_u64().unwrap_or(0) >= 1, "{cleared}");
    let st = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a, json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["jobs"].as_array().map(Vec::len), Some(0), "{st}");
}

/// A `cancel` that lands before the job's pump first RUNS: the kernel's task body
/// picks the cancel first and drops the pump unpolled. The job's end guards (its row
/// finalised to `stopped`, its monitors told it ended) belong to the pump FROM ITS
/// FORK, so they hold on that path too — never a row stuck at `stopping`.
/// Deterministic: a current-thread runtime runs no other task until this one waits,
/// and nothing between the fork and the cancel does.
#[tokio::test(flavor = "current_thread")]
async fn a_job_cancelled_before_its_pump_first_runs_still_finalises() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let (job_id, _) = crate::start_job(
        &a,
        Some(&py.to_string_lossy()),
        Some(vec!["-c".to_string(), "import time; time.sleep(30)".to_string()]),
        None,
        &kernel,
    )
    .await
    .expect("started");
    let c = kernel.cancel_task(&job_id).await;
    assert_eq!(c["cancelled"], true, "{c}");
    // the dropped pump's guards write the row: wait on that (the deadline only
    // bounds a failure)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while jobs::jobs()[&(a.clone(), job_id)].status == "stopping" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the row never left `stopping`"
        );
        tokio::task::yield_now().await;
    }
    let (status, code, closed) = {
        let g = jobs::jobs();
        let j = &g[&(a.clone(), job_id)];
        (j.status.clone(), j.exit_code, j.monitors_closed)
    };
    assert_eq!(
        (status.as_str(), code, closed),
        ("stopped", Some(crate::sys::KILLED_EXIT_CODE), true)
    );
}

/// `job_started` is the first thing a job's watchers hear of it: the pump reads no
/// byte before the announcement is out. A shell on a PTY prints at once, and its
/// output used to land on the inbox ahead of its own `job_started` (the pump was
/// told its id first, and runs on another worker).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_started_comes_before_the_jobs_first_output() {
    let tmp = TempDir::new().unwrap();
    let sh = crate::sys::fake_shell(tmp.path(), "fakesh");
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    let of_a_job = |v: &Value| {
        ["job_started", "progress", "output", "job_done"].contains(&v["type"].as_str().unwrap_or(""))
    };
    for _ in 0..100 {
        let (k, item) = (
            Arc::clone(&kernel),
            Item::new(
                a.clone(),
                json!({"type": "start", "command": sh.to_string_lossy(), "pty": {"cols": 80, "rows": 24}}),
            ),
        );
        let settle = tokio::spawn(async move { fantastic_kernel::test_support::send_settled(&k, vec![item]).await });
        let first = until(&mut rx, of_a_job, 20).await.expect("an event of the job");
        assert_eq!(first["type"], "job_started", "{first}");
        let jid = first["job_id"].clone();
        let w = json!({"type": "write", "job_id": jid, "data": format!("x{}", crate::sys::ENTER)});
        fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), w)]).await;
        until(&mut rx, |v| v["type"] == "job_done" && v["job_id"] == jid, 20)
            .await
            .expect("its job_done");
        settle.await.expect("the start settled");
    }
}

/// `kernel.args` / FANTASTIC_ARGS was set by no spawner: a command runner takes its
/// arguments as ARGV, and the connector source names no such thing.
#[test]
fn the_connector_has_no_args() {
    assert!(!CONNECTOR_SRC.contains("FANTASTIC_ARGS"));
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
    let r = started(&kernel, &a.clone(), py_start(
                &py,
                "import threading\n\
                 kernel.watch(kernel.self_id, lambda p: None)\n\
                 kernel.on_message(lambda p: None)\n\
                 threading.Event().wait()\n",
                true,
            )).await;
    let job_id = r["job_id"].as_i64().expect("job_id");
    let proxies = [
        AgentId::from(format!("_xbw_{job_id}_{a}").as_str()),
        AgentId::from(format!("_xbself_{job_id}").as_str()),
    ];
    // both subscriptions fully up (their drains run), not just their inboxes
    wait_drains(&kernel, &a, &["watch_drain", "inbox_drain"], 20).await;
    for p in &proxies {
        assert!(kernel.inboxes.contains_key(p), "{p} was never registered");
    }

    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "stop"}))])
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

// ─── python parity ───────────────────────────────────────────────
//
// Each test below pins a text, a coercion or an ordering to the canonical python
// (`PB/io/exec_bridge/src/exec_bridge/tools.py`). Where python's own behaviour is a
// quirk, the quirk is what is pinned.

/// Reflect reports the record's `cwd` as written (null when unset — there
/// is no process default any more) and the `inherit_env` it runs with.
#[tokio::test]
async fn reflect_reports_the_record_as_written() {
    let tmp = TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("cwd".to_string(), json!("/wd"));
    meta.insert("inherit_env".to_string(), json!(["PATH"]));
    meta.insert("allowed".to_string(), json!(["ls"]));
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let r = reflect_reply(&a, &kernel);
    assert_eq!(r["cwd"], "/wd");
    assert_eq!(r["inherit_env"], json!(["PATH"]));
    assert_eq!(r["allowed"], json!(["ls"]));

    let tmp2 = TempDir::new().unwrap();
    let mut m2 = Map::new();
    m2.insert("cwd".to_string(), Value::Null);
    let (k2, b) = mk_kernel_with(&tmp2, m2).await;
    let r2 = reflect_reply(&b, &k2);
    assert_eq!(r2["cwd"], Value::Null);
    assert_eq!(r2["inherit_env"], json!([]));
}

/// `shutdown` is a common verb this bundle does not declare: the shared entry
/// answers it itself, nothing to release, and it is never mail.
#[tokio::test]
async fn shutdown_is_answered_by_the_shared_entry() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "shutdown"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r, Value::Null, "{r}");
}

/// Every refusal quotes with python's `repr`, and an ABSENT `type` is `None`.
#[tokio::test]
async fn refusal_texts_use_python_repr() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    // an ABSENT type is `null` in the refusal, which lists what the agent answers.
    let r = crate::verbs::Call::parse(a.as_str(), &json!({}), &[]).expect_err("refused");
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], Value::Null, "{r}");
    // `no job {jid!r}`.
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
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

/// A broken `allowed` regex is refused with its text up to the engine tail after `': '`
/// (that tail is the regex engine's own).
#[test]
fn a_broken_allowed_regex_names_the_pattern() {
    let b = ExecBridgeBundle;
    let mut broken = Map::new();
    broken.insert("allowed".to_string(), json!(["("]));
    let e = b.validate_meta(&broken, &Map::new()).expect("refused");
    assert!(
        e.starts_with("allowed contains a broken regex '(': "),
        "{e}"
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
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "status", "job_id": null}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["jobs"], json!([]), "null job_id means ALL: {r}");
    for not_int in [json!(""), json!([]), json!("7"), json!(7.5), json!(true)] {
        let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
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
        fantastic_kernel::test_support::non_finite("Infinity"),
    );
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), Value::Object(inf))])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], json!("bad_arg"), "{r}");
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "status", "job_id": 0}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "not_found", "0 is an id, not ALL: {r}");
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "stop", "job_id": 7}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["error"], "exec_bridge: no job 7");
    assert_eq!(r["job_id"], json!(7), "the raw value is echoed: {r}");
}

/// `start` checks its arguments against its declaration: nothing is coerced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_checks_its_arguments_and_coerces_none() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let call = |p: Value| {
        let kernel = Arc::clone(&kernel);
        let a = a.clone();
        async move { fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a, p)]).await.remove(0).result }
    };
    // A command of nothing but whitespace does not match the declared pattern: refused,
    // not a spawn of "".
    let r = call(json!({"type": "start", "command": "  \t "})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "command", "{r}");
    // `args` is a list: a text is refused, never split or wrapped.
    let r = call(json!({"type": "start", "command": "ls", "args": "-l"})).await;
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
    // A wake-up is a monitor's: `notify`/`match`/`every` are no argument of `start`,
    // refused by name, never dropped silently.
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
    }
    let r = call(json!({"type": "start", "command": py.to_string_lossy(),
                        "args": ["-c", "pass"]}))
    .await;
    assert!(r["job_id"].is_i64(), "{r}");
    // an argv item that is not text is refused, not stringified
    let r = call(json!({"type": "start", "command": py.to_string_lossy(),
                        "args": ["-c", "pass", true, 2, 1.5]}))
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert!(r["arg"].as_str().unwrap_or("").starts_with("args"), "{r}");
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
    let mut ids = Vec::new();
    for _ in 0..3 {
        let r = started(&kernel, &a.clone(), py_start(&py, "import time; time.sleep(30)", false)).await;
        ids.push(r["job_id"].as_i64().expect("job_id"));
    }
    let st = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    let listed: Vec<i64> = st["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .filter_map(|j| j["job_id"].as_i64())
        .collect();
    assert_eq!(listed, ids, "status must list jobs in start order");
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "stop"}))])
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
    let _ = started(&kernel, &a.clone(), py_start(&py, "import time; time.sleep(30)", false)).await;
    let st = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert!(
        st["jobs"][0]["job_id"].is_i64(),
        "job_id must be an int: {st}"
    );
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "stop"}))])
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
    let r = started(&kernel, &a.clone(), py_start(&py, "import time; time.sleep(30)", false)).await;
    let jid = r["job_id"].as_i64().expect("job_id");
    let clear = || async {
        fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
                a.clone(),
                json!({"type": "clear", "job_id": jid}),
            )])
            .await
            .remove(0)
            .result
    };
    // A BARE clear leaves it — it is running.
    let bare = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "clear"}))])
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
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
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
    let st = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "status"}))])
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
    let _ = started(&kernel, &a.clone(), py_start(
                &py,
                "import sys\nsys.stdout.buffer.write(b'a\\r\\nb\\xff')\n",
                false,
            )).await;
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
    let _ = started(&kernel, &a.clone(), py_start(&py, "print('x' * 200000)\n", false)).await;
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
    let _ = started(&kernel, &a.clone(), py_start(&py, SAY_READY_THEN_SLEEP, false)).await;
    ready(&mut rx).await;
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "stop"}))])
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
    let _ = started(&kernel, &a.clone(), py_start(
                &py,
                "import os, sys, time\nsys.stdout.flush()\nos.close(1)\nos.close(2)\ntime.sleep(4)\n",
                false,
            )).await;
    // Give both pipes time to hit EOF while the process is still sleeping.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        reflect_reply(&a, &kernel)["running"],
        1,
        "reflect counts a live process whose pipes are closed"
    );
    crate::mail::declare_for_test(&a, &["ping"]);
    let mail = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "ping"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(mail["queued"], true, "mail must reach a live job: {mail}");
    let st = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "status"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["jobs"][0]["exit_code"], json!(null), "{st}");
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "stop"}))])
        .await
        .remove(0)
        .result;
}

/// A spawn failure reads exactly as python's `OSError`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_failure_text_matches_python() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = started(&kernel, &a.clone(), json!({"type": "start", "command": "nope"})).await;
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
    extra.insert("allowed".to_string(), json!([".*"]));
    extra.insert("cwd".to_string(), json!(gone.to_string_lossy()));
    let (kernel, a) = mk_kernel_with(&tmp, extra).await;
    let r = started(&kernel, &a.clone(), json!({"type": "start", "command": "/bin/echo", "args": ["x"]})).await;
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
    meta.insert("allowed".to_string(), json!([".*"]));
    // the operator's record `env` cannot claim the control vars either
    meta.insert(
        "env".to_string(),
        json!({"FANTASTIC_SELF_ID": "root", "FANTASTIC_CTRL_FD": "0", "MINE": "kept"}),
    );
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let mut rx = inbox(&kernel, &a);
    let _ = started(&kernel, &a.clone(), json!({
                "type": "start",
                "command": py.to_string_lossy(),
                "args": ["-u", "-c", "import os; print('ID', os.environ['FANTASTIC_SELF_ID'], 'FD', os.environ['FANTASTIC_CTRL_FD'])"],
            })).await;
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
/// `kernel.call({target, payload})`: no `_call_stack` tag, no cycle/depth
/// refusal of its own. A kind-B frame is the SAME batch item carrying its body:
/// a pid and a call row like any other execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_forwards_a_send_untouched_and_a_binary_one_through_the_batch() {
    use tokio::io::AsyncWriteExt;
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let tap = AgentId::from("tap_relay");
    let mut tap_rx = kernel.ensure_inbox(&tap);
    assert!(fantastic_kernel::send::as_operator(kernel.watch(&a, tap.clone())).await.is_none());
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

/// The three `arg` refusals carry `error, reason, hint,
/// arg` IN THAT ORDER (key order is wire — `Value` equality ignores it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arg_refusals_are_byte_exact_in_key_order() {
    let ask = |k: Arc<Kernel>, a: AgentId, p: Value| async move {
        let r = fantastic_kernel::test_support::send_settled(&k, vec![Item::new(a, p)]).await.remove(0).result;
        serde_json::to_string(&r).unwrap()
    };
    let open = || {
        let mut extra = Map::new();
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
    let job = started(&kernel, &a.clone(), py_start(
                &py,
                "import sys, time\nfor i in range(6):\n    print(f'step {i} ok', flush=True)\n\
                 print('ERROR disk full', file=sys.stderr, flush=True)\nprint('step 6 ok', flush=True)\n\
                 time.sleep(30)\n",
                false,
            )).await;
    let jid = job["job_id"].clone();
    let _ = until(&mut rx, |v| v["type"] == "progress" && v["n"] == 8, 20)
        .await
        .expect("8 lines never printed");
    let ask = |p: Value| {
        let k = Arc::clone(&kernel);
        let a = a.clone();
        async move { fantastic_kernel::test_support::send_settled(&k, vec![Item::new(a, p)]).await.remove(0).result }
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
/// `start` SETTLES WHEN ITS JOB EXITS (#795), as `run` does: the job is announced
/// (`job_started`) while it runs, and the call's settle is its end — not `running`,
/// with its exit code and its last lines.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_settles_when_its_job_exits() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    let program = format!("{LF_STDIO}import time, sys; time.sleep(0.5); print('bye'); sys.exit(4)");
    let (k, item) = (
        Arc::clone(&kernel),
        Item::new(a.clone(), json!({"type": "start", "command": py.to_string_lossy(), "args": ["-u", "-c", program]})),
    );
    let settle = tokio::spawn(async move { fantastic_kernel::test_support::send_settled(&k, vec![item]).await.remove(0).result });
    let ann = until(&mut rx, |v| v["type"] == "job_started", 10).await.expect("no job_started");
    assert!(!settle.is_finished(), "the start settled before its job ended");
    let r = tokio::time::timeout(Duration::from_secs(20), settle)
        .await
        .expect("the start never settled")
        .expect("the start panicked");
    assert_eq!(r["job_id"], ann["job_id"], "{r}");
    assert_ne!(r["status"], "running", "{r}");
    assert_eq!(r["exit_code"], 4, "{r}");
    assert!(r["lines"].to_string().contains("bye"), "{r}");
}

/// `status` is a SNAPSHOT: on a running job it answers at once, the job still running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_is_a_snapshot_of_a_running_job() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let job = started(&kernel, &a, py_start(&py, "import time; time.sleep(30)", false)).await;
    let jid = job["job_id"].as_i64().expect("job_id");
    let t0 = std::time::Instant::now();
    let st = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "status", "job_id": jid}))])
        .await
        .remove(0)
        .result;
    assert_eq!(st["jobs"][0]["status"], "running", "{st}");
    assert!(t0.elapsed() < Duration::from_secs(1), "took {:?}", t0.elapsed());
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "stop", "job_id": jid}))])
        .await;
}

/// `status` on a job this agent never had answers at once: `not_found`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_of_an_unknown_job_answers_at_once() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let t0 = std::time::Instant::now();
    let st = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "status", "job_id": 999_999}),
        )])
        .await
        .remove(0)
        .result;
    let took = t0.elapsed();
    assert!(took < std::time::Duration::from_secs(1), "took {took:?}");
    assert_eq!(st["reason"], "not_found", "{st}");
}

/// A STOPPED job ends its `start`: the call settles with the job's end, so a starter
/// never waits on a job that is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_job_settles_its_start() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    let (k, item) = (Arc::clone(&kernel), Item::new(a.clone(), py_start(&py, "import time; time.sleep(30)", false)));
    let settle = tokio::spawn(async move { fantastic_kernel::test_support::send_settled(&k, vec![item]).await.remove(0).result });
    let ann = until(&mut rx, |v| v["type"] == "job_started", 10).await.expect("no job_started");
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), json!({"type": "stop", "job_id": ann["job_id"]}))])
        .await;
    let r = tokio::time::timeout(Duration::from_secs(10), settle)
        .await
        .expect("a stopped job's start never settled")
        .expect("the start panicked");
    assert_eq!(r["job_id"], ann["job_id"], "{r}");
    assert_ne!(r["status"], "running", "{r}");
}

#[test]
fn an_unbalanced_allowed_pattern_permits_nothing() {
    let mut meta = Map::new();
    meta.insert("allowed".into(), json!(["ls)|(?:.*"]));
    assert!(command_permitted(&meta, "rm -rf /").is_some());
    assert!(command_permitted(&meta, "ls").is_some());
}

/// `run` — its reply is the job's end: a short program comes back done, with its
/// exit code and its printed lines.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_returns_the_output_in_one_reply() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let program = format!("{LF_STDIO}import sys; print(\"hello\"); sys.exit(3)");
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({
                "type": "run",
                "command": py.to_string_lossy(),
                "args": ["-u", "-c", program],
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
    let _ = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "clear", "job_id": jid}),
        )])
        .await;
}

/// THE ASYNC RUN (#795): the call is acked at once, and its settle comes when the
/// process EXITS — as a message to the caller, with the exit code and the output.
/// Nothing polls: the handler deferred its settle and the job's pump settled it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_settles_when_the_process_exits() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let program = format!("{LF_STDIO}import sys, time; time.sleep(0.6); print(\"late\"); sys.exit(4)");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let t0 = std::time::Instant::now();
    let ack = fantastic_kernel::send::as_operator(
        kernel.call(
            Item::new(
                a.clone(),
                json!({"type": "run", "command": py.to_string_lossy(), "args": ["-u", "-c", program]}),
            ),
            move |o| async move {
                let _ = tx.send((o, std::time::Instant::now()));
            },
        ),
    )
    .await;
    assert_eq!(ack.status, fantastic_kernel::Status::Accepted, "{:?}", ack.result);
    assert!(t0.elapsed() < Duration::from_millis(500), "the ack waited for the job");
    let (o, at) = tokio::time::timeout(Duration::from_secs(20), rx)
        .await
        .expect("the run settled")
        .expect("delivered");
    assert!(at.duration_since(t0) >= Duration::from_millis(500), "settled before the exit");
    assert_eq!(o.status, fantastic_kernel::Status::Completed, "{}", o.result);
    assert_eq!(o.result["exit_code"], 4, "{}", o.result);
    assert_ne!(o.result["status"], "running", "{}", o.result);
    assert!(
        o.result["lines"]
            .as_array()
            .is_some_and(|l| l.iter().any(|row| row["line"].as_str() == Some("late"))),
        "{}",
        o.result
    );
}

/// The call's own DEADLINE bounds a `run` (no `wait_s`): past it the run settles
/// `timed_out` and its job is killed with it, never left running unowned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_past_its_deadline_times_out_and_its_job_dies() {
    let Some(py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let program = format!("{LF_STDIO}import time; time.sleep(30)");
    let t0 = std::time::Instant::now();
    let o = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(
            a.clone(),
            json!({"type": "run", "command": py.to_string_lossy(), "args": ["-u", "-c", program]}),
        )
        .with_timeout(0.5)])
        .await
        .remove(0);
    assert_eq!(o.status, fantastic_kernel::Status::TimedOut, "{}", o.result);
    assert!(t0.elapsed() < Duration::from_secs(5), "took {:?}", t0.elapsed());
    let mut live = true;
    for _ in 0..50 {
        live = jobs::jobs().values().any(|j| j.owner == a && jobs::alive(j));
        if !live {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!live, "the job outlived its run");
}

/// `run` refuses a `tail` below 1.
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
    let mut payload = base.clone();
    payload.as_object_mut().unwrap().insert("tail".to_string(), json!(0));
    let r = fantastic_kernel::test_support::send_settled(&kernel, vec![Item::new(a.clone(), payload)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "tail", "{r}");
}
