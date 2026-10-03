//! A job is DRIVEN by the caller that started it — the operator, always. The twin
//! of python `exec_bridge/tests/test_job_starter.py`.
//!
//! Audit AV-8 (2026-10-03): every job verb checked only that the job belonged to
//! THIS exec_bridge agent, so any caller granted `write` on a shared leg typed into
//! another caller's shell (a shell job carries the agent's FANTASTIC_CTRL_FD
//! identity) and killed its jobs. Now `start` records the job's `starter` (the
//! calling agent, a foreign caller's `<guid>|<agent>`, or null for the operator)
//! and the verbs that DRIVE a job — `write`, `resize`, `paste_image`, `stop`,
//! `interrupt`, `clear` — refuse a named job another caller started; their bulk
//! forms act on the caller's own jobs.
//!
//! The READ verbs — `status`, `output`, `ack` — stay gated by grants alone: the
//! documented read-only viewer (granted `status`/`output`/`ack`, never `start`) is
//! by construction NOT the starter, so scoping them to the starter would end it.

use std::path::PathBuf;
use std::sync::Arc;

use fantastic_kernel::{Agent, AgentId, Item, Kernel, TaskId};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

use crate::{jobs, ExecBridgeBundle, HANDLER_MODULE};

/// Prints a line, then waits for one: a live PTY job until it is written to.
const CHILD: &str =
    "import sys\nprint('READY', flush=True)\nprint('GOT', sys.stdin.readline().strip(), flush=True)\n";

fn python() -> Option<PathBuf> {
    which::which("python3").ok()
}

/// python's `term` fixture: an open exec_bridge `term` that `a` and `b` may both
/// call. The id is unique per test — the job table is process-global.
async fn term(tmp: &TempDir) -> (Arc<Kernel>, String) {
    let mut kernel = Kernel::new();
    kernel.bundles.register(HANDLER_MODULE, ExecBridgeBundle);
    let k = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        tmp.path().join(".fantastic"),
        false,
    );
    k.register(Arc::clone(&root));
    k.set_root(Arc::clone(&root));
    let id = format!(
        "term_{}",
        tmp.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace('.', "_")
    );
    let r = k
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "id": id,
                   "handler_module": HANDLER_MODULE, "ingress_rule": "allow_all_ingress",
                   "cwd": tmp.path().to_string_lossy(), "allowed": [".*"],
                   "callable_by": {"a": "*", "b": "*"}}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], json!(id), "{r}");
    for who in ["a", "b"] {
        k.register(Agent::new(
            AgentId::from(who),
            None,
            Some(AgentId::from("kernel_state")),
            json!({"can_call": {id.as_str(): "*"}})
                .as_object()
                .cloned()
                .unwrap(),
            PathBuf::new(),
            false,
        ));
    }
    (k, id)
}

async fn call(k: &Arc<Kernel>, id: &str, payload: Value, as_: Option<&str>) -> Value {
    call_item(k, Item::new(AgentId::from(id), payload), as_).await
}

async fn call_item(k: &Arc<Kernel>, item: Item, as_: Option<&str>) -> Value {
    match as_ {
        None => k.send(vec![item]).await.remove(0).result,
        Some(who) => {
            fantastic_kernel::send::with_sender(AgentId::from(who), k.send(vec![item]))
                .await
                .remove(0)
                .result
        }
    }
}

async fn start_as(k: &Arc<Kernel>, id: &str, py: &std::path::Path, who: Option<&str>) -> i64 {
    let r = call(
        k,
        id,
        json!({"type": "start", "command": py.to_string_lossy(), "args": ["-c", CHILD],
               "pty": {"cols": 80, "rows": 24}}),
        who,
    )
    .await;
    assert_eq!(r["pty"], json!(true), "{r}");
    r["job_id"].as_i64().unwrap()
}

fn not_yours(verb: &str, jid: i64) -> Value {
    json!({
        "error": format!(
            "exec_bridge: job {jid} is not yours — only the caller that started it (or the \
             operator) may {verb} it"
        ),
        "reason": "unauthorized",
        "job_id": jid,
    })
}

/// `(alive, starter)` of the job, off the table.
fn row(id: &str, jid: i64) -> (bool, Option<String>) {
    let g = jobs::jobs();
    let j = g
        .get(&(AgentId::from(id), TaskId(jid)))
        .expect("the job is in the table");
    (jobs::alive(j), j.starter.clone())
}

