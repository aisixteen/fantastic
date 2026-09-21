//! #29 — the ARRAY-ONLY batch send.
//!
//! Ports the assertions of python `tests/kernel/test_batch_send.py`. These are
//! IN-PROCESS invariants: input order, real concurrency, the timeout killing
//! through the front door so `on_cancel` runs BEFORE the abort, a third-party
//! cancel touching only its own item, and a handler panic settling as an
//! outcome instead of unwinding into the caller. None of it is observable over
//! the wire, which is why it is asserted natively rather than in the shared
//! conformance corpus.

use crate::agent::AgentId;
use crate::outcome::{Item, Status};
use crate::tasks::tests::{mk_agent, mk_kernel, Shared};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The live rows, MINUS `ps`'s own. `ps` is a dispatch like any other and the
/// table is honest about that — it shows its own row — so a test counting "work
/// in flight" has to say which row it means.
async fn ps_rows(kernel: &Arc<crate::kernel::Kernel>) -> Vec<serde_json::Value> {
    kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"ps"}),
        )])
        .await
        .remove(0)
        .result["tasks"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["verb"] != "ps")
        .collect()
}

#[tokio::test]
async fn a_mixed_batch_answers_every_item_in_input_order() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_fast").await;

    let out = kernel
        .send(vec![
            Item::new(AgentId::from("nobody_at_all"), json!({"type": "fast"})),
            Item::new(a.clone(), json!({"type": "fast"})),
            Item::new(a.clone(), json!({"type": "boom"})),
        ])
        .await;

    assert_eq!(out.len(), 3, "one outcome per item, always");
    // 0: refused before dispatch — NOTHING ever entered the task table.
    assert_eq!(out[0].status, Status::Failed);
    assert!(out[0].pid.is_none(), "a refusal has no pid");
    assert_eq!(out[0].result["reason"], "unknown_agent");
    // 1: ran and answered.
    assert_eq!(out[1].status, Status::Completed);
    assert!(out[1].pid.is_some());
    assert_eq!(out[1].result["fast"], true);
    // 2: the handler panicked — a RESULT, not a crash of the caller.
    assert_eq!(out[2].status, Status::Failed);
    assert_eq!(out[2].result["raised"], true);
    assert!(out[2].result["error"].as_str().unwrap().contains("kaput"));
}

#[tokio::test]
async fn a_batch_runs_concurrently_not_one_after_another() {
    // Wall-clock ≈ the SLOWEST item, not the sum. This is the whole reason the
    // batch exists: a model turn's N calls must not cost N round-trips.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_conc").await;

    let started = Instant::now();
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        kernel.send(vec![
            Item::new(a.clone(), json!({"type": "slow"})).with_timeout(0.4),
            Item::new(a.clone(), json!({"type": "slow"})).with_timeout(0.4),
            Item::new(a.clone(), json!({"type": "slow"})).with_timeout(0.4),
        ]),
    )
    .await
    .expect("a concurrent batch must not take anywhere near 5s");
    let elapsed = started.elapsed();

    assert!(out.iter().all(|o| o.status == Status::TimedOut));
    assert!(
        elapsed < Duration::from_millis(1_100),
        "three 0.4s items took {elapsed:?} — they ran in series"
    );
}

#[tokio::test]
async fn a_timeout_kills_through_the_front_door() {
    // Expiry runs `cancel_task`, so the bundle's `on_cancel` hook fires BEFORE
    // the abort and frees the real resource. A bare future-drop would strand it.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_to").await;

    let out = kernel
        .send(vec![
            Item::new(a.clone(), json!({"type": "slow"})).with_timeout(0.2)
        ])
        .await;

    assert_eq!(out[0].status, Status::TimedOut);
    assert_eq!(out[0].result["reason"], "timeout");
    assert!(out[0].pid.is_some(), "it DID reach the task table");
    // C31: python's prose, verbatim — target id RAW, verb `py_repr`-quoted,
    // plus `task_id`.
    let pid = out[0].pid.expect("checked above").0;
    assert_eq!(
        out[0].result["error"],
        json!("send: b_to 'slow' exceeded its 0.2s timeout; task cancelled")
    );
    assert_eq!(out[0].result["task_id"], json!(pid));

    // The hook ran, and it ran while the row was still there — the ordering is
    // the point, not the fact.
    let hooks = sh.hook_log();
    assert_eq!(hooks.len(), 1, "on_cancel did not fire: {hooks:?}");
    assert!(hooks[0].2, "the hook ran AFTER the row was gone");
    assert!(
        sh.held_ids().is_empty(),
        "the resource outlived its task: {:?}",
        sh.held_ids()
    );
    assert_eq!(ps_rows(&kernel).await.len(), 0, "the row survived");
}

