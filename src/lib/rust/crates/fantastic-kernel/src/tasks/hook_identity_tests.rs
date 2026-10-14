//! The task table's hook identity and service tasks — the twin of
//! python `tests/kernel/test_hook_identity_service_tasks_quiet_flush.py`.
//!
//! - Every `on_cancel` hook — the `cancel` verb's path and an item's own
//!   deadline alike — and every `on_delete` hook runs AS THE AGENT ITSELF (its own
//!   id is the sender), never as the operator or the root.
//! - `cancel` refuses the kernel's own service tasks (keepalive loops the
//!   root owns); `ps` still lists them, and a group kill leaves them out.
use super::*;
use crate::agent::Agent;
use crate::bundle::{Bundle, BundleError, Reply};
use crate::outcome::Item;
use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Duration;

type Log = Arc<Mutex<Vec<(String, String, Option<String>)>>>;

struct IdBundle {
    log: Log,
}

#[async_trait]
impl Bundle for IdBundle {
    fn name(&self) -> &str {
        "hook_identity_probe"
    }

    async fn handle(
        &self,
        _agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "boot" | "shutdown" => Ok(None),
            "slow" => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(Some(json!({"finished": true})))
            }
            // a root-owned keepalive loop forked INSIDE this dispatch: it is in
            // this task's process group by parentage
            "fork_service" => {
                let root = kernel.root().expect("root").id.clone();
                kernel.spawn_keepalive_task(&root, "svc_probe", std::future::pending::<()>());
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(Some(json!({"finished": true})))
            }
            other => Ok(Some(json!({"error": format!("unknown verb {other:?}")}))),
        }
    }

    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        _task_id: &TaskId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let sender = crate::send::current_sender().map(|s| s.0);
        self.log
            .lock()
            .expect("log poisoned")
            .push(("cancel".into(), agent_id.0.clone(), sender));
        // an emit on its OWN inbox — ai_core's final `status done` to a browser
        kernel
            .emit(
                agent_id,
                json!({"type": "status", "phase": "done", "reason": "cancelled"}),
            )
            .await;
        Ok(())
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let sender = crate::send::current_sender().map(|s| s.0);
        self.log
            .lock()
            .expect("log poisoned")
            .push(("delete".into(), agent_id.0.clone(), sender));
        Ok(())
    }
}

