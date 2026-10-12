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
use crate::outcome::Item;

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
pub(crate) struct Shared {
    /// (agent, task_id, task_still_in_the_table_when_the_hook_ran)
    hooks: Mutex<Vec<(String, i64, bool)>>,
    /// Resources currently held, keyed by the task holding them.
    held: Mutex<HashSet<i64>>,
    /// How many times a counting handler actually RAN.
    runs: Mutex<usize>,
    /// C87: when set, `on_cancel` answers `Err` instead of releasing the
    /// resource — exercises the "a hook that fails is logged and the kill
    /// still happens" path (`tasks.rs`'s `  [cancel] <id> bundle on_cancel
    /// raised: <e>` line).
    fail_on_cancel: std::sync::atomic::AtomicBool,
}

impl Shared {
    pub(crate) fn hook_log(&self) -> Vec<(String, i64, bool)> {
        self.hooks.lock().expect("hooks poisoned").clone()
    }
    pub(crate) fn run_count(&self) -> usize {
        *self.runs.lock().expect("runs poisoned")
    }
    pub(crate) fn held_ids(&self) -> HashSet<i64> {
        self.held.lock().expect("held poisoned").clone()
    }
    pub(crate) fn set_fail_on_cancel(&self, fail: bool) {
        self.fail_on_cancel
            .store(fail, std::sync::atomic::Ordering::SeqCst);
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
    // ADVERTISES `shutdown`: that is what `shutdown_agents` selects on, so the
    // slow agent is one it tells.
    fn verb_docs(&self) -> Value {
        json!({"shutdown": "no-op"})
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
            // `reflect_ms` on the record holds the reflect — what a deadline on a
            // whole-tree reflect has to cut short.
            "reflect" => {
                let ms = kernel
                    .agents
                    .get(agent_id)
                    .and_then(|a| a.meta.read().ok()?.get("reflect_ms")?.as_u64());
                if let Some(ms) = ms {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                json!({
                    "id": agent_id.0,
                    "sentence": "slow test agent",
                })
            }
            // A RETURNED dict that merely carries `raised: true` — a relayed
            // peer's answer. Counted, so a retry would show.
            "raised_reply" => {
                *self.sh.runs.lock().expect("runs poisoned") += 1;
                json!({"error": "RuntimeError: on the peer", "raised": true})
            }
            // Returns immediately — the peer a batch uses to prove the slow item
            // did not serialize the fast one.
            "fast" => json!({"fast": true}),
            // A handler that PANICS. The kernel settles this as an outcome, so
            // the batch's other items are untouched and `send` returns normally.
            "boom" => panic!("kaput"),
            // Counts how many times the HANDLER actually ran under this id —
            // the only honest way to assert "exactly once".
            "count" => {
                let n = {
                    let mut g = self.sh.runs.lock().expect("runs poisoned");
                    *g += 1;
                    *g
                };
                json!({"run": n})
            }
            // Fails until the Nth run — the retry engine's canonical target.
            "flaky" => {
                let n = {
                    let mut g = self.sh.runs.lock().expect("runs poisoned");
                    *g += 1;
                    *g
                };
                let need = payload
                    .get("succeed_on")
                    .and_then(Value::as_u64)
                    .unwrap_or(3);
                if (n as u64) < need {
                    panic!("flaky miss {n}");
                }
                json!({"made_it": n})
            }
            // In flight until someone kills us (or the test ends). Takes the
            // "resource" for the duration — released ONLY by `on_cancel`.
            "slow" => {
                if let Some(t) = current_task() {
                    self.sh.held.lock().expect("held poisoned").insert(t.0);
                }
                // `hold_ms` — a dispatch that really does settle, for the
                // shutdown quiet-phase twin. Default: outlives the test.
                let ms = payload
                    .get("hold_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(30_000);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                json!({"finished": true}) // never reached in most of these tests
            }
            // A nested send — the callee's dispatch becomes a CHILD task of this one.
            "chain" => {
                let next = AgentId::from(payload.get("next").and_then(Value::as_str).unwrap_or(""));
                kernel
                    .send(vec![Item::new(next, json!({"type": "slow"}))])
                    .await
                    .remove(0)
                    .result
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
                let r = kernel
                    .send(vec![Item::new(next, json!({"type": "fork"}))])
                    .await
                    .remove(0)
                    .result;
                tokio::time::sleep(Duration::from_secs(30)).await;
                r
            }
            // An AGENT cancelling another agent's work: discover via `ps`, then
            // `cancel`. Plain verbs — no bundle coupling.
            "kill" => {
                let victim = payload.get("victim").and_then(Value::as_str).unwrap_or("");
                let table = kernel
                    .send(vec![Item::new(
                        AgentId::from("kernel"),
                        json!({"type": "ps"}),
                    )])
                    .await
                    .remove(0)
                    .result;
                let found = table["tasks"]
                    .as_array()
                    .and_then(|rows| {
                        rows.iter()
                            .find(|t| t["agent"] == json!(victim) && t["verb"] == json!("slow"))
                            .cloned()
                    })
                    .and_then(|t| t["id"].as_i64());
                match found {
                    None => json!({"error": "no victim task"}),
                    Some(task_id) => {
                        kernel
                            .send(vec![Item::new(
                                AgentId::from("kernel"),
                                json!({"type": "cancel", "task_id": task_id}),
                            )])
                            .await
                            .remove(0)
                            .result
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
        self.sh
            .hooks
            .lock()
            .expect("hooks poisoned")
            .push((agent_id.0.clone(), task_id.0, alive));
        self.sh
            .held
            .lock()
            .expect("held poisoned")
            .remove(&task_id.0);
        if self
            .sh
            .fail_on_cancel
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err("boom".into());
        }
        Ok(())
    }
}

// ─── harness ───────────────────────────────────────────────────

pub(crate) fn mk_kernel(sh: Arc<Shared>) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register("slow.tools", SlowBundle { sh });
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        PathBuf::new(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    kernel
}

pub(crate) async fn mk_agent(kernel: &Arc<Kernel>, id: &str) -> AgentId {
    mk_agent_grant(kernel, id, Value::Null).await
}

/// Create a slow agent with an explicit `can_call` grant. Two-sided deny-all: a
/// `chain` send to a SIBLING is lateral egress, so the chain tests grant that edge.
/// A slow agent whose `can_call` is `can_call` — and, both ends being explicit,
/// every live agent that grant names accepts it back (its `callable_by` names
/// `id` with the same verbs).
pub(crate) async fn mk_agent_grant(kernel: &Arc<Kernel>, id: &str, can_call: Value) -> AgentId {
    let mut payload = json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "slow.tools", "id": id});
    if !can_call.is_null() {
        payload["can_call"] = can_call.clone();
    }
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), payload)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], json!(id), "create_agent failed: {r}");
    if let Some(map) = can_call.as_object() {
        for (k, v) in map {
            if !k.starts_with('$') && kernel.resolve_target(k).is_some() {
                crate::test_support::grant(kernel, k, "callable_by", id, v.clone());
            }
        }
    }
    AgentId::from(id)
}

