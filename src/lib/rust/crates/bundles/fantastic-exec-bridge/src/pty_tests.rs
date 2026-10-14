//! A terminal is a PTY JOB (terminal_backend merged in) — twins of python
//! `exec_bridge/tests/test_pty_jobs.py`, plus the record-owned environment
//! (`test_the_kernel_env_reaches_a_child_only_by_name`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fantastic_kernel::{Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

use crate::{ExecBridgeBundle, HANDLER_MODULE};

/// A tiny program that proves it sits on a terminal: its stdin is a tty, TERM is
/// set, the window size is the one asked for, and a typed line reaches it.
const CHILD: &str = "import fcntl,os,struct,sys,termios,signal
def size():
    r,c=struct.unpack('HHHH',fcntl.ioctl(0,termios.TIOCGWINSZ,b'\\0'*8))[:2]
    return c,r
signal.signal(signal.SIGWINCH, lambda *a: print('WINCH %d %d' % size(), flush=True))
print('TTY', os.isatty(0), os.environ.get('TERM'), flush=True)
print('SIZE %d %d' % size(), flush=True)
line = sys.stdin.readline()
print('GOT', line.strip(), flush=True)
";

fn python() -> Option<PathBuf> {
    which::which("python3").ok()
}

async fn kernel_with(tmp: &TempDir, id: &str, extra: Value) -> Arc<Kernel> {
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
    let mut create = json!({
        "type": "create_agent", "parent_id": "kernel", "id": id,
        "handler_module": HANDLER_MODULE, "ingress_rule": "allow_all_ingress",
        "cwd": tmp.path().to_string_lossy(), "allowed": [".*"],
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        create[k] = v;
    }
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], json!(id), "{r}");
    kernel
}

async fn call(kernel: &Arc<Kernel>, id: &str, p: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(id), p)])
        .await
        .remove(0)
        .result
}

/// Collect `output` text until `pred`, returning it.
async fn output_until(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    pred: impl Fn(&str) -> bool,
) -> String {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !pred(&text) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out; got {text:?}"
        );
        if let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            if item.payload["type"] == "output" {
                text.push_str(item.payload["data"].as_str().unwrap_or(""));
            }
        }
    }
    text
}

async fn next_of(rx: &mut fantastic_kernel::inbox::InboxReceiver, ty: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        assert!(tokio::time::Instant::now() < deadline, "no {ty}");
        if let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            if item.payload["type"] == ty {
                return item.payload;
            }
        }
    }
}

