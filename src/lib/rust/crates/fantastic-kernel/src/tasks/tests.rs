//! The TASK TABLE — `ps` + `cancel`, the kernel's process table.
//!
//! Mirrors Python's `tests/test_task_table.py` (the canonical contract).
//!
//! The unit is a TASK (ONE in-flight dispatch), not an agent: `"<agent>:<n>"`
//! is a pid, and tasks form a TREE through the send chain (a `send` issued
//! inside a handler is a CHILD of that handler's task). `ps` reads the live
//! table; `cancel` kills a task AND ITS SUBTREE, like killing a process group.
//!
//! Both are SYSTEM verbs — baked into the substrate, answered natively by every
//! agent, in no bundle's verb map. So an AGENT can cancel another agent's work
//! (`agent_cancels_another_agents_task`) — that is the point: cancellation is a
//! property of the one primitive, not a UI feature bolted onto one bundle.

use super::*;
use crate::agent::Agent;
use crate::bundle::{Bundle, BundleError, Reply};
use async_trait::async_trait;
use serde_json::Map;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

// ─── a test bundle: a handler that STAYS in flight + the on_cancel hook ───

/// What the test bundle records. `held` is a stand-in for a REAL resource (an
/// OS subprocess, a PTY): the slow handler takes it and only `on_cancel` gives
/// it back — aborting the future would leak it, which is the whole point of the
/// hook.
#[derive(Default)]
struct Shared {
    /// (agent, task_id, task_still_in_the_table_when_the_hook_ran)
    hooks: Mutex<Vec<(String, String, bool)>>,
    /// Resources currently held, keyed by the task holding them.
    held: Mutex<HashSet<String>>,
}

impl Shared {
    fn hook_log(&self) -> Vec<(String, String, bool)> {
        self.hooks.lock().expect("hooks poisoned").clone()
    }
    fn held_ids(&self) -> HashSet<String> {
        self.held.lock().expect("held poisoned").clone()
    }
}

struct SlowBundle {
    sh: Arc<Shared>,
}

