//! Unit tests for the exec_bridge bundle.
//!
//! Tests that must actually SPAWN find a real interpreter with `which` and WIRE
//! it onto the agent record, standing in for the operator — the resolver itself
//! never probes PATH. When the host has no python (rare on dev machines, common
//! on minimal CI containers) those tests return successfully — we don't want to
//! fail CI on systems without Python. The resolution tests need no interpreter
//! at all: they assert on paths, so they run everywhere.

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

/// Find a real Python interpreter to WIRE onto the test agent, or `None` to
/// skip. This is the TEST acting as the operator — the ALLOWLIST itself does
/// NOT probe PATH (that would invent an interpreter nobody named), so a test
/// that wants to spawn must list one on the record, exactly as an operator does.
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

/// A kernel + one exec_bridge agent with a real interpreter ALLOWLISTED on its
/// record when the host has one, and the leg OPENED. The wiring is deliberate:
/// the allowlist refuses rather than guessing off PATH, so an agent nobody
/// configured cannot spawn — which is what the operator does for real.
async fn mk_kernel(tmp: &TempDir) -> (Arc<Kernel>, AgentId) {
    let mut extra = Map::new();
    if let Some(py) = find_real_python() {
        extra.insert(
            "interpreters".to_string(),
            json!([py.to_string_lossy().to_string()]),
        );
    }
    // The gate is SEALED by default; these tests exercise the verbs, so the
    // operator opens the leg (the seal itself is covered separately). The record
    // names the python shim: the connector is now OPT-IN, and these tests are
    // exactly the programs that assume `kernel` in scope.
    extra.insert("ingress_rule".to_string(), json!("allow_all_ingress"));
    extra.insert("shim".to_string(), json!("python"));
    mk_kernel_with(tmp, extra).await
}

/// `mk_kernel`, but the agent record carries exactly the meta given — for the
/// tests that drive resolution itself and must control every tier.
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
        "type": "create_agent",
        "handler_module": HANDLER_MODULE,
        "id": pid,
    });
    let obj = create.as_object_mut().expect("object");
    for (k, v) in extra {
        obj.insert(k, v);
    }
    kernel.send(&AgentId::from("core"), create).await;
    (kernel, AgentId::from(pid.as_str()))
}

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("exec_bridge"));
}

/// The readme teaches the SCRIPT execution model, not "the `code` IS the tool".
/// Measured twice on gemma-4-12B (2026-08-06): that phrasing reads as "the code
/// DEFINES the tool", so the model authored `def run(payload): ...` and sent
/// `{type:'run'}` expecting a call — two inert agents, nothing written.
#[test]
fn readme_states_the_script_execution_model() {
    assert!(README.contains("a SCRIPT"), "{README}");
    assert!(README.contains("top to bottom"), "{README}");
    assert!(README.contains("ALLOWLIST"), "{README}");
    assert!(README.contains("NOT a sandbox"), "{README}");
}

/// No doc in this crate may present the retired `exec` verb as sendable. It is
/// GONE from `handle`, so a doc that still advertises it teaches a caller to send
/// something that falls through to the mail path and errors. The only licensed
/// mention is the sentence that DENIES the verb — that one is load-bearing
/// (a model that learned `exec` elsewhere needs to be told it is not here).
#[test]
fn no_doc_advertises_the_retired_exec_verb() {
    const LIB_SRC: &str = include_str!("lib.rs");
    for (which, text) in [("src/readme.md", README), ("src/lib.rs", LIB_SRC)] {
        for line in text.lines() {
            if !line.contains("`exec`") {
                continue;
            }
            assert!(
                line.contains("no `exec`"),
                "{which} documents the retired `exec` verb as if it existed: {line}"
            );
        }
    }
}