async fn start(kernel: &Arc<Kernel>, id: &str, py: &Path) -> i64 {
    let r = call(
        kernel,
        id,
        json!({"type": "start", "command": py.to_string_lossy(), "args": ["-c", CHILD],
               "pty": {"cols": 91, "rows": 37}}),
    )
    .await;
    assert_eq!(r["pty"], json!(true), "{r}");
    r["job_id"].as_i64().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pty_job_runs_on_a_terminal_and_takes_keystrokes() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let k = kernel_with(&tmp, "term_a", json!({})).await;
    let mut rx = k.ensure_inbox(&AgentId::from("term_a"));
    let jid = start(&k, "term_a", &py).await;
    let text = output_until(&mut rx, |t| t.contains("SIZE")).await;
    assert!(text.contains("TTY True xterm-256color"), "{text}");
    assert!(text.contains("SIZE 91 37"), "{text}");
    let w = call(
        &k,
        "term_a",
        json!({"type": "write", "job_id": jid, "data": "hello\n"}),
    )
    .await;
    assert_eq!(w, json!({"written": 6}));
    output_until(&mut rx, |t| t.contains("GOT hello")).await;
    let done = next_of(&mut rx, "job_done").await;
    assert_eq!(
        done,
        json!({"type": "job_done", "job_id": jid, "exit_code": 0, "ok": true})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_reaches_the_program_and_status_reports_it() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let k = kernel_with(&tmp, "term_r", json!({})).await;
    let mut rx = k.ensure_inbox(&AgentId::from("term_r"));
    let jid = start(&k, "term_r", &py).await;
    output_until(&mut rx, |t| t.contains("SIZE")).await;
    let r = call(
        &k,
        "term_r",
        json!({"type": "resize", "job_id": jid, "cols": 120, "rows": 40}),
    )
    .await;
    assert_eq!(r, json!({"resized": true, "cols": 120, "rows": 40}));
    output_until(&mut rx, |t| t.contains("WINCH 120 40")).await;
    let snap = call(&k, "term_r", json!({"type": "status", "job_id": jid})).await;
    let s = &snap["jobs"][0];
    assert_eq!(
        (&s["pty"], &s["cols"], &s["rows"]),
        (&json!(true), &json!(120), &json!(40)),
        "{snap}"
    );
    assert!(
        s.get("stdout").is_none(),
        "a PTY job has no stdout/stderr split: {snap}"
    );
    call(
        &k,
        "term_r",
        json!({"type": "write", "job_id": jid, "data": "x\n"}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_replays_the_ring_for_a_client_that_re_attaches() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let k = kernel_with(&tmp, "term_o", json!({})).await;
    let mut rx = k.ensure_inbox(&AgentId::from("term_o"));
    let jid = start(&k, "term_o", &py).await;
    output_until(&mut rx, |t| t.contains("SIZE")).await;
    let ring = call(&k, "term_o", json!({"type": "output", "job_id": jid})).await;
    let ring = ring["output"].as_str().unwrap().to_string();
    assert!(
        ring.contains("TTY True") && ring.contains("SIZE 91 37"),
        "{ring}"
    );
    let tail = call(
        &k,
        "term_o",
        json!({"type": "output", "job_id": jid, "max_bytes": 8}),
    )
    .await;
    let tail = tail["output"].as_str().unwrap();
    assert!(tail.len() <= 8 && ring.ends_with(tail), "{tail:?}");
    call(
        &k,
        "term_o",
        json!({"type": "write", "job_id": jid, "data": "x\n"}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pty_job_is_gated_like_any_start_and_its_verbs_want_a_pty_job() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let k = kernel_with(
        &tmp,
        "term_g",
        json!({"allowed": [format!("{} -c .*", py.display())]}),
    )
    .await;
    let r = call(
        &k,
        "term_g",
        json!({"type": "start", "command": "/bin/sh", "args": ["-i"], "pty": {"cols": 80, "rows": 24}}),
    )
    .await;
    assert_eq!(r["reason"], "unauthorized", "{r}");
    let r = call(
        &k,
        "term_g",
        json!({"type": "start", "command": py.to_string_lossy(), "args": ["-c", "0"], "pty": {"cols": 0}}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    let r = call(
        &k,
        "term_g",
        json!({"type": "write", "job_id": 999_999, "data": "x"}),
    )
    .await;
    assert_eq!(r["reason"], "not_found", "{r}");
    let pipe = call(
        &k,
        "term_g",
        json!({"type": "start", "command": py.to_string_lossy(), "args": ["-c", "input()"]}),
    )
    .await;
    let r = call(
        &k,
        "term_g",
        json!({"type": "write", "job_id": pipe["job_id"], "data": "x"}),
    )
    .await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("bad_arg"), &json!("job_id")),
        "{r}"
    );
    call(
        &k,
        "term_g",
        json!({"type": "stop", "job_id": pipe["job_id"]}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paste_image_types_a_saved_path() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let k = kernel_with(&tmp, "term_p", json!({})).await;
    let mut rx = k.ensure_inbox(&AgentId::from("term_p"));
    let jid = start(&k, "term_p", &py).await;
    output_until(&mut rx, |t| t.contains("SIZE")).await;
    // The mime is the caller's to state — never guessed.
    let item = Item::new(
        AgentId::from("term_p"),
        json!({"type": "paste_image", "job_id": jid}),
    )
    .with_binary_key("data", b"\x89PNG".to_vec());
    let r = k.send(vec![item]).await.remove(0).result;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("missing_arg"), &json!("mime")),
        "{r}"
    );
    let item = Item::new(
        AgentId::from("term_p"),
        json!({"type": "paste_image", "job_id": jid, "mime": "image/png"}),
    )
    .with_binary_key("data", b"\x89PNG....".to_vec());
    let r = k.send(vec![item]).await.remove(0).result;
    assert_eq!(r["bytes"], json!(8), "{r}");
    let path = r["path"].as_str().unwrap().to_string();
    assert!(path.ends_with(".png"), "{r}");
    call(
        &k,
        "term_p",
        json!({"type": "write", "job_id": jid, "data": "\n"}),
    )
    .await;
    let text = output_until(&mut rx, |t| t.contains("GOT") && t.contains(".png")).await;
    assert!(text.contains(&path), "{text}");
}

/// The kernel's environment reaches a child only by NAME (`inherit_env`); `["*"]`
/// is the operator's wildcard; nothing is inherited by default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_kernel_env_reaches_a_child_only_by_name() {
    std::env::set_var("FT_SECRET_RS", "s3cret");
    std::env::set_var("FT_OPEN_RS", "open");
    let line = json!(["-c", "echo ${FT_SECRET_RS:-none}/${FT_OPEN_RS:-none}"]);
    for (id, inherit, want) in [
        ("env_bare", Value::Null, "none/none"),
        ("env_named", json!(["FT_OPEN_RS"]), "none/open"),
        ("env_all", json!(["*"]), "s3cret/open"),
    ] {
        let tmp = TempDir::new().unwrap();
        let k = kernel_with(&tmp, id, json!({"inherit_env": inherit})).await;
        let mut rx = k.ensure_inbox(&AgentId::from(id));
        call(
            &k,
            id,
            json!({"type": "start", "command": "/bin/sh", "args": line}),
        )
        .await;
        let done = next_of(&mut rx, "job_done").await;
        assert_eq!(
            json!(crate::jobs::printed(&AgentId::from(id), &done, "stdout")),
            json!(format!("{want}\n")),
            "{id}: {done}"
        );
    }
}

/// A leg with no `cwd` runs nothing — where a command runs is never guessed.
#[tokio::test]
async fn a_leg_with_no_cwd_runs_nothing() {
    let tmp = TempDir::new().unwrap();
    let k = kernel_with(&tmp, "nocwd", json!({"cwd": null})).await;
    let r = call(
        &k,
        "nocwd",
        json!({"type": "start", "command": "/bin/sh", "args": ["-c", "true"]}),
    )
    .await;
    assert_eq!(
        (&r["reason"], &r["arg"]),
        (&json!("missing_arg"), &json!("cwd")),
        "{r}"
    );
}

fn in_session(leader: i32) -> Vec<i32> {
    crate::spawn::all_pids_for_tests()
        .into_iter()
        // SAFETY: `getsid` on a plain pid.
        .filter(|p| *p != leader && unsafe { libc::getsid(*p) } == leader)
        .collect()
}

fn exists(pid: i32) -> bool {
    // SAFETY: signal 0 probes existence only.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Measured (the CLI, 2026-09-28): a shell puts each job it starts in its OWN
/// process group inside the PTY session, so killing the leader alone (and the
/// tty's hangup, which reaches the foreground group only) left `sleep &` and a
/// `nohup` running, reparented to init. `stop` and `delete_agent` (the kernel's
/// shutdown runs the same teardown) now kill the whole session. Twin of python
/// `test_every_teardown_kills_the_whole_session`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_teardown_kills_the_whole_session() {
    for (aid, end) in [("sh_stop", "stop"), ("sh_delete", "delete_agent")] {
        let tmp = TempDir::new().unwrap();
        let k = kernel_with(
            &tmp,
            aid,
            json!({"allowed": ["/bin/sh -i"], "env": {"PATH": "/usr/bin:/bin"}}),
        )
        .await;
        let r = call(
            &k,
            aid,
            json!({"type": "start", "command": "/bin/sh", "args": ["-i"],
                   "pty": {"cols": 80, "rows": 24}}),
        )
        .await;
        let jid = r["job_id"].as_i64().expect("job_id");
        let leader = crate::jobs::jobs()
            .get(&(AgentId::from(aid), fantastic_kernel::TaskId(jid)))
            .expect("job row")
            .pid as i32;
        call(
            &k,
            aid,
            json!({"type": "write", "job_id": jid,
                   "data": "sleep 300 &\nnohup sleep 301 >/dev/null 2>&1 &\nsleep 302\n"}),
        )
        .await;
        let mut members = Vec::new();
        for _ in 0..100 {
            members = in_session(leader);
            if members.len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(members.len() >= 3, "{end}: {members:?}");
        if end == "stop" {
            call(&k, aid, json!({"type": "stop", "job_id": jid})).await;
        } else {
            call(
                &k,
                "kernel_state",
                json!({"type": "delete_agent", "id": aid}),
            )
            .await;
        }
        let mut alive: Vec<i32> = std::iter::once(leader).chain(members).collect();
        for _ in 0..100 {
            alive.retain(|p| exists(*p));
            if alive.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(alive.is_empty(), "{end}: survivors {alive:?}");
    }
}

/// A plain (pipe) job leads its own session too, so `stop` and `delete_agent`
/// take what it launched — a script's `sleep &` and a `nohup` used to outlive it.
/// Twin of python `test_stop_and_delete_kill_everything_a_job_launched`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_and_delete_kill_everything_a_job_launched() {
    let line = "sleep 303 & nohup sleep 304 >/dev/null 2>&1 & sleep 305";
    for (aid, end) in [("pipe_stop", "stop"), ("pipe_delete", "delete_agent")] {
        let tmp = TempDir::new().unwrap();
        let k = kernel_with(
            &tmp,
            aid,
            json!({"allowed": ["/bin/sh -c .*"], "env": {"PATH": "/usr/bin:/bin"}}),
        )
        .await;
        let r = call(
            &k,
            aid,
            json!({"type": "start", "command": "/bin/sh", "args": ["-c", line]}),
        )
        .await;
        let jid = r["job_id"].as_i64().expect("job_id");
        let leader = crate::jobs::jobs()
            .get(&(AgentId::from(aid), fantastic_kernel::TaskId(jid)))
            .expect("job row")
            .pid as i32;
        let mut members = Vec::new();
        for _ in 0..100 {
            members = in_session(leader);
            if members.len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(members.len() >= 3, "{end}: {members:?}");
        if end == "stop" {
            call(&k, aid, json!({"type": "stop", "job_id": jid})).await;
        } else {
            call(
                &k,
                "kernel_state",
                json!({"type": "delete_agent", "id": aid}),
            )
            .await;
        }
        let mut alive: Vec<i32> = std::iter::once(leader).chain(members).collect();
        for _ in 0..100 {
            alive.retain(|p| exists(*p));
            if alive.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(alive.is_empty(), "{end}: survivors {alive:?}");
    }
}

/// A `<call>` strips a scalar like `data`, so a key ending in Enter rides
/// `bytes`, written verbatim (owner 2026-10-04); exactly one of the two.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_bytes_is_verbatim_so_enter_reaches_the_program() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let k = kernel_with(&tmp, "term_b", json!({})).await;
    let mut rx = k.ensure_inbox(&AgentId::from("term_b"));
    let jid = start(&k, "term_b", &py).await;
    output_until(&mut rx, |t| t.contains("SIZE")).await;
    let both = call(
        &k,
        "term_b",
        json!({"type": "write", "job_id": jid, "data": "a", "bytes": "b"}),
    )
    .await;
    assert_eq!(both["arg"], json!("data"), "{both}");
    let w = call(
        &k,
        "term_b",
        json!({"type": "write", "job_id": jid, "bytes": "hello\n"}),
    )
    .await;
    assert_eq!(w, json!({"written": 6}));
    output_until(&mut rx, |t| t.contains("GOT hello")).await;
}
