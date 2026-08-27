//! #34–#37 — the execution ledger, as a CALLER sees it through `send`.
//!
//! Ports the assertions of python `tests/kernel/test_execution_ledger.py`.
//! In-process invariants: a named id runs once and only once, a concurrent
//! re-send JOINS rather than re-running, a retry budget re-dispatches under the
//! same handle, and a spent budget parks the row as a dead letter that is never
//! silently re-armed.

use crate::agent::AgentId;
use crate::outcome::{Item, Status};
use crate::tasks::tests::{mk_agent, mk_kernel, Shared};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

type K = Arc<crate::kernel::Kernel>;

async fn ledger_rows(kernel: &K) -> Vec<serde_json::Value> {
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ledger"}),
        )])
        .await
        .remove(0)
        .result["executions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[tokio::test]
async fn a_named_id_runs_the_handler_exactly_once() {
    // C9/C11: the SECOND send of a settled id returns the recorded outcome and
    // the handler does not run. This is the whole dedup contract.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_once").await;

    let first = kernel
        .send(vec![
            Item::new(a.clone(), json!({"type": "count"})).with_id("job-1")
        ])
        .await
        .remove(0);
    assert_eq!(first.status, Status::Completed);
    assert_eq!(first.result["run"], 1);
    assert_eq!(sh.run_count(), 1);

    for _ in 0..4 {
        let again = kernel
            .send(vec![
                Item::new(a.clone(), json!({"type": "count"})).with_id("job-1")
            ])
            .await
            .remove(0);
        assert_eq!(again.result["run"], 1, "the recorded outcome must return");
        assert_eq!(again.attempts, 1);
    }
    assert_eq!(sh.run_count(), 1, "the handler ran more than once");
}

#[tokio::test]
async fn concurrent_sends_of_one_id_join_and_run_the_body_once() {
    // C10: once-only under concurrency. Both callers get the SAME outcome and
    // exactly one dispatch happens — this is what makes a double-click safe.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_conc").await;

    let (l, r) = tokio::join!(
        kernel.send(vec![
            Item::new(a.clone(), json!({"type": "count"})).with_id("click-7")
        ]),
        kernel.send(vec![
            Item::new(a.clone(), json!({"type": "count"})).with_id("click-7")
        ]),
    );
    assert_eq!(sh.run_count(), 1, "a race ran the body twice");
    assert_eq!(l[0].result, r[0].result, "both callers see one outcome");
}

#[tokio::test]
async fn an_unnamed_execution_is_ledgered_under_its_pid() {
    // ONE ID SPACE: every execution is a row, so any reply is re-fetchable by
    // the pid `ps`/`cancel` already use.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_unnamed").await;

    let out = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "count"}))])
        .await
        .remove(0);
    let pid = out.pid.clone().expect("a dispatched item has a pid");

    let rows = ledger_rows(&kernel).await;
    let row = rows
        .iter()
        .find(|r| r["id"] == pid.0)
        .expect("the unnamed execution has a row");
    assert_eq!(row["named"], false);
    assert_eq!(row["status"], "completed");

    // Re-fetch BY PID returns the recorded outcome; it never re-runs.
    let refetched = kernel
        .send(vec![
            Item::new(a.clone(), json!({"type": "count"})).with_id(pid.0.clone())
        ])
        .await
        .remove(0);
    assert_eq!(refetched.result["run"], 1);
    assert_eq!(sh.run_count(), 1);
}

#[tokio::test]
async fn a_retry_budget_re_dispatches_under_the_same_id() {
    // The engine retries a terminal failure while budget lasts, and only the
    // FINAL outcome settles — a joiner never sees an attempt that was retried
    // away. `attempts` reports what the work actually cost.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_retry").await;

    let out = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "flaky", "succeed_on": 3}),
        )
        .with_id("flaky-1")
        .with_retries(5)])
        .await
        .remove(0);

    assert_eq!(out.status, Status::Completed, "{:?}", out.result);
    assert_eq!(out.result["made_it"], 3);
    assert_eq!(out.attempts, 3, "attempts must report the real cost");
    assert_eq!(sh.run_count(), 3);
}