#[tokio::test]
async fn reflect_reports_resolved_interpreter() {
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;
    let r = kernel.send(&pid, json!({"type": "reflect"})).await;
    assert_eq!(r["id"], pid.as_str());
    // Py-canonical sentence: the process edge, sealed, allowlisted.
    assert_eq!(
        r["sentence"],
        "Process edge of the io family — sealed by default, allowlisted interpreters only. Spawns a program in the background; it gets a kernel wire over a private control fd."
    );
    assert_eq!(r["running"], 0);
    // The ALLOWLIST `mk_kernel` wired onto the record is advertised verbatim —
    // never something discovered off PATH. No python on the host ⇒ empty list.
    match find_real_python() {
        Some(py) => assert_eq!(r["interpreters"], json!([py.to_string_lossy().to_string()])),
        None => assert_eq!(r["interpreters"], json!([])),
    }
    // Verbs surface.
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

#[test]
fn allowlist_resolves_and_refuses_naming_the_legal_set() {
    // Empty allowlist ⇒ a loud refusal telling the operator what to set.
    let e = resolve_interpreter(&Map::new(), None).expect_err("empty allowlist must refuse");
    assert!(e.contains("interpreters=["), "must teach the fix: {e}");

    // One entry ⇒ requesting nothing is unambiguous and legal.
    let mut meta = Map::new();
    meta.insert("interpreters".to_string(), json!(["/venv/bin/python"]));
    assert_eq!(
        resolve_interpreter(&meta, None).unwrap(),
        "/venv/bin/python"
    );

    // Several ⇒ silence is ambiguous and refused, naming the set.
    meta.insert(
        "interpreters".to_string(),
        json!(["/a/bin/python", "/b/bin/python"]),
    );
    let e = resolve_interpreter(&meta, None).expect_err("ambiguity must refuse");
    assert!(e.contains("/a/bin/python"), "must name the set: {e}");

    // An unlisted request is refused NAMING the legal set — the mechanism
    // measured (2026-08-06) to let a small model recover on its next turn.
    let e = resolve_interpreter(&meta, Some("/evil/python")).expect_err("unlisted must refuse");
    assert!(e.contains("not allowlisted"), "{e}");
    assert!(e.contains("/b/bin/python"), "must name the set: {e}");

    // A listed request resolves; `~` expands on both sides.
    assert_eq!(
        resolve_interpreter(&meta, Some("/b/bin/python")).unwrap(),
        "/b/bin/python"
    );
}

#[test]
fn shim_is_optional_and_unknown_names_refuse() {
    // No shim ⇒ the raw program, untouched — the WIRE is the contract.
    assert_eq!(apply_shim(None, "print(1)").unwrap(), "print(1)");
    // The python shim prepends the connector.
    let with = apply_shim(Some("python"), "print(1)").unwrap();
    assert!(with.starts_with(CONNECTOR_SRC));
    assert!(with.ends_with("print(1)"));
    // Unknown ⇒ refusal naming the set, never a silent raw-wire pass-through.
    let e = apply_shim(Some("ruby"), "x").expect_err("unknown shim must refuse");
    assert!(e.contains("python"), "must name the legal set: {e}");
}

#[test]
fn argv_template_substitutes_and_defaults_to_cpython() {
    let argv = build_argv(&Map::new(), "/py", "CODE");
    assert_eq!(argv, vec!["/py", "-u", "-c", "CODE"]);
    let mut meta = Map::new();
    meta.insert(
        "argv_template".to_string(),
        json!(["<interp>", "-e", "<program>"]),
    );
    assert_eq!(build_argv(&meta, "/node", "JS"), vec!["/node", "-e", "JS"]);
}

/// Precedence is not a fallback: an ABSENT key still walks to the next tier.
/// Only an explicitly supplied one that cannot resolve refuses.
///
/// END TO END: the allowlist refusal reaches the caller as `start`'s reply, and
/// NOTHING is spawned — no job id, no row in the table. And the leg is SEALED
/// by default: with no `ingress_rule`, `start` denies before the allowlist is
/// even consulted, while `reflect` (discovery) still answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_refuses_unlisted_interpreter_and_sealed_leg_denies() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel
        .send(
            &a,
            json!({"type": "start", "code": "x = 1", "interpreter": "/no/such/python"}),
        )
        .await;
    assert!(
        r["error"]
            .as_str()
            .unwrap_or("")
            .contains("not allowlisted"),
        "an unlisted interpreter must refuse naming the set: {r}"
    );
    assert!(r["job_id"].is_null(), "nothing may be spawned: {r}");
    let st = kernel.send(&a, json!({"type": "status"})).await;
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
        .send(&sealed, json!({"type": "start", "code": "x"}))
        .await;
    assert_eq!(d["reason"], json!("unauthorized"), "{d}");
    assert!(
        d["hint"]
            .as_str()
            .unwrap_or("")
            .contains("ingress_rule=allow_all_ingress"),
        "the denial must teach how to open: {d}"
    );
    let refl = k2.send(&sealed, json!({"type": "reflect"})).await;
    assert_eq!(refl["sealed"], json!(true), "{refl}");
    assert!(
        refl["interpreters"].is_array(),
        "discovery must work sealed: {refl}"
    );
}