#[async_trait]
impl Bundle for SlowBundle {
    fn name(&self) -> &str {
        "slow"
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "boot" | "shutdown" => Value::Null,
            "reflect" => json!({"id": agent_id.0, "sentence": "slow test agent"}),
            // In flight until someone kills us (or the test ends). Takes the
            // "resource" for the duration — released ONLY by `on_cancel`.
            "slow" => {
                if let Some(t) = current_task() {
                    self.sh.held.lock().expect("held poisoned").insert(t.0);
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
                json!({"finished": true}) // never reached in these tests
            }
            // A nested send — the callee's dispatch becomes a CHILD task of this one.
            "chain" => {
                let next = AgentId::from(payload.get("next").and_then(Value::as_str).unwrap_or(""));
                kernel.send(&next, json!({"type": "slow"})).await
            }
            // Work that OUTLIVES this dispatch: registered in the table, so `ps`
            // sees it and `cancel` can still reach it once the dispatch is gone.
            "fork" => {
                let sh = Arc::clone(&self.sh);
                let tid = kernel.spawn_task(agent_id, "job", async move {
                    if let Some(t) = current_task() {
                        sh.held.lock().expect("held poisoned").insert(t.0);
                    }
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
                json!({"task_id": tid.0})
            }
            // Fork long-lived work on a PEER, then stay in flight. The fork
            // outlives the `fork` dispatch and is re-parented to THIS task.
            "chain_fork" => {
                let next = AgentId::from(payload.get("next").and_then(Value::as_str).unwrap_or(""));
                let r = kernel.send(&next, json!({"type": "fork"})).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
                r
            }
            // An AGENT cancelling another agent's work: discover via `ps`, then
            // `cancel`. Plain verbs — no privileged surface, no bundle coupling.
            "kill" => {
                let victim = payload.get("victim").and_then(Value::as_str).unwrap_or("");
                let table = kernel
                    .send(&AgentId::from("kernel"), json!({"type": "ps"}))
                    .await;
                let found = table["tasks"]
                    .as_array()
                    .and_then(|rows| {
                        rows.iter()
                            .find(|t| t["agent"] == json!(victim) && t["verb"] == json!("slow"))
                            .cloned()
                    })
                    .and_then(|t| t["id"].as_str().map(str::to_string));
                match found {
                    None => json!({"error": "no victim task"}),
                    Some(task_id) => {
                        kernel
                            .send(
                                &AgentId::from("kernel"),
                                json!({"type": "cancel", "task_id": task_id}),
                            )
                            .await
                    }
                }
            }
            other => json!({"error": format!("unknown verb {other:?}")}),
        };
        Ok(Some(reply))
    }

    /// The optional bundle hook — the SIGTERM half. Records that it ran BEFORE
    /// the abort (the task is still in the table here), which is the whole
    /// contract: a bundle gets to release real resources while its handler still
    /// exists.
    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let alive = kernel.tasks.contains(task_id);
        self.sh.hooks.lock().expect("hooks poisoned").push((
            agent_id.0.clone(),
            task_id.0.clone(),
            alive,
        ));
        self.sh
            .held
            .lock()
            .expect("held poisoned")
            .remove(&task_id.0);
        Ok(())
    }
}

// ─── harness ───────────────────────────────────────────────────

fn mk_kernel(sh: Arc<Shared>) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register("slow.tools", SlowBundle { sh });
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        PathBuf::new(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    kernel
}

async fn mk_agent(kernel: &Arc<Kernel>, id: &str) -> AgentId {
    mk_agent_grant(kernel, id, Value::Null).await
}

/// Create a slow agent with an explicit `can_call` grant. Two-sided deny-all: a
/// `chain` send to a SIBLING is lateral egress, so the chain tests grant that edge.
async fn mk_agent_grant(kernel: &Arc<Kernel>, id: &str, can_call: Value) -> AgentId {
    let mut payload = json!({"type": "create_agent", "handler_module": "slow.tools", "id": id});
    if !can_call.is_null() {
        payload["can_call"] = can_call;
    }
    let r = kernel.send(&AgentId::from("core"), payload).await;
    assert_eq!(r["id"], json!(id), "create_agent failed: {r}");
    AgentId::from(id)
}

/// Create a slow agent that ACCEPTS a lateral caller (its `callable_by`) — the
/// ingress half a chain target needs under two-sided deny-all.
async fn mk_agent_accept(kernel: &Arc<Kernel>, id: &str, callable_by: Value) -> AgentId {
    let payload = json!({
        "type": "create_agent", "handler_module": "slow.tools", "id": id,
        "callable_by": callable_by,
    });
    let r = kernel.send(&AgentId::from("core"), payload).await;
    assert_eq!(r["id"], json!(id), "create_agent failed: {r}");
    AgentId::from(id)
}

/// The `ps` reply rows — through the VERB, like any caller.
async fn rows(kernel: &Arc<Kernel>) -> Vec<Value> {
    let r = kernel
        .send(&AgentId::from("kernel"), json!({"type": "ps"}))
        .await;
    r["tasks"].as_array().cloned().unwrap_or_default()
}

async fn row_for(kernel: &Arc<Kernel>, agent: &AgentId, verb: &str) -> Option<Value> {
    rows(kernel)
        .await
        .into_iter()
        .find(|t| t["agent"] == json!(agent.0) && t["verb"] == json!(verb))
}

async fn cancel(kernel: &Arc<Kernel>, task_id: &str) -> Value {
    kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type": "cancel", "task_id": task_id}),
        )
        .await
}