#[tokio::test]
async fn no_timeout_means_the_kernel_never_invents_one() {
    // Absent = wait forever. The kernel defaulting a deadline would silently
    // kill legitimate long work.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_noto").await;

    let raced = tokio::time::timeout(
        Duration::from_millis(600),
        kernel.send(vec![Item::new(a.clone(), json!({"type": "slow"}))]),
    )
    .await;
    assert!(raced.is_err(), "an item with no timeout settled on its own");
}

#[tokio::test]
async fn a_bad_timeout_is_refused_not_guessed() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_badto").await;

    for bad in [0.0, -1.0, f64::NAN] {
        let out = kernel
            .send(vec![
                Item::new(a.clone(), json!({"type": "fast"})).with_timeout(bad)
            ])
            .await;
        assert_eq!(out[0].status, Status::Failed, "timeout {bad} was accepted");
        assert_eq!(out[0].result["reason"], "bad_arg");
        assert_eq!(out[0].result["arg"], "timeout");
    }
}

#[tokio::test]
async fn a_name_that_looks_like_an_old_pid_is_just_a_name() {
    // THE NAMESPACE IS GONE. A caller used to be refused an id shaped
    // `<agent>:<hex>` because that was the kernel's to mint. It mints numbers
    // now and a caller supplies only NAMES, so there is nothing to collide with
    // and nothing to refuse: this is an ordinary named execution.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_ns").await;

    let out = kernel
        .send(vec![
            Item::new(a.clone(), json!({"type": "fast"})).with_id("b_ns:deadbeef")
        ])
        .await;
    assert_eq!(out[0].status, Status::Completed);
    assert!(out[0].pid.expect("a pid").0 > 0);
}

#[tokio::test]
async fn the_whole_batch_is_visible_in_ps_before_the_first_await() {
    // Pids are minted in a SYNCHRONOUS launch phase. If they were minted as
    // each item awaited, `ps` would show the batch appearing one row at a time
    // and a caller could not cancel an item it had not seen yet.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_ps").await;

    let k = Arc::clone(&kernel);
    let peer = a.clone();
    let batch = tokio::spawn(async move {
        k.send(vec![
            Item::new(peer.clone(), json!({"type": "slow"})).with_timeout(1.0),
            Item::new(peer.clone(), json!({"type": "slow"})).with_timeout(1.0),
        ])
        .await
    });

    let mut rows = 0;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        rows = ps_rows(&kernel).await.len();
        if rows >= 2 {
            break;
        }
    }
    assert_eq!(rows, 2, "the batch did not appear whole in ps");
    let _ = batch.await;
}

#[tokio::test]
async fn cancelling_one_item_leaves_its_siblings_alone() {
    // A third party kills ONE pid mid-batch: that item settles `cancelled`,
    // every other item answers normally, and `send` still returns.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_cx").await;

    let k = Arc::clone(&kernel);
    let peer = a.clone();
    let batch = tokio::spawn(async move {
        k.send(vec![
            Item::new(peer.clone(), json!({"type": "slow"})),
            Item::new(peer.clone(), json!({"type": "fast"})),
        ])
        .await
    });

    let mut victim = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Some(row) = ps_rows(&kernel)
            .await
            .into_iter()
            .find(|r| r["verb"] == "slow")
        {
            victim = row["id"].as_i64();
            break;
        }
    }
    let victim = victim.expect("the slow row never appeared");
    let killed = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "cancel", "task_id": victim}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(killed["cancelled"], true);

    let out = batch.await.expect("send must return, not unwind");
    assert_eq!(out[0].status, Status::Cancelled);
    assert_eq!(out[0].result["cancelled"], true);
    assert_eq!(out[1].status, Status::Completed, "a sibling was collateral");
    assert_eq!(out[1].result["fast"], true);
}

#[tokio::test]
async fn two_items_to_one_agent_both_run() {
    // The substrate does not serialize by target — two calls to one agent are
    // two independent dispatches with two pids.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_two").await;

    let out = kernel
        .send(vec![
            Item::new(a.clone(), json!({"type": "fast"})),
            Item::new(a.clone(), json!({"type": "fast"})),
        ])
        .await;
    assert!(out.iter().all(|o| o.status == Status::Completed));
    assert_ne!(out[0].pid, out[1].pid, "two dispatches, two pids");
}

#[tokio::test]
async fn an_empty_batch_is_an_empty_answer() {
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    assert!(kernel.send(vec![]).await.is_empty());
}