/// Reflect is how a caller LEARNS its wiring — the ALLOWLIST is advertised so
/// an authoring model never has to guess an interpreter path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_advertises_the_allowlist_and_shims() {
    let tmp = TempDir::new().unwrap();
    let mut meta = Map::new();
    meta.insert("interpreters".to_string(), json!(["/wired/bin/python"]));
    let (kernel, a) = mk_kernel_with(&tmp, meta).await;
    let r = kernel.send(&a, json!({"type": "reflect"})).await;
    assert_eq!(r["interpreters"], json!(["/wired/bin/python"]), "{r}");
    assert_eq!(r["shims"], json!(["python"]), "{r}");
    assert!(r["wire"]
        .as_str()
        .unwrap_or("")
        .contains("FANTASTIC_CTRL_FD"));
}

/// SIGINT reaches the job — the program catches it and exits cleanly.
/// Output arrives as `job_done`, not as `start`'s reply (which is the job id).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_sends_sigint_to_the_job() {
    let Some(_real) = find_real_python() else {
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
        .send(&pid, json!({"type": "start", "code": code}))
        .await;
    assert!(r["job_id"].is_string(), "start failed: {r}");
    // Let python install the handler before signalling it.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let n = kernel.send(&pid, json!({"type": "interrupt"})).await;
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
    let Some(_real) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, pid) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &pid);
    let r = kernel
        .send(
            &pid,
            json!({"type": "start", "code": "import time; time.sleep(30)"}),
        )
        .await;
    assert!(r["job_id"].is_string(), "start failed: {r}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let n = kernel.send(&pid, json!({"type": "stop"})).await;
    assert!(n["killed"].as_u64().unwrap_or(0) >= 1, "stop returned {n}");

    until(&mut rx, |v| v["type"] == "job_done", 20)
        .await
        .expect("no job_done after stop");
    let st = kernel.send(&pid, json!({"type": "status"})).await;
    let status = st["jobs"][0]["status"].as_str().unwrap_or("");
    assert_ne!(status, "running", "row left stuck running: {st}");
}

// ─── the vendored connector ─────────────────────────────────────