/// Create a slow agent that ACCEPTS a lateral caller (its `callable_by`) — the
/// ingress half a chain target needs under two-sided deny-all.
async fn mk_agent_accept(kernel: &Arc<Kernel>, id: &str, callable_by: Value) -> AgentId {
    let payload = json!({
        "type": "create_agent", "parent_id": "kernel", "handler_module": "slow.tools", "id": id,
        "callable_by": callable_by,
    });
    let r = kernel
        .send(vec![Item::new(AgentId::from("kernel_state"), payload)])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], json!(id), "create_agent failed: {r}");
    AgentId::from(id)
}

/// The `ps` reply rows — through the VERB, like any caller.
async fn rows(kernel: &Arc<Kernel>) -> Vec<Value> {
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ps"}),
        )])
        .await
        .remove(0)
        .result;
    r["tasks"].as_array().cloned().unwrap_or_default()
}

async fn row_for(kernel: &Arc<Kernel>, agent: &AgentId, verb: &str) -> Option<Value> {
    rows(kernel)
        .await
        .into_iter()
        .find(|t| t["agent"] == json!(agent.0) && t["verb"] == json!(verb))
}

async fn cancel(kernel: &Arc<Kernel>, task_id: i64) -> Value {
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "cancel", "task_id": task_id}),
        )])
        .await
        .remove(0)
        .result
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
async fn wait_for_task(kernel: &Arc<Kernel>, agent: &AgentId, verb: &str) -> i64 {
    for _ in 0..300 {
        if let Some(row) = row_for(kernel, agent, verb).await {
            return row["id"].as_i64().expect("task id");
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
        async move {
            k.send(vec![Item::new(
                a,
                json!({"type": "chain", "next": "slow_b"}),
            )])
            .await
            .remove(0)
            .result
        }
    });
    wait_for_task(&kernel, &b, "slow").await;

    let parent = row_for(&kernel, &a, "chain").await.expect("a's chain row");
    let child = row_for(&kernel, &b, "slow").await.expect("b's slow row");

    // A TASK ID IS A MESSAGE ID — the one counter's number, positive and
    // ascending. It used to be an opaque `<agent>:<8 hex>` token, hashed so a
    // model could not read an ordering into it (measured 2026-08-20 in python:
    // one inferred ordering from `<agent>:<n>` and re-polled forever). Python
    // reversed that trade: one number naming one thing in `ps`, `cancel`, the
    // ledger, a reply tag and the message log is worth more than a number that
    // hides its order.
    for row in [&parent, &child] {
        assert!(row["id"].as_i64().expect("id") > 0);
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
        async move {
            k.send(vec![Item::new(
                a,
                json!({"type": "chain", "next": "slow_b"}),
            )])
            .await
            .remove(0)
            .result
        }
    });
    let b_task = wait_for_task(&kernel, &b, "slow").await;
    let a_task = wait_for_task(&kernel, &a, "chain").await;

    let r = cancel(&kernel, a_task).await;
    assert_eq!(r["cancelled"], json!(true));
    // Leaves-up: the child is killed before the parent that awaits it.
    assert_eq!(r["killed"], json!([b_task, a_task]));

    // The subtree is GONE from the table.
    let live: HashSet<i64> = rows(&kernel)
        .await
        .iter()
        .filter_map(|t| t["id"].as_i64())
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
            .map(|(agent, tid, _)| (agent.as_str(), *tid))
            .collect::<Vec<_>>(),
        vec![(b.0.as_str(), b_task), (a.0.as_str(), a_task)],
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

    // An id the counter never handed out: nothing to kill, and it says so.
    let r = cancel(&kernel, 999_999).await;
    assert_eq!(r, json!({"cancelled": false, "killed": []}));

    // A task id that WAS live and is now finished (we just cancelled it).
    let caller = tokio::spawn({
        let k = Arc::clone(&kernel);
        let a = a.clone();
        async move {
            k.send(vec![Item::new(a, json!({"type": "slow"}))])
                .await
                .remove(0)
                .result
        }
    });
    let task_id = wait_for_task(&kernel, &a, "slow").await;
    let first = cancel(&kernel, task_id).await;
    assert_eq!(first["cancelled"], json!(true));
    let reply = tokio::time::timeout(Duration::from_secs(2), caller)
        .await
        .expect("caller must resolve")
        .expect("caller must not panic");
    assert_eq!(reply, json!({"cancelled": true}));

    let again = cancel(&kernel, task_id).await;
    assert_eq!(again, json!({"cancelled": false, "killed": []}));

    // A malformed call fails LOUD; it does not report a phantom success.
    let bad = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "cancel"}),
        )])
        .await
        .remove(0)
        .result;
    assert!(bad["error"].is_string(), "expected an error: {bad}");
    assert!(bad.get("cancelled").is_none());

    // `not isinstance(raw, (int, str))`: a float is neither — missing_arg, the
    // same refusal as an absent id, never an honest {cancelled:false}.
    for f in [json!(3.5), json!(3.0)] {
        let r = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "cancel", "task_id": f}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["reason"], "missing_arg", "task_id={f}: {r}");
        assert_eq!(r["arg"], "task_id");
    }
}

