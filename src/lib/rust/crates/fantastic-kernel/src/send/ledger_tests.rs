//! #34–#37 — the execution ledger, as a CALLER sees it through `send`.
//!
//! Ports the assertions of python `tests/kernel/test_execution_ledger.py`.
//! In-process invariants: a named id runs once and only once, a concurrent
//! re-send JOINS rather than re-running, a retry budget re-dispatches under the
//! same handle, and a spent budget parks the row as a dead letter that is never
//! silently re-armed.

use crate::agent::AgentId;
use crate::outcome::{Item, Status};
use crate::send::with_sender;
use crate::tasks::tests::{mk_agent, mk_agent_grant, mk_kernel, Shared};
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
    let pid = out.pid.expect("a dispatched item has a pid");

    let rows = ledger_rows(&kernel).await;
    let row = rows
        .iter()
        .find(|r| r["id"] == serde_json::json!(pid.0))
        .expect("the unnamed execution has a row");
    assert_eq!(row["named"], false);
    assert_eq!(row["status"], "completed");

    // AND IT CANNOT BE RE-FETCHED, because nobody named it. A pid is the
    // kernel's; a caller joins an execution through the NAME it gave, and an
    // unnamed one has none. That is the trade: re-fetchability is what a name
    // buys, and this row is here to be READ (`ledger`), not re-entered.
    assert_eq!(row["name"], serde_json::Value::Null);
    assert_eq!(sh.run_count(), 1);
}

#[tokio::test]
async fn ledger_row_key_order_matches_python() {
    // C16: python's row is exactly `{id, target, attempts, retries, named,
    // name?, status, outcome?}` — `name` OMITTED (not `null`) when the
    // execution is unnamed. Measured: rust used to carry `name:null` on
    // every row.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_key_order").await;

    let _ = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "count"}))])
        .await
        .remove(0);
    // `mk_agent` itself issues a `create_agent` (+ its `boot` side effect),
    // each its own ledger row — find OUR unnamed "count" dispatch by target
    // + status among them, not by position.
    let unnamed = ledger_rows(&kernel)
        .await
        .into_iter()
        .find(|r| r["target"] == json!("l_key_order") && r["status"] == "completed")
        .expect("the unnamed count dispatch's row");
    assert_eq!(
        unnamed.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["id", "target", "attempts", "retries", "named", "status", "outcome"],
        "{unnamed}"
    );
    assert!(
        !unnamed.as_object().unwrap().contains_key("name"),
        "unnamed rows must OMIT `name`, not carry it as null: {unnamed}"
    );

    let _ = kernel
        .send(vec![
            Item::new(a.clone(), json!({"type": "count"})).with_id("named-1")
        ])
        .await
        .remove(0);
    let named = ledger_rows(&kernel)
        .await
        .into_iter()
        .find(|r| r["name"] == json!("named-1"))
        .expect("the named row");
    assert_eq!(
        named.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["id", "target", "attempts", "retries", "named", "name", "status", "outcome"],
        "{named}"
    );
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
    let dead = rows
        .iter()
        .find(|r| r["name"] == "dead-1")
        .expect("the row");
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
    // C31: python's prose, verbatim — the JOINED execution's id (bare
    // digits, not this joiner's own pid-less dispatch), plus `task_id`.
    let exec_id = joined.result["task_id"]
        .as_i64()
        .expect("a joiner timeout must carry task_id");
    assert_eq!(
        joined.result["error"],
        json!(format!(
            "send: joined execution {exec_id} did not settle within this \
             caller's 0.2s wait; the execution itself keeps running"
        ))
    );

    // The OWNER is still running — the joiner's timeout did not reach it.
    let rows = ledger_rows(&kernel).await;
    let row = rows
        .iter()
        .find(|r| r["name"] == "long-1")
        .expect("the row");
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

    // The operator sees it. The row is found by the NAME it carries — the id is
    // the kernel's and the caller never learns it except through the outcome.
    let rows = ledger_rows(&kernel).await;
    let mine = rows
        .iter()
        .find(|r| r["name"] == "operator-job")
        .expect("the operator's own row");
    let mine = mine["id"].as_i64().unwrap();

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
    let agent_ids: Vec<i64> = seen["executions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert!(
        !agent_ids.contains(&mine),
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
            victim = row["id"].as_i64();
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

/// Joining is authorized against the verb THAT EXECUTION RAN.
///
/// The gate used to be asked the wrong question on a join — the joiner's own
/// payload verb — and the JOIN then handed back the ORIGINAL call's outcome, a
/// reply for a verb the joiner never had authorized. Measured in python
/// 2026-08-28: a caller granted only a harmless verb reused another's id and
/// received its secret verbatim. Dedup was correct; the RESULT crossed an
/// authorization boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joining_is_gated_on_the_verb_that_execution_ran() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    for (who, grant) in [("alice", "work"), ("bob", "ping")] {
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({
                    "type": "create_agent", "parent_id": "kernel",
                    "handler_module": "slow.tools", "id": who,
                    "can_call": {"vault": [grant]},
                }),
            )])
            .await;
    }
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({
                "type": "create_agent", "parent_id": "kernel",
                "handler_module": "slow.tools", "id": "vault",
                "callable_by": {"alice": ["work"], "bob": ["ping"]},
            }),
        )])
        .await;

    let k = Arc::clone(&kernel);
    let a = with_sender(AgentId::from("alice"), async move {
        k.send(vec![Item {
            target: AgentId::from("vault"),
            payload: json!({"type": "work"}),
            id: Some("job1".into()),
            timeout: None,
            retries: None,
            binary: None,
            raw_id: None,
            raw_timeout: None,
            raw_retries: None,
        }])
        .await
        .remove(0)
    })
    .await;
    // `errored`: the vault HANDLER ran and answered "unknown verb". A semantic
    // no, not a transport failure — and a settled outcome either way, which is
    // what makes the join below the thing under test.
    assert_eq!(a.status, Status::Errored, "{a:?}");

    let k = Arc::clone(&kernel);
    let b = with_sender(AgentId::from("bob"), async move {
        k.send(vec![Item {
            target: AgentId::from("vault"),
            payload: json!({"type": "ping"}),
            id: Some("job1".into()),
            timeout: None,
            retries: None,
            binary: None,
            raw_id: None,
            raw_timeout: None,
            raw_retries: None,
        }])
        .await
        .remove(0)
    })
    .await;
    assert_eq!(
        b.result["reason"], "unauthorized",
        "bob joined an execution it may not run: {b:?}"
    );

    // An AUTHORIZED re-sender still dedups — the fix must not cost ensure-semantics.
    let k = Arc::clone(&kernel);
    let again = with_sender(AgentId::from("alice"), async move {
        k.send(vec![Item {
            target: AgentId::from("vault"),
            payload: json!({"type": "work"}),
            id: Some("job1".into()),
            timeout: None,
            retries: None,
            binary: None,
            raw_id: None,
            raw_timeout: None,
            raw_retries: None,
        }])
        .await
        .remove(0)
    })
    .await;
    assert_eq!(
        again.result, a.result,
        "the recorded outcome stopped returning"
    );
}