/// The vendored copy must be byte-identical to python's. A copy without this test
/// is worse than no copy: drift would surface as a program that runs on one kernel
/// and mysteriously fails on another, which is the hardest class of bug to trace.
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
/// Catches a corrupted or truncated vendor.
#[test]
fn connector_carries_the_wire_contract() {
    for needle in [
        "FANTASTIC_CTRL_FD",
        "FANTASTIC_SELF_ID",
        "FANTASTIC_ARGS",
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
/// the child at the advertised number? Everything else — relay, inbox, self-reap —
/// is built on this one fact.
///
/// Deliberately does NOT go through the bundle: it drives `spawn_job` directly, so a
/// failure points at the fd handoff and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_child_receives_the_control_socket_on_the_advertised_fd() {
    let Some(py) = find_real_python() else {
        eprintln!("skipping: no python3");
        return;
    };
    // The child reports what it sees. `S_IFSOCK` proves it is OUR socket and not some
    // inherited pipe that happened to land on the same number.
    let code = r#"
import os, stat
fd = int(os.environ["FANTASTIC_CTRL_FD"])
print("fd", fd, "sock", stat.S_ISSOCK(os.fstat(fd).st_mode), "self", os.environ["FANTASTIC_SELF_ID"], "args", os.environ["FANTASTIC_ARGS"])
"#;
    let argv = vec![
        py.to_string_lossy().to_string(),
        "-u".to_string(),
        "-c".to_string(),
        code.to_string(),
    ];
    let sp = crate::spawn::spawn_job(&argv, "owner_agent", r#"{"n":41}"#, None).expect("spawn");
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
        text.contains(r#"args {"n":41}"#),
        "args not delivered: {text}"
    );
}

/// The connector's lifeline: when the parent drops the control socket the child must
/// reap ITSELF (`_o._exit(0)`), even parked forever. This is what stops a dying kernel
/// leaving orphaned tools behind — and it only works because `spawn_job` drops the
/// parent's copy of the child end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_child_self_reaps_when_the_control_socket_closes() {
    let Some(py) = find_real_python() else {
        eprintln!("skipping: no python3");
        return;
    };
    // The self-reap lives in the CONNECTOR (its reader thread sees EOF and
    // `_o._exit(0)`s) — so this program takes the python shim, exactly as a
    // parked durable tool would.
    let code = "import threading\nthreading.Event().wait()\n";
    let program = apply_shim(Some("python"), code).unwrap();
    let argv = vec![
        py.to_string_lossy().to_string(),
        "-u".to_string(),
        "-c".to_string(),
        program,
    ];
    let mut sp = crate::spawn::spawn_job(&argv, "owner_agent", "{}", None).expect("spawn");
    // Parked. Nothing has killed it.
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

/// `start` returns AT ONCE — that is the contract. A blocking start would be the
/// retired run-and-wait verb, back under a new name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_returns_a_job_id_immediately() {
    let Some(_py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let t0 = std::time::Instant::now();
    let r = kernel
        .send(
            &a,
            json!({"type": "start", "code": "import time; time.sleep(30)"}),
        )
        .await;
    assert!(r["job_id"].is_string(), "no job_id: {r}");
    assert_eq!(r["status"], "running");
    assert!(t0.elapsed() < Duration::from_secs(5), "start blocked");
    kernel.send(&a, json!({"type": "stop"})).await;
}

/// One `progress` per line, and `n` is ONE sequence across stdout AND stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_streams_per_line_with_a_shared_counter() {
    let Some(_py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    kernel
        .send(
            &a,
            json!({"type": "start", "code":
                "import sys\nprint('o1')\nprint('e1', file=sys.stderr)\nprint('o2')\n"}),
        )
        .await;
    let done = until(&mut rx, |v| v["type"] == "job_done", 20)
        .await
        .expect("no job_done");
    assert_eq!(done["ok"], true, "job failed: {done}");
    assert!(done["stdout"].as_str().unwrap().contains("o1"));
    assert!(done["stderr"].as_str().unwrap().contains("e1"));
}

/// The connector round-trip: the program reaches ANOTHER agent by id and gets a reply.
/// This is the whole point of the control socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_program_can_send_to_another_agent_and_read_the_reply() {
    let Some(_py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    kernel
        .send(
            &a,
            json!({"type": "start", "code":
                "r = kernel.send('core', {'type': 'reflect'})\nprint('GOT', r.get('id'))\n"}),
        )
        .await;
    let done = until(&mut rx, |v| v["type"] == "job_done", 25)
        .await
        .expect("no job_done");
    assert!(
        done["stdout"].as_str().unwrap().contains("GOT core"),
        "connector send did not round-trip: {done}"
    );
}

/// `kernel.args` — this call's arguments, not something baked into the code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_call_args_reach_the_program() {
    let Some(_py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    kernel
        .send(
            &a,
            json!({"type": "start", "code": "print('N', kernel.args['n'] + 1)",
                   "args": {"n": 41}}),
        )
        .await;
    let done = until(&mut rx, |v| v["type"] == "job_done", 20)
        .await
        .expect("no job_done");
    assert!(done["stdout"].as_str().unwrap().contains("N 42"), "{done}");
}

/// An unknown verb is MAIL for the running program, ACKed `{queued:true}` — and
/// delivered exactly once (the fanout already did it; re-emitting would double it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_verb_is_queued_to_the_running_program() {
    let Some(_py) = find_real_python() else {
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let mut rx = inbox(&kernel, &a);
    // RAW string: a `\`-continued Rust literal strips the next line's leading
    // whitespace, which silently destroys Python indentation.
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
    kernel
        .send(&a, json!({"type": "start", "code": code}))
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let ack = kernel.send(&a, json!({"type": "ping"})).await;
    assert_eq!(ack["queued"], true, "not queued: {ack}");
    let pong = until(&mut rx, |v| v["type"] == "pong", 20)
        .await
        .expect("program never received the turn");
    assert_eq!(pong["count"], 1, "delivered more than once: {pong}");
    kernel.send(&a, json!({"type": "stop"})).await;
}

/// No running job ⇒ a loud error, never a silent drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_verb_without_a_job_errors_loudly() {
    let tmp = TempDir::new().unwrap();
    let (kernel, a) = mk_kernel(&tmp).await;
    let r = kernel.send(&a, json!({"type": "ping"})).await;
    assert!(
        r["error"].as_str().unwrap_or("").contains("no running job"),
        "{r}"
    );
}