// ─── the emergent one: an AGENT cancels another agent's work ───

#[tokio::test]
async fn agent_cancels_another_agents_task() {
    // An agent wields `cancel`: the killer agent's own handler discovers the
    // victim's task through `ps` and sends `cancel`. The baseline (`$self`) does
    // not reach the root, so the killer holds a grant that names them there.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let victim = mk_agent(&kernel, "slow_victim").await;
    let killer = mk_agent_grant(
        &kernel,
        "slow_killer",
        json!({"kernel_state": ["ps", "cancel"]}),
    )
    .await;

    let caller = tokio::spawn({
        let k = Arc::clone(&kernel);
        let v = victim.clone();
        async move {
            k.send(vec![Item::new(v, json!({"type": "slow"}))])
                .await
                .remove(0)
                .result
        }
    });
    let victim_task = wait_for_task(&kernel, &victim, "slow").await;

    let r = kernel
        .send(vec![Item::new(
            killer,
            json!({"type": "kill", "victim": victim.0}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["cancelled"], json!(true));
    assert_eq!(r["killed"], json!([victim_task]));

    let live: HashSet<i64> = rows(&kernel)
        .await
        .iter()
        .filter_map(|t| t["id"].as_i64())
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
        async move {
            k.send(vec![Item::new(a, json!({"type": "slow"}))])
                .await
                .remove(0)
                .result
        }
    });
    let task_id = wait_for_task(&kernel, &a, "slow").await;
    wait_until("the resource to be held", || {
        sh.held_ids().contains(&task_id)
    })
    .await;

    let r = cancel(&kernel, task_id).await;
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

#[tokio::test]
async fn on_cancel_failure_prints_python_line() {
    // C87: python `print(f"  [cancel] {self.id} bundle on_cancel raised:
    // {e}")` (`_agent.py:2858-2859`) — STDOUT, and cancel is not refusable:
    // the abort still happens even though the hook errored. (The line
    // itself is reviewed by inspection — see the note on the state-
    // subscriber twin about capturing process stdout under parallel tests.)
    let sh = Arc::new(Shared::default());
    sh.set_fail_on_cancel(true);
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "slow_a").await;

    let caller = tokio::spawn({
        let k = Arc::clone(&kernel);
        let a = a.clone();
        async move {
            k.send(vec![Item::new(a, json!({"type": "slow"}))])
                .await
                .remove(0)
                .result
        }
    });
    let task_id = wait_for_task(&kernel, &a, "slow").await;
    wait_until("the resource to be held", || {
        sh.held_ids().contains(&task_id)
    })
    .await;

    let r = cancel(&kernel, task_id).await;
    assert_eq!(
        r["cancelled"],
        json!(true),
        "a raising hook does not block the abort"
    );
    // The hook DID run (it clears `held` before returning its Err) — the
    // failure is in its RETURN, not in whether it ran.
    assert!(sh.held_ids().is_empty());
    let _ = tokio::time::timeout(Duration::from_secs(2), caller).await;
}

// ─── long-lived work: init-adoption + reachable by cancel ──────

#[tokio::test]
async fn forked_work_outlives_its_dispatch_and_is_adopted_by_its_grandparent() {
    // Work can OUTLIVE the dispatch that spawned it (a job, a pump). The bundle
    // registers it with `kernel.spawn_task`, so `ps` sees it; when the dispatch
    // that forked it dies, the job is RE-PARENTED to that dispatch's parent
    // (init-adoption, exactly like Linux) — which is what makes "cancel the
    // message" also kill the jobs that message spawned.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent_grant(&kernel, "slow_a", json!({"slow_b": "*"})).await;
    let b = mk_agent_accept(&kernel, "slow_b", json!({"slow_a": "*"})).await;

    let caller = tokio::spawn({
        let k = Arc::clone(&kernel);
        let a = a.clone();
        async move {
            k.send(vec![Item::new(
                a,
                json!({"type": "chain_fork", "next": "slow_b"}),
            )])
            .await
            .remove(0)
            .result
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
    let r = cancel(&kernel, a_task).await;
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

// ─── the substrate's own verbs: `docs of=kernel`, never a reflect ──────

#[tokio::test]
async fn kernel_verbs_are_docs_not_reflect() {
    // `ps`/`cancel` and the tree verbs belong to no bundle — they are the
    // SUBSTRATE's own, answered by the root alone, and READ with `docs
    // of=kernel` (owner, 2026-10-05: a reflect carries no verbs).
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "slow_a").await;
    for target in [AgentId::from("kernel"), a.clone()] {
        let r = kernel
            .send(vec![Item::new(target.clone(), json!({"type": "reflect"}))])
            .await
            .remove(0)
            .result;
        assert!(r.get("kernel_verbs").is_none(), "{target}: {r}");
    }
    let one = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "docs", "of": "kernel"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(one["bundle"], json!("kernel"));
    assert_eq!(one["verbs"]["cancel"], json!("args: task_id:str"));
    assert!(
        one["verbs"].get("get").is_none(),
        "`get` is not a kernel verb"
    );
    let detailed = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "docs", "of": "kernel", "verb": "cancel", "tier": "detailed"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(detailed["verbs"].as_object().unwrap().len(), 1);
    assert!(detailed["verbs"]["cancel"]
        .as_str()
        .unwrap()
        .contains("SUBTREE"));
}

#[tokio::test]
async fn shutdown_agents_tells_every_agent_and_then_refuses_new_roots() {
    // THE POINT OF A GRACEFUL STOP. An agent must get the chance to persist
    // what matters before the process goes — so it is TOLD, its tools still
    // work, and only then does the door close. A stop that just killed tasks
    // would lose exactly the state the shutdown exists to save.
    let kernel = mk_kernel(Arc::new(Shared::default()));
    let a = mk_agent(&kernel, "sd_a").await;
    let b = mk_agent(&kernel, "sd_b").await;
    let out = kernel.shutdown_agents().await;
    assert_eq!(out["drained"], true, "{out}");
    let shut = out["shut"].as_array().expect("shut is a list").clone();
    for id in [&a, &b] {
        assert!(
            shut.iter().any(|s| s.as_str() == Some(id.as_str())),
            "every agent that ADVERTISES `shutdown` is told: {id} missing from {out}"
        );
    }
    assert!(kernel.draining(), "the drain flag holds after the verb");

    // A NEW ROOT IS REFUSED — and refused BEFORE it costs an id. An agent
    // send, not an operator one: the operator's control surface is how the stop
    // is watched and finished, so it is never what the drain closes.
    let peer = mk_agent(&kernel, "sd_peer").await;
    let out = crate::test_support::as_agent(
        AgentId::from("kernel_state"),
        kernel.send(vec![Item::new(peer, json!({"type": "chain"}))]),
    )
    .await
    .remove(0);
    assert_eq!(out.status, crate::outcome::Status::Failed, "{out:?}");
    assert_eq!(
        out.result.get("reason").and_then(Value::as_str),
        Some(crate::refusal::DRAINING),
        "a new root while draining is refused, coded: {}",
        out.result
    );
    assert!(out.pid.is_none(), "and refused before it costs an id");
}

#[tokio::test]
async fn the_shutdown_agents_verb_tells_a_grandchild() {
    // Telling is kernel bookkeeping (the handler run directly, ungated), so every
    // advertising agent is told at ANY depth. Through the kernel verb the
    // dispatch runs AS THE ROOT, whose grant reaches no one — gated, a
    // grandchild would be refused and never told.
    let kernel = mk_kernel(Arc::new(Shared::default()));
    let p = mk_agent(&kernel, "sd_parent").await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": p.as_str(),
                   "handler_module": "slow.tools", "id": "sd_grandchild"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "sd_grandchild", "{r}");
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "shutdown_agents"}),
        )])
        .await
        .remove(0)
        .result;
    let shut = out["shut"].as_array().expect("shut is a list");
    assert!(
        shut.iter().any(|s| s == "sd_grandchild"),
        "the grandchild was not told: {out}"
    );
}