fn mk(log: &Log) -> Arc<Kernel> {
    let mut kernel = Kernel::new();
    kernel.bundles.register(
        "hook_identity_probe.tools",
        IdBundle {
            log: Arc::clone(log),
        },
    );
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

async fn call(kernel: &Arc<Kernel>, target: &str, payload: Value) -> crate::outcome::Outcome {
    kernel
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
}

async fn create(kernel: &Arc<Kernel>, id: &str, parent: &str) {
    let r = call(
        kernel,
        "kernel",
        json!({"type": "create_agent", "parent_id": parent, "id": id,
               "handler_module": "hook_identity_probe.tools"}),
    )
    .await
    .result;
    assert_eq!(r["id"], json!(id), "create_agent failed: {r}");
}

/// `gc` under `mid` under the root — NOT a direct child, so the root's baseline
/// `$subtree` does not reach it.
async fn grandchild(kernel: &Arc<Kernel>) -> AgentId {
    create(kernel, "mid", "kernel_state").await;
    create(kernel, "gc", "mid").await;
    AgentId::from("gc")
}

async fn task_of(kernel: &Arc<Kernel>, agent: &str, verb: &str) -> i64 {
    for _ in 0..300 {
        if let Some(r) = kernel
            .tasks
            .rows()
            .into_iter()
            .find(|t| t["agent"] == json!(agent) && t["verb"] == json!(verb))
        {
            return r["id"].as_i64().expect("task id");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {agent}'s {verb} task");
}

fn drained(rx: &mut crate::inbox::InboxReceiver) -> Vec<Value> {
    std::iter::from_fn(|| rx.try_recv())
        .map(|i| i.payload)
        .collect()
}

fn done_status() -> Value {
    json!({"type": "status", "phase": "done", "reason": "cancelled"})
}

#[tokio::test]
async fn on_cancel_runs_as_the_agent_on_the_cancel_verb_path() {
    let log: Log = Arc::default();
    let kernel = mk(&log);
    let gc = grandchild(&kernel).await;
    let mut inbox = kernel.ensure_inbox(&gc);
    let k = Arc::clone(&kernel);
    let pending = tokio::spawn(async move { call(&k, "gc", json!({"type": "slow"})).await });
    let pid = task_of(&kernel, "gc", "slow").await;
    let out = call(&kernel, "kernel", json!({"type": "cancel", "task_id": pid})).await;
    assert_eq!(out.result, json!({"cancelled": true, "killed": [pid]}));
    assert_eq!(pending.await.unwrap().status.as_str(), "cancelled");
    assert_eq!(
        *log.lock().unwrap(),
        vec![(
            "cancel".to_string(),
            "gc".to_string(),
            Some("gc".to_string())
        )]
    );
    // the own-inbox emit is delivered — never gated root → gc and dropped
    assert!(drained(&mut inbox).contains(&done_status()));
}

#[tokio::test]
async fn on_cancel_runs_as_the_agent_on_the_deadline_path() {
    let log: Log = Arc::default();
    let kernel = mk(&log);
    let gc = grandchild(&kernel).await;
    let mut inbox = kernel.ensure_inbox(&gc);
    let out = kernel
        .send(vec![
            Item::new(gc.clone(), json!({"type": "slow"})).with_timeout(0.2)
        ])
        .await
        .remove(0);
    assert_eq!(out.status.as_str(), "timed_out", "{:?}", out.result);
    assert_eq!(
        *log.lock().unwrap(),
        vec![(
            "cancel".to_string(),
            "gc".to_string(),
            Some("gc".to_string())
        )]
    );
    assert!(drained(&mut inbox).contains(&done_status()));
}

#[tokio::test]
async fn on_delete_runs_as_the_agent_itself() {
    let log: Log = Arc::default();
    let kernel = mk(&log);
    grandchild(&kernel).await;
    let out = call(
        &kernel,
        "kernel",
        json!({"type": "delete_agent", "id": "mid"}),
    )
    .await;
    assert_eq!(out.result["deleted"], json!(true), "{:?}", out.result);
    // depth-first, each hook as its own agent — not the root that ran the verb
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            (
                "delete".to_string(),
                "gc".to_string(),
                Some("gc".to_string())
            ),
            (
                "delete".to_string(),
                "mid".to_string(),
                Some("mid".to_string())
            ),
        ]
    );
}

#[tokio::test]
async fn cancel_refuses_a_root_owned_keepalive_loop_and_ps_still_lists_it() {
    let kernel = mk(&Log::default());
    let svc = kernel.spawn_keepalive_task(
        &AgentId::from("kernel_state"),
        "flush_executions",
        std::future::pending::<()>(),
    );
    let out = call(
        &kernel,
        "kernel",
        json!({"type": "cancel", "task_id": svc.0}),
    )
    .await;
    assert_eq!(out.status.as_str(), "errored");
    assert_eq!(
        out.result,
        json!({
            "error": format!("cancel: task {} (flush_executions) is a kernel service task — not cancellable", svc.0),
            "reason": "bad_arg",
            "hint": "it runs for the life of the kernel and stays listed in ps; kernel_stop ends it",
            "arg": "task_id",
        })
    );
    assert!(kernel.tasks.contains(&svc), "still running");
    let ps = call(&kernel, "kernel", json!({"type": "ps"})).await.result;
    assert!(ps["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["id"] == json!(svc.0)));
    // a keepalive loop ANOTHER agent owns is ordinary work: cancellable
    create(&kernel, "own", "kernel_state").await;
    let theirs = kernel.spawn_keepalive_task(
        &AgentId::from("own"),
        "ws_drain",
        std::future::pending::<()>(),
    );
    let out = call(
        &kernel,
        "kernel",
        json!({"type": "cancel", "task_id": theirs.0}),
    )
    .await;
    assert_eq!(out.result, json!({"cancelled": true, "killed": [theirs.0]}));
}

#[tokio::test]
async fn a_group_kill_leaves_a_service_task_alive() {
    let kernel = mk(&Log::default());
    create(&kernel, "forker", "kernel_state").await;
    let k = Arc::clone(&kernel);
    let pending =
        tokio::spawn(async move { call(&k, "forker", json!({"type": "fork_service"})).await });
    let pid = task_of(&kernel, "forker", "fork_service").await;
    let svc = task_of(&kernel, "kernel_state", "svc_probe").await;
    let row = kernel
        .tasks
        .rows()
        .into_iter()
        .find(|t| t["id"] == json!(svc))
        .unwrap();
    assert_eq!(
        row["parent"],
        json!(pid),
        "the service loop is in the group"
    );
    let out = call(&kernel, "kernel", json!({"type": "cancel", "task_id": pid})).await;
    assert_eq!(out.result, json!({"cancelled": true, "killed": [pid]}));
    assert_eq!(pending.await.unwrap().status.as_str(), "cancelled");
    assert!(kernel.tasks.contains(&TaskId(svc)), "left alive");
}