async fn done(k: &Arc<Kernel>, id: &str) {
    call(k, id, json!({"type": "stop"}), None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_caller_cannot_drive_a_job() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let (k, id) = term(&tmp).await;
    let jid = start_as(&k, &id, &py, Some("a")).await;
    for (verb, extra) in [
        ("write", json!({"data": "hijack\n"})),
        ("resize", json!({"cols": 100, "rows": 30})),
        ("stop", json!({})),
        ("interrupt", json!({})),
        ("clear", json!({})),
    ] {
        let mut p = json!({"type": verb, "job_id": jid});
        for (key, v) in extra.as_object().cloned().unwrap() {
            p[key] = v;
        }
        let out = call(&k, &id, p, Some("b")).await;
        assert_eq!(out, not_yours(verb, jid), "{verb}");
    }
    // paste_image carries its image raw, on the binary channel
    let paste = Item::new(
        AgentId::from(id.as_str()),
        json!({"type": "paste_image", "job_id": jid, "mime": "image/png"}),
    )
    .with_binary_key("data", b"\x89PNG".to_vec());
    let out = call_item(&k, paste, Some("b")).await;
    assert_eq!(out, not_yours("paste_image", jid));
    assert!(row(&id, jid).0, "b's verbs left a's job running");
    // the bulk forms act on b's own jobs — none
    assert_eq!(
        call(&k, &id, json!({"type": "stop"}), Some("b")).await,
        json!({"killed": 0})
    );
    assert_eq!(
        call(&k, &id, json!({"type": "interrupt"}), Some("b")).await,
        json!({"interrupted": 0})
    );
    assert_eq!(row(&id, jid), (true, Some("a".to_string())));
    done(&k, &id).await;
}

/// The viewer pattern the readme documents: granted only `status`/`output`/`ack`,
/// it re-attaches to a terminal someone else started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_viewer_reads_a_job_it_did_not_start() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let (k, id) = term(&tmp).await;
    let jid = start_as(&k, &id, &py, Some("a")).await;
    let listed = call(&k, &id, json!({"type": "status"}), Some("b")).await;
    let ids: Vec<Value> = listed["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["job_id"].clone())
        .collect();
    assert_eq!(ids, vec![json!(jid)], "{listed}");
    let one = call(&k, &id, json!({"type": "status", "job_id": jid}), Some("b")).await;
    assert_eq!(one["jobs"][0]["job_id"], json!(jid), "{one}");
    let ring = call(&k, &id, json!({"type": "output", "job_id": jid}), Some("b")).await;
    assert!(
        ring["job_id"] == json!(jid) && ring["output"].is_string(),
        "{ring}"
    );
    let acked = call(
        &k,
        &id,
        json!({"type": "ack", "job_id": jid, "chars": 1}),
        Some("b"),
    )
    .await;
    let mut keys: Vec<&String> = acked.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["paused", "unacked"], "{acked}");
    done(&k, &id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_starter_and_the_operator_drive_it() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let (k, id) = term(&tmp).await;
    let jid = start_as(&k, &id, &py, Some("a")).await;
    assert_eq!(
        call(
            &k,
            &id,
            json!({"type": "resize", "job_id": jid, "cols": 90, "rows": 30}),
            Some("a")
        )
        .await,
        json!({"resized": true, "cols": 90, "rows": 30})
    );
    let mine = call(&k, &id, json!({"type": "status"}), Some("a")).await;
    assert_eq!(mine["jobs"][0]["job_id"], json!(jid), "{mine}");
    let everyone = call(&k, &id, json!({"type": "status"}), None).await;
    assert_eq!(everyone["jobs"][0]["job_id"], json!(jid), "{everyone}");
    assert_eq!(
        call(
            &k,
            &id,
            json!({"type": "write", "job_id": jid, "data": "op\n"}),
            None
        )
        .await,
        json!({"written": 3})
    );
    done(&k, &id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operator_started_job_is_the_operators() {
    let Some(py) = python() else { return };
    let tmp = TempDir::new().unwrap();
    let (k, id) = term(&tmp).await;
    let jid = start_as(&k, &id, &py, None).await;
    assert_eq!(row(&id, jid).1, None);
    let out = call(
        &k,
        &id,
        json!({"type": "write", "job_id": jid, "data": "x\n"}),
        Some("a"),
    )
    .await;
    assert_eq!(out, not_yours("write", jid));
    done(&k, &id).await;
}