#[tokio::test]
async fn a_second_shutdown_agents_answers_the_same_and_shuts_nothing_new() {
    // IDEMPOTENT. An operator watching a stop will call it twice; the second
    // call must not be a second teardown.
    let kernel = mk_kernel(Arc::new(Shared::default()));
    let first = kernel.shutdown_agents().await;
    let second = kernel.shutdown_agents().await;
    assert_eq!(first["drained"], second["drained"]);
    assert_eq!(
        second["cancelled"].as_array().map(Vec::len),
        Some(0),
        "nothing new to cancel the second time: {second}"
    );
}

#[tokio::test]
async fn shutdown_agents_is_the_roots_control_surface_only() {
    // A CHILD MAY NOT STOP THE TREE. The verb is addressed to the kernel root
    // (or the reserved `kernel` address); anything else answers with an error rather than
    // quietly draining the whole process on one agent's say-so.
    let kernel = mk_kernel(Arc::new(Shared::default()));
    let child = AgentId::from("sd_child");
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "slow.tools", "id": child.as_str(),
            }),
        )])
        .await;
    let out = kernel
        .send(vec![Item::new(
            child.clone(),
            json!({"type": "shutdown_agents"}),
        )])
        .await
        .remove(0)
        .result;
    // THE ROUTER REFUSES IT FIRST, and more usefully than the handler's own
    // guard would: a coded `not_here` naming the verb and where to send it.
    // The guard inside the handler stays as the second line, for a target that
    // resolves to a non-root agent.
    assert_eq!(
        out.get("reason").and_then(Value::as_str),
        Some("not_here"),
        "a child must be refused: {out}"
    );
    assert!(!kernel.draining(), "a refused call drains nothing");
}

