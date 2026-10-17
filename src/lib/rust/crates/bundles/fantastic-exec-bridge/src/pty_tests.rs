//! A terminal is a PTY JOB (terminal_backend merged in) — twins of python
//! `exec_bridge/tests/test_pty_jobs.py`, plus the record-owned environment
//! (`test_the_kernel_env_reaches_a_child_only_by_name`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fantastic_kernel::{Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

use crate::sys::ENTER;
use crate::{ExecBridgeBundle, HANDLER_MODULE};

/// A tiny program that proves it sits on a terminal: its stdin is a tty, TERM is
/// set, the window size is the one asked for, and a typed line reaches it. A
/// resize is announced where the OS signals one (SIGWINCH).
const CHILD: &str = "import os,sys,signal
def size():
    return tuple(os.get_terminal_size(1))
if hasattr(signal, 'SIGWINCH'):
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
        json!({"type": "write", "job_id": jid, "data": format!("hello{ENTER}")}),
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
    // SIGWINCH is unix's: a Windows console program is not signalled a resize.
    #[cfg(unix)]
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
        json!({"type": "write", "job_id": jid, "data": format!("x{ENTER}")}),
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
    // the ring once the guest's last line is all in (it then waits on input):
    // the line's end can land a moment after its text
    let mut ring = String::new();
    loop {
        let now = call(&k, "term_o", json!({"type": "output", "job_id": jid})).await;
        let now = now["output"].as_str().unwrap().to_string();
        if now == ring {
            break;
        }
        ring = now;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
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
        json!({"type": "write", "job_id": jid, "data": format!("x{ENTER}")}),
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
        // the shell-joined line, the interpreter's path regex-escaped (a Windows
        // path's `\` is a regex escape)
        json!({"allowed": [format!(
            "{} -c .*",
            fancy_regex::escape(&crate::shlex_join(&[py.to_string_lossy().to_string()]))
        )]}),
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
        json!({"type": "write", "job_id": jid, "data": ENTER}),
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
    let (command, args) = crate::sys::print_env_job();
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
            json!({"type": "start", "command": command, "args": args}),
        )
        .await;
        let done = next_of(&mut rx, "job_done").await;
        // a console program on Windows ends its line with `\r\n`
        let printed = crate::jobs::printed(&AgentId::from(id), &done, "stdout");
        assert_eq!(
            json!(printed.replace("\r\n", "\n")),
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

/// The processes in the job `leader` leads, other than the leader: its session
/// on unix, its Job Object on Windows.
fn in_session(leader: u32) -> Vec<u32> {
    crate::sys::job_members(leader)
}

fn exists(pid: u32) -> bool {
    fantastic_kernel::lock::pid_alive(pid)
}

/// A shell puts each job it starts in its OWN process group inside the PTY
/// session, so killing the leader alone (and the tty's hangup, which reaches the
/// foreground group only) would leave `sleep &` and a `nohup` running,
/// reparented to init. `stop` and `delete_agent` (the kernel's shutdown runs the
/// same teardown) kill the whole session — on Windows, the job's Job Object. Twin
/// of python `test_every_teardown_kills_the_whole_session`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_teardown_kills_the_whole_session() {
    let (command, args, env, typed) = crate::sys::interactive_shell();
    for (aid, end) in [("sh_stop", "stop"), ("sh_delete", "delete_agent")] {
        let tmp = TempDir::new().unwrap();
        let k = kernel_with(&tmp, aid, json!({"env": env})).await;
        let r = call(
            &k,
            aid,
            json!({"type": "start", "command": command, "args": args,
                   "pty": {"cols": 80, "rows": 24}}),
        )
        .await;
        let jid = r["job_id"].as_i64().expect("job_id");
        let leader = crate::jobs::jobs()
            .get(&(AgentId::from(aid), fantastic_kernel::TaskId(jid)))
            .expect("job row")
            .pid;
        call(
            &k,
            aid,
            json!({"type": "write", "job_id": jid, "data": typed}),
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
        let mut alive: Vec<u32> = std::iter::once(leader).chain(members).collect();
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
/// take what it launched — a script's `sleep &` and a `nohup` included.
/// Twin of python `test_stop_and_delete_kill_everything_a_job_launched`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_and_delete_kill_everything_a_job_launched() {
    let (command, args, env) = crate::sys::launcher_job();
    for (aid, end) in [("pipe_stop", "stop"), ("pipe_delete", "delete_agent")] {
        let tmp = TempDir::new().unwrap();
        let k = kernel_with(&tmp, aid, json!({"env": env})).await;
        let r = call(
            &k,
            aid,
            json!({"type": "start", "command": command, "args": args}),
        )
        .await;
        let jid = r["job_id"].as_i64().expect("job_id");
        let leader = crate::jobs::jobs()
            .get(&(AgentId::from(aid), fantastic_kernel::TaskId(jid)))
            .expect("job row")
            .pid;
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
        let mut alive: Vec<u32> = std::iter::once(leader).chain(members).collect();
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
/// `bytes`, written verbatim; exactly one of the two.
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
        json!({"type": "write", "job_id": jid, "bytes": format!("hello{ENTER}")}),
    )
    .await;
    assert_eq!(w, json!({"written": 6}));
    output_until(&mut rx, |t| t.contains("GOT hello")).await;
}

/// Floods past the high-water mark, then prints a marker and waits on a line.
const FLOOD: &str = "import sys
sys.stdout.write('x' * 150000 + '\\nDONE\\n')
sys.stdout.flush()
sys.stdin.readline()
";

/// A viewer that streamed but never acked (a reloaded page) leaves `unacked` past
/// HIGH and the reader paused. The new viewer's `output` replay hands it everything
/// so far, so it zeroes the count and resumes the reader — python
/// `test_a_replay_clears_the_backlog_a_gone_viewer_left`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replay_clears_the_backlog_a_gone_viewer_left() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let k = kernel_with(&tmp, "term_f", json!({})).await;
    let mut rx = k.ensure_inbox(&AgentId::from("term_f"));
    let r = call(
        &k,
        "term_f",
        json!({"type": "start", "command": py.to_string_lossy(), "args": ["-c", FLOOD],
               "pty": {"cols": 80, "rows": 24}}),
    )
    .await;
    let jid = r["job_id"].as_i64().unwrap();
    let probe = json!({"type": "ack", "job_id": jid, "chars": 0});
    let mut st = json!({});
    for _ in 0..250 {
        st = call(&k, "term_f", probe.clone()).await;
        if st["paused"] == json!(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        st["paused"] == json!(true)
            && st["unacked"].as_u64().unwrap() > crate::pty::HIGH_WATERMARK as u64,
        "{st}"
    );
    call(&k, "term_f", json!({"type": "output", "job_id": jid})).await;
    let st = call(&k, "term_f", probe).await;
    // the count restarted at 0; the resumed reader may already have counted more
    assert!(
        st["paused"] == json!(false)
            && st["unacked"].as_u64().unwrap() < crate::pty::HIGH_WATERMARK as u64,
        "{st}"
    );
    // the rest of the flood (under HIGH) arrives with no further ack
    output_until(&mut rx, |t| t.contains("DONE")).await;
    call(
        &k,
        "term_f",
        json!({"type": "write", "job_id": jid, "data": ENTER}),
    )
    .await;
}