/// Poll until `f` holds (the handler is in flight), or fail the test.
async fn wait_until<F: Fn() -> bool>(what: &str, f: F) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Wait until `agent`'s `verb` dispatch shows up in the table, then return its id.
async fn wait_for_task(kernel: &Arc<Kernel>, agent: &AgentId, verb: &str) -> String {
    for _ in 0..300 {
        if let Some(row) = row_for(kernel, agent, verb).await {
            return row["id"].as_str().expect("task id").to_string();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {agent}'s {verb} task");
}

// ─── ps: the live table + the send TREE ────────────────────────

#[tokio::test]
async fn ps_shows_the_tree_of_a_nested_send() {
    // A `send` issued INSIDE a handler is a CHILD of that handler's task. `ps`
    // shows both rows with the parent link, so a caller sees who spawned what.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent_grant(&kernel, "slow_a", json!({"slow_b": "*"})).await;
    let b = mk_agent_accept(&kernel, "slow_b", json!({"slow_a": "*"})).await;

    let caller = tokio::spawn({
        let k = Arc::clone(&kernel);
        let a = a.clone();
        async move { k.send(&a, json!({"type": "chain", "next": "slow_b"})).await }
    });
    wait_for_task(&kernel, &b, "slow").await;

    let parent = row_for(&kernel, &a, "chain").await.expect("a's chain row");
    let child = row_for(&kernel, &b, "slow").await.expect("b's slow row");

    // Task id IS a pid: "<agent_id>:<8 hex>". The token is OPAQUE on purpose — a
    // visible counter reads as a SEQUENCE, and messaging is async (a send may return
    // once, several times, or never). Measured 2026-08-20 on python: a model inferred
    // ordering from the number and re-polled forever instead of matching on the id.
    for (row, agent) in [(&parent, &a), (&child, &b)] {
        let id = row["id"].as_str().expect("id");
        let (aid, token) = id.rsplit_once(':').expect("<agent>:<token>");
        assert_eq!(aid, agent.0);
        assert_eq!(token.len(), 8, "token is 8 hex chars, got {token:?}");
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "token is lowercase hex, got {token:?}"
        );
    }
    // The tree: b's dispatch is a child of a's dispatch. a's caller is the test
    // task — not a dispatch — so a's task is a root (init-owned).
    assert_eq!(child["parent"], parent["id"]);
    assert_eq!(parent["parent"], Value::Null);
    let keys: HashSet<&str> = parent
        .as_object()
        .expect("row object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        HashSet::from(["id", "agent", "parent", "verb", "started_at"])
    );
    assert!(parent["started_at"].is_f64(), "started_at is a float clock");
    // `ps` shows its own row too (it IS a dispatch) — the table is honest.
    assert!(rows(&kernel).await.iter().any(|t| t["verb"] == json!("ps")));

    caller.abort();
}

// ─── cancel: the subtree dies, the caller learns ───────────────

#[tokio::test]
async fn cancel_kills_the_whole_subtree() {
    // Cancelling A's task kills A AND the B dispatch A is blocked on — children
    // first, like killing a process group. The blocked caller's `send` RESOLVES
    // to {"cancelled": true}; it never raises.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent_grant(&kernel, "slow_a", json!({"slow_b": "*"})).await;
    let b = mk_agent_accept(&kernel, "slow_b", json!({"slow_a": "*"})).await;

    let caller = tokio::spawn({
        let k = Arc::clone(&kernel);
        let a = a.clone();
        async move { k.send(&a, json!({"type": "chain", "next": "slow_b"})).await }
    });
    let b_task = wait_for_task(&kernel, &b, "slow").await;
    let a_task = wait_for_task(&kernel, &a, "chain").await;

    let r = cancel(&kernel, &a_task).await;
    assert_eq!(r["cancelled"], json!(true));
    // Leaves-up: the child is killed before the parent that awaits it.
    assert_eq!(r["killed"], json!([b_task, a_task]));

    // The subtree is GONE from the table.
    let live: HashSet<String> = rows(&kernel)
        .await
        .iter()
        .filter_map(|t| t["id"].as_str().map(str::to_string))
        .collect();
    assert!(
        !live.contains(&a_task) && !live.contains(&b_task),
        "{live:?}"
    );

    // The blocked caller gets the cancellation as a REPLY, not a panic.
    let reply = tokio::time::timeout(Duration::from_secs(2), caller)
        .await
        .expect("caller must resolve")
        .expect("caller must not panic");
    assert_eq!(reply, json!({"cancelled": true}));

    // The on_cancel hook ran for BOTH agents, each with its own task id, and in
    // both cases BEFORE the abort (the task was still live in the table).
    let log = sh.hook_log();
    assert_eq!(
        log.iter()
            .map(|(agent, tid, _)| (agent.as_str(), tid.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (b.0.as_str(), b_task.as_str()),
            (a.0.as_str(), a_task.as_str())
        ],
    );
    assert!(
        log.iter().all(|(_, _, alive)| *alive),
        "hooks ran pre-abort"
    );
}

#[tokio::test]
async fn cancel_unknown_or_finished_task_is_honest() {
    // An unknown id — and an id that already finished — cancel NOTHING, and say
    // so: {"cancelled": false, "killed": []}. No pretending, no panic.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "slow_a").await;

    let r = cancel(&kernel, "no_such_agent:7").await;
    assert_eq!(r, json!({"cancelled": false, "killed": []}));

    // A task id that WAS live and is now finished (we just cancelled it).
    let caller = tokio::spawn({
        let k = Arc::clone(&kernel);
        let a = a.clone();
        async move { k.send(&a, json!({"type": "slow"})).await }
    });
    let task_id = wait_for_task(&kernel, &a, "slow").await;
    let first = cancel(&kernel, &task_id).await;
    assert_eq!(first["cancelled"], json!(true));
    let reply = tokio::time::timeout(Duration::from_secs(2), caller)
        .await
        .expect("caller must resolve")
        .expect("caller must not panic");
    assert_eq!(reply, json!({"cancelled": true}));

    let again = cancel(&kernel, &task_id).await;
    assert_eq!(again, json!({"cancelled": false, "killed": []}));

    // A malformed call fails LOUD; it does not report a phantom success.
    let bad = kernel
        .send(&AgentId::from("kernel"), json!({"type": "cancel"}))
        .await;
    assert!(bad["error"].is_string(), "expected an error: {bad}");
    assert!(bad.get("cancelled").is_none());
}

// ─── the emergent one: an AGENT cancels another agent's work ───

#[tokio::test]
async fn agent_cancels_another_agents_task() {
    // `cancel` is a PLAIN VERB, so an agent wields it: the killer agent's own
    // handler discovers the victim's task through `ps` and sends `cancel`. No
    // UI, no privileged caller — the capability is in the substrate.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let victim = mk_agent(&kernel, "slow_victim").await;
    let killer = mk_agent(&kernel, "slow_killer").await;

    let caller = tokio::spawn({
        let k = Arc::clone(&kernel);
        let v = victim.clone();
        async move { k.send(&v, json!({"type": "slow"})).await }
    });
    let victim_task = wait_for_task(&kernel, &victim, "slow").await;

    let r = kernel
        .send(&killer, json!({"type": "kill", "victim": victim.0}))
        .await;
    assert_eq!(r["cancelled"], json!(true));
    assert_eq!(r["killed"], json!([victim_task]));

    let live: HashSet<String> = rows(&kernel)
        .await
        .iter()
        .filter_map(|t| t["id"].as_str().map(str::to_string))
        .collect();
    assert!(!live.contains(&victim_task));
    let reply = tokio::time::timeout(Duration::from_secs(2), caller)
        .await
        .expect("caller must resolve")
        .expect("caller must not panic");
    assert_eq!(reply, json!({"cancelled": true}));
}

// ─── on_cancel: the reliability hook ───────────────────────────

#[tokio::test]
async fn on_cancel_hook_runs_and_releases_its_resource() {
    // Aborting a future frees NO real resource. The slow handler TAKES one and
    // never gives it back on its own — only the hook does. Cancel must therefore
    // leave nothing held (this is the orphaned-subprocess class of bug).
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "slow_a").await;

    let caller = tokio::spawn({
        let k = Arc::clone(&kernel);
        let a = a.clone();
        async move { k.send(&a, json!({"type": "slow"})).await }
    });
    let task_id = wait_for_task(&kernel, &a, "slow").await;
    wait_until("the resource to be held", || {
        sh.held_ids().contains(&task_id)
    })
    .await;

    let r = cancel(&kernel, &task_id).await;
    assert_eq!(r["cancelled"], json!(true));

    assert!(
        sh.held_ids().is_empty(),
        "on_cancel must release the resource: {:?}",
        sh.held_ids()
    );
    let log = sh.hook_log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].0, a.0);
    assert_eq!(log[0].1, task_id);
    assert!(log[0].2, "the hook ran BEFORE the abort");
    let _ = tokio::time::timeout(Duration::from_secs(2), caller).await;
}