#[tokio::test]
async fn a_keepalive_loop_is_neither_waited_for_nor_cancelled() {
    // THE PERSISTER MUST OUTLIVE THE STOP. It is the thing that writes what the
    // `shutdown` dispatches just produced; waiting for it would never return, and
    // cancelling it would drop the very rows the shutdown was for.
    let kernel = mk_kernel(Arc::new(Shared::default()));
    let live = kernel.spawn_keepalive_task(
        &AgentId::from("kernel_state"),
        "persist_writer",
        std::future::pending::<()>(),
    );
    assert!(
        kernel.tasks.pending(&[]).iter().all(|t| *t != live),
        "a keepalive task is not pending work"
    );
    let out = kernel.shutdown_agents().await;
    assert!(
        !out["cancelled"]
            .as_array()
            .expect("cancelled is a list")
            .iter()
            .any(|v| v.as_i64() == Some(live.0)),
        "the keepalive loop must survive the stop: {out}"
    );
    assert!(kernel.tasks.contains(&live), "and still be running");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forked_service_loop_is_not_waited_for_but_is_cancelled() {
    // python `Kernel.shutdown_agents`: the quiet wait ignores `_keepalive` AND
    // `_forked`. A forked service loop (a PTY reader, a subprocess pump) never
    // ends, so waiting for one spent the WHOLE `QUIET_TIMEOUT` on every stop
    // and then cancelled it anyway. The cancel step still takes it — unlike a
    // keepalive, a fork does not outlive the stop.
    let kernel = mk_kernel(Arc::new(Shared::default()));
    let live = kernel.spawn_task(
        &AgentId::from("kernel_state"),
        "pty_reader",
        std::future::pending::<()>(),
    );
    assert!(
        kernel
            .tasks
            .pending_dispatches(&[])
            .iter()
            .all(|t| *t != live),
        "a forked loop is not a pending DISPATCH"
    );
    assert!(
        kernel.tasks.pending(&[]).contains(&live),
        "but it IS pending work the cancel step reaches"
    );
    let t0 = std::time::Instant::now();
    let out = kernel.shutdown_agents().await;
    let took = t0.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "the quiet phase must not wait on a forked loop (QUIET_TIMEOUT is 10 s), took {took:?}"
    );
    assert!(
        out["cancelled"]
            .as_array()
            .expect("cancelled is a list")
            .iter()
            .any(|v| v.as_i64() == Some(live.0)),
        "the cancel step still takes it: {out}"
    );
    assert!(!kernel.tasks.contains(&live), "and it is gone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_quiet_phase_still_waits_for_a_real_in_flight_dispatch() {
    // THE OTHER HALF. Ignoring forks must not turn the graceful stop into a
    // kill: a dispatch actually running is waited for, and settles rather than
    // appearing in `cancelled`.
    let kernel = mk_kernel(Arc::new(Shared::default()));
    let a = mk_agent(&kernel, "sd_hold").await;
    let k = Arc::clone(&kernel);
    let running = tokio::spawn(async move {
        k.send(vec![Item::new(a, json!({"type": "slow", "hold_ms": 400}))])
            .await
            .remove(0)
    });
    while kernel.tasks.pending_dispatches(&[]).is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let t0 = std::time::Instant::now();
    let out = kernel.shutdown_agents().await;
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(300),
        "the quiet phase waits for in-flight work, took only {took:?}"
    );
    assert!(
        took < Duration::from_secs(9),
        "and it stops waiting once the work settles, well inside QUIET_TIMEOUT: {took:?}"
    );
    assert_eq!(
        out["cancelled"].as_array().map(Vec::len),
        Some(0),
        "a dispatch that settled is not cancelled: {out}"
    );
    let done = running.await.expect("the dispatch task joined");
    assert_eq!(done.status, crate::outcome::Status::Completed, "{done:?}");
}