/// AN ERRORED EXECUTION IS STILL AN EXECUTION. The `ledger` verb lists what
/// named work BECAME, and a handler that RAN and said no is a row like any
/// other — `e2e/coding/peer_review` proves a second mind was consulted by
/// finding the `ai -> ai_rev` row, whatever the reviewer answered.
#[tokio::test]
async fn an_errored_execution_is_visible_in_the_ledger() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "l_err").await;
    let out = kernel
        .send(vec![Item::new(a.clone(), json!({"type": "nope_unknown"}))])
        .await
        .remove(0);
    assert_eq!(out.status, Status::Errored, "{out:?}");

    // BY PID — the agent also has a child-boot row under the same target.
    let pid = out.pid.expect("a dispatched send has a pid").0;
    let rows = ledger_rows(&kernel).await;
    let hit = rows
        .iter()
        .find(|r| r["id"] == pid)
        .unwrap_or_else(|| panic!("the errored execution vanished: {rows:?}"));
    assert_eq!(hit["target"], a.0, "{hit:?}");
    assert_eq!(hit["status"], "errored", "{hit:?}");
}

/// A SEND MADE FROM A FORK IS STILL A LEDGER ROW. `ai_core`'s reply routing runs
/// in a `spawn_task`, so every tool call a model makes is issued from a fork —
/// which is exactly what `e2e/coding/peer_review` looks for when it proves a
/// second mind was consulted (`target == "ai_rev"`).
#[tokio::test]
async fn a_send_issued_from_a_spawned_fork_is_in_the_operators_ledger() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let b = mk_agent(&kernel, "l_peer").await;
    // GRANT THE EDGE: a refused send never reaches the ledger at all.
    let a = mk_agent_grant(&kernel, "l_forker", json!({"l_peer": "*"})).await;
    // TWO-SIDED DENY-ALL: the callee must ACCEPT the caller, or the send is
    // refused at ingress and never reaches the ledger at all.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": "l_peer", "callable_by": {"l_forker": "*"}}),
        )])
        .await;

    // The fork acts as `a`, the way route_reply acts as its agent.
    let k = Arc::clone(&kernel);
    let peer = b.clone();
    let tid = with_sender(a.clone(), async {
        kernel.spawn_task(&a, "route_reply", async move {
            let _ = k.send(vec![Item::new(peer, json!({"type": "fast"}))]).await;
        })
    })
    .await;
    // Let the fork run.
    for _ in 0..100 {
        if !kernel.tasks.contains(&tid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let rows = ledger_rows(&kernel).await;
    assert!(
        rows.iter()
            .any(|r| r["target"] == b.0 && r["outcome"]["result"]["fast"] == true),
        "a forked agent's send left no ledger row: {rows:?}"
    );
}

/// READ AS AN AGENT, SEE ONLY YOUR OWN. The view is origin-scoped, so a reader
/// that is not the operator cannot see another agent's executions — which is
/// what makes `ledger` safe to grant. Pinned because `e2e/coding/peer_review`
/// reads it over a REST leg, and what that leg counts as the ORIGIN decides
/// whether the probe sees the model's work at all.
#[tokio::test]
async fn the_ledger_view_is_origin_scoped() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let b = mk_agent(&kernel, "l_theirs").await;
    let a = mk_agent_grant(&kernel, "l_mine", json!({"l_theirs": "*"})).await;
    // TWO-SIDED DENY-ALL: the callee must ACCEPT the caller, or the send is
    // refused at ingress and never reaches the ledger at all.
    let _ = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "update_agent", "id": "l_theirs", "callable_by": {"l_mine": "*"}}),
        )])
        .await;

    let k = Arc::clone(&kernel);
    let peer = b.clone();
    with_sender(a.clone(), async move {
        let _ = k.send(vec![Item::new(peer, json!({"type": "fast"}))]).await;
    })
    .await;

    // The OPERATOR sees it.
    let mine = |rows: Vec<serde_json::Value>| {
        rows.iter()
            .any(|r| r["target"] == b.0 && r["outcome"]["result"]["fast"] == true)
    };
    assert!(
        mine(kernel.ledger.rows_for(None)),
        "the operator must see every execution: {:?}",
        kernel.ledger.rows_for(None)
    );
    // A THIRD PARTY does not.
    let other = AgentId::from("l_stranger");
    assert!(
        !mine(kernel.ledger.rows_for(Some(&other))),
        "an agent saw an execution it did not originate"
    );
    // The ORIGINATOR does.
    assert!(
        mine(kernel.ledger.rows_for(Some(&a))),
        "the originator lost its own row"
    );
}