// ─── long-lived work: init-adoption + reachable by cancel ──────

#[tokio::test]
async fn forked_work_outlives_its_dispatch_and_is_adopted_by_its_grandparent() {
    // Work can OUTLIVE the dispatch that spawned it (a job, a pump). The bundle
    // registers it with `kernel.spawn_task`, so `ps` sees it; when the dispatch
    // that forked it dies, the job is RE-PARENTED to that dispatch's parent
    // (init-adoption, exactly like Linux) — which is what makes "cancel the
    // turn" also kill the jobs the turn spawned.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent_grant(&kernel, "slow_a", json!({"slow_b": "*"})).await;
    let b = mk_agent_accept(&kernel, "slow_b", json!({"slow_a": "*"})).await;

    let caller = tokio::spawn({
        let k = Arc::clone(&kernel);
        let a = a.clone();
        async move {
            k.send(&a, json!({"type": "chain_fork", "next": "slow_b"}))
                .await
        }
    });
    let a_task = wait_for_task(&kernel, &a, "chain_fork").await;
    let job = wait_for_task(&kernel, &b, "job").await;

    // The `fork` dispatch that spawned the job is GONE (it replied at once) —
    // but the job lives on, re-parented to the task that forked it.
    for _ in 0..300 {
        if row_for(&kernel, &b, "fork").await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(row_for(&kernel, &b, "fork").await.is_none(), "fork retired");
    let job_row = row_for(&kernel, &b, "job").await.expect("job row");
    assert_eq!(job_row["parent"], json!(a_task), "init-adoption");

    // Killing the OUTER task reaches the forked job through the adopted link.
    let r = cancel(&kernel, &a_task).await;
    assert_eq!(r["cancelled"], json!(true));
    assert_eq!(r["killed"], json!([job, a_task]));
    assert!(
        sh.held_ids().is_empty(),
        "the job's on_cancel must release it: {:?}",
        sh.held_ids()
    );
    let reply = tokio::time::timeout(Duration::from_secs(2), caller)
        .await
        .expect("caller must resolve")
        .expect("caller must not panic");
    assert_eq!(reply, json!({"cancelled": true}));
}

// ─── reflect advertises the substrate's own verbs ──────────────

#[tokio::test]
async fn root_reflect_advertises_ps_and_cancel() {
    // `ps`/`cancel` are universal system verbs — they belong to no bundle, so
    // the ROOT reflect advertises them. One round-trip tells a caller that work
    // in flight is inspectable and killable.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "slow_a").await;

    let r = kernel
        .send(&AgentId::from("kernel"), json!({"type": "reflect"}))
        .await;
    let sv = &r["system_verbs"];
    let cancel_doc = sv["cancel"].as_str().expect("cancel doc");
    assert!(cancel_doc.contains("task_id"));
    assert!(cancel_doc.to_lowercase().contains("subtree"));
    assert!(sv["ps"].as_str().expect("ps doc").contains("tasks"));
    for v in [
        "create_agent",
        "update_agent",
        "delete_agent",
        "list_agents",
    ] {
        assert!(sv[v].is_string(), "missing system verb {v}");
    }
    // Non-root agents don't repeat it (root-only, like `runtime`).
    let child = kernel.send(&a, json!({"type": "reflect"})).await;
    assert!(child.get("system_verbs").is_none());
}