// ── C36: a fork is a message ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_task_writes_a_task_row() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "forker").await;
    let r = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "fork"}))])
        .await
        .remove(0);
    let forked = r.result["task_id"].as_i64().expect("a forked task id");
    // THE TASK ID IS THE ROW ID: the fork took its number from the store by
    // writing the row, not from the counter behind the store's back.
    let row = kernel.rows.get_one(forked).expect("the task row");
    assert_eq!(row["kind"], json!("task"));
    assert_eq!(row["verb"], json!("job"));
    assert_eq!(row["agent"], json!("forker"));
    assert_eq!(
        row["parent"].as_i64(),
        r.pid.map(TaskId::get),
        "parent = the dispatch that forked it"
    );
    assert_eq!(
        row.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["kind", "verb", "parent", "id", "agent", "ts"],
        "python's key order: the row as written, then what the store adds"
    );
    // …and it sits on the forking agent's list, beside its `call` rows.
    let mine = crate::test_support::rows_about(&kernel, &a);
    let kinds: Vec<&str> = mine
        .iter()
        .filter_map(|r| r.get("kind").and_then(Value::as_str))
        .collect();
    // the `fork` call, then the fork — the child's `boot` is kernel bookkeeping
    // (run directly, no call row).
    assert_eq!(kinds, vec!["call", "task"]);
}