/// C22: the ledger row stores the execution's ORIGINAL PAYLOAD, and the join
/// denial runs the gate with it — not with a synthesised `{"type": verb}`.
///
/// `create_agent` is the case that proves it: its gate reads `parent_id` for the
/// PLACEMENT check and the grant maps for CONFERRAL. Asked about a payload with
/// neither, the gate answers a different question than the call did — and the
/// joiner would receive a record for a subtree it may not plant in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_joiner_is_gated_with_the_executions_original_payload() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    // `alice` may create under the root; `bob` may only create under itself
    // (the structural baseline every agent has), which is exactly what the
    // placement check reads out of the ORIGINAL `parent_id`.
    for (who, grant) in [
        ("alice", json!({"kernel_state": ["create_agent"]})),
        ("bob", json!({})),
    ] {
        let _ = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({
                    "type": "create_agent", "parent_id": "kernel",
                    "handler_module": "slow.tools", "id": who,
                    "can_call": grant,
                }),
            )])
            .await;
    }
    let payload = json!({
        "type": "create_agent", "parent_id": "kernel_state",
        "handler_module": "slow.tools", "id": "planted",
    });
    let k = Arc::clone(&kernel);
    let p = payload.clone();
    let made = with_sender(AgentId::from("alice"), async move {
        k.send(vec![Item {
            target: AgentId::from("kernel_state"),
            payload: p,
            id: Some("plant-1".into()),
            timeout: None,
            retries: None,
            binary: None,
            raw_id: None,
            raw_timeout: None,
            raw_retries: None,
        }])
        .await
        .remove(0)
    })
    .await;
    assert_eq!(made.status, Status::Completed, "{made:?}");

    // bob joins the SAME id. The gate is asked about the create it would be
    // handed — placement on `kernel_state` — and refuses.
    let k = Arc::clone(&kernel);
    let joined = with_sender(AgentId::from("bob"), async move {
        k.send(vec![Item {
            target: AgentId::from("kernel_state"),
            payload: json!({"type": "create_agent", "parent_id": "bob",
                            "handler_module": "slow.tools", "id": "mine"}),
            id: Some("plant-1".into()),
            timeout: None,
            retries: None,
            binary: None,
            raw_id: None,
            raw_timeout: None,
            raw_retries: None,
        }])
        .await
        .remove(0)
    })
    .await;
    assert_eq!(joined.status, Status::Failed, "{joined:?}");
    assert_eq!(joined.result["reason"], json!("unauthorized"), "{joined:?}");
    assert!(
        joined.result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("kernel_state"),
        "the denial must name the ORIGINAL placement: {joined:?}"
    );
}