#[tokio::test]
async fn a_spent_budget_parks_a_dead_letter_that_never_re_arms() {
    // C12: failed work must neither vanish nor loop. It parks WITH its outcome,
    // queryable; a retry is an explicit NEW id, never a silent re-run.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_dead").await;

    let out = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "flaky", "succeed_on": 99}),
        )
        .with_id("dead-1")
        .with_retries(1)])
        .await
        .remove(0);
    assert_eq!(out.status, Status::Failed);
    assert_eq!(out.attempts, 2, "one attempt plus one retry");
    assert_eq!(sh.run_count(), 2);

    // It is INSPECTABLE — terminal non-completed rows are the dead-letter queue.
    let rows = ledger_rows(&kernel).await;
    let dead = rows.iter().find(|r| r["id"] == "dead-1").expect("the row");
    assert_eq!(dead["status"], "failed");
    assert_eq!(dead["attempts"], 2);

    // Re-sending the same id — even with a bigger budget — returns the recorded
    // failure. Nothing is retried silently.
    let again = kernel
        .send(vec![Item::new(
            a.clone(),
            json!({"type": "flaky", "succeed_on": 1}),
        )
        .with_id("dead-1")
        .with_retries(9)])
        .await
        .remove(0);
    assert_eq!(again.status, Status::Failed);
    assert_eq!(sh.run_count(), 2, "a parked dead letter re-armed");
}

#[tokio::test]
async fn a_joiner_timeout_bounds_only_its_own_wait() {
    // A joiner does not OWN the execution: its deadline expiring must not kill
    // the owner's work, only end the joiner's wait.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_join").await;

    let k = Arc::clone(&kernel);
    let peer = a.clone();
    let owner = tokio::spawn(async move {
        k.send(vec![
            Item::new(peer, json!({"type": "slow"})).with_id("long-1")
        ])
        .await
    });
    tokio::time::sleep(Duration::from_millis(120)).await;

    let joined = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "slow"}))
            .with_id("long-1")
            .with_timeout(0.2)])
        .await
        .remove(0);
    assert_eq!(joined.status, Status::TimedOut);

    // The OWNER is still running — the joiner's timeout did not reach it.
    let rows = ledger_rows(&kernel).await;
    let row = rows.iter().find(|r| r["id"] == "long-1").expect("the row");
    assert_eq!(row["status"], "in_flight", "the joiner killed the owner");
    owner.abort();
}

#[tokio::test]
async fn the_ledger_verb_is_scoped_to_what_you_originated() {
    // Unscoped this is a read side-channel around `callable_by`: an agent could
    // read the result BODIES of agents it holds no edge to.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_scope").await;

    // An OPERATOR execution (no origin).
    let _ = kernel
        .send(vec![
            Item::new(a.clone(), json!({"type": "count"})).with_id("operator-job")
        ])
        .await;

    // The operator sees it.
    let ids: Vec<String> = ledger_rows(&kernel)
        .await
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&"operator-job".to_string()));

    // An AGENT reading the ledger sees only its own origins — not this one.
    let seen = crate::send::with_sender(AgentId::from("l_scope"), async {
        kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "ledger"}),
            )])
            .await
            .remove(0)
            .result
    })
    .await;
    let agent_ids: Vec<String> = seen["executions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !agent_ids.contains(&"operator-job".to_string()),
        "an agent read another origin's rows: {agent_ids:?}"
    );
}

#[tokio::test]
async fn the_ledger_verb_is_paged_like_every_other_listing() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_page").await;
    for i in 0..4 {
        let _ = kernel
            .send(vec![
                Item::new(a.clone(), json!({"type": "count"})).with_id(format!("p{i}"))
            ])
            .await;
    }
    let page = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ledger", "limit": 2}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(page["executions"].as_array().unwrap().len(), 2);
    assert!(page["next"].is_string(), "a further page must be named");
}

#[tokio::test]
async fn a_cancelled_execution_stays_cancelled() {
    // A cancel is an absorbing HUMAN decision, not a transient: the retry
    // engine must never re-arm work an operator killed.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_cx").await;

    let k = Arc::clone(&kernel);
    let peer = a.clone();
    let batch = tokio::spawn(async move {
        k.send(vec![Item::new(peer, json!({"type": "slow"}))
            .with_id("kill-1")
            .with_retries(5)])
            .await
    });

    let mut victim = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let ps = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "ps"}),
            )])
            .await
            .remove(0)
            .result;
        if let Some(row) = ps["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["verb"] == "slow")
        {
            victim = row["id"].as_str().map(String::from);
            break;
        }
    }
    let victim = victim.expect("the slow row never appeared");
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "cancel", "task_id": victim}),
        )])
        .await;

    let out = batch.await.expect("send returns").remove(0);
    assert_eq!(out.status, Status::Cancelled, "a cancel was retried away");
    assert_eq!(out.attempts, 1, "cancel must not spend the budget");
}