/// D5 — a task whose parent has already ended keeps NAMING it.
///
/// `register` used to drop such a parent to `None` ("adopted by init"). Python
/// removed exactly that line (`_kernel.py:602-616`): a task put outside every
/// subtree cannot be reached by `cancel` on the caller that forked it.
/// Measured: rust's `kernel ps` answered `parent: null` for the scheduler tick
/// loop, the pty readers and `ps` itself, where python names the dispatch that
/// spawned them.
#[tokio::test]
async fn a_task_keeps_a_parent_that_has_already_ended() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let agent = mk_agent_grant(&kernel, "forker", json!({})).await;
    let handle = crate::tasks::TaskHandle::new();

    let dead = crate::tasks::TaskId(kernel.mint_id());
    // The forking dispatch ended before the work it forked registered itself —
    // the shape a bundle's long-lived loop has (the scheduler tick, a pty
    // reader): the parent id is real, the parent task is gone.
    assert!(!kernel.tasks.contains(&dead), "never registered");

    let forked = crate::tasks::TaskId(kernel.mint_id());
    kernel
        .tasks
        .register(&forked, &agent, "tick_loop", Some(dead), handle);

    let row = kernel
        .tasks
        .rows()
        .into_iter()
        .find(|r| r["id"] == json!(forked.get()))
        .expect("the forked task is in ps");
    assert_eq!(
        row["parent"],
        json!(dead.get()),
        "the id survives the parent — `null` would put it outside every subtree"
    );
    // And it is therefore NOT a root, so the shutdown sweep leaves it to the
    // subtree it belongs to (python's `roots only` skip).
    assert!(!kernel.tasks.is_root(&forked));
}

/// PER-INSTANCE VERBS (owner, 2026-10-08) — python
/// `test_docs_of_an_agent_read_its_own_table_when_its_bundle_names_one`: a bundle
/// whose agents serve different verbs names each one's table
/// (`instance_verb_docs`); `None` keeps the bundle's.
#[tokio::test]
async fn docs_of_an_agent_read_its_own_table_when_its_bundle_names_one() {
    struct PerInstance;
    #[async_trait]
    impl Bundle for PerInstance {
        fn name(&self) -> &str {
            "per_instance_probe"
        }
        fn verb_docs(&self) -> Value {
            json!({"ping": "No args. The bundle's own verb."})
        }
        fn instance_verb_docs(&self, agent: &Agent) -> Option<Value> {
            (agent.id.as_str() != "plain").then(|| {
                let mut m = Map::new();
                m.insert("ping".into(), json!("No args. Still here."));
                m.insert(
                    format!("own_{}", agent.id.as_str()),
                    json!("args: x:int. Only mine."),
                );
                Value::Object(m)
            })
        }
        async fn handle(
            &self,
            _: &AgentId,
            _: &Value,
            _: &Arc<Kernel>,
        ) -> Result<Reply, BundleError> {
            Ok(None)
        }
    }
    let mut kernel = Kernel::new();
    kernel.bundles.register("per_instance_probe", PerInstance);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        PathBuf::new(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    for id in ["a1", "a2", "plain"] {
        kernel.register(Agent::new(
            AgentId::from(id),
            Some("per_instance_probe".into()),
            Some(AgentId::from("kernel_state")),
            Map::new(),
            PathBuf::new(),
            false,
        ));
    }
    let docs = |of: &str, extra: Value| {
        let mut p = json!({"type": "docs", "of": of});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            p[k] = v;
        }
        let k = Arc::clone(&kernel);
        async move {
            k.send(vec![Item::new(AgentId::from("kernel"), p)])
                .await
                .remove(0)
                .result
        }
    };
    let a1 = docs("a1", json!({})).await;
    assert_eq!(
        a1["verbs"],
        json!({"ping": "No args", "own_a1": "args: x:int"}),
        "{a1}"
    );
    let a2 = docs("a2", json!({"tier": "detailed", "verb": "own_a2"})).await;
    assert_eq!(
        a2["verbs"],
        json!({"own_a2": "args: x:int. Only mine."}),
        "{a2}"
    );
    assert_eq!(
        docs("plain", json!({})).await["verbs"],
        json!({"ping": "No args"})
    );
    let gone = docs("a1", json!({"verb": "own_a2"})).await;
    assert_eq!(gone["reason"], "bad_arg", "{gone}");
    assert!(gone["hint"].as_str().unwrap().contains("own_a1"), "{gone}");
}
