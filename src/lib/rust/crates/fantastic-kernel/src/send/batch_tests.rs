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
    // batch exists: a model reply's N calls must not cost N round-trips.
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

// ─── parallel EXECUTION, observed (owner, 2026-10-01) ───────────────────────
//
// The wall-clock twin above proves concurrency by timing; this pair proves it
// by OBSERVATION, independent of load: item A's handler holds until item B's
// handler has STARTED. Run one after the other, A could never see that and
// gives up after 3 s ("no overlap"). Run together, B's start releases A. The
// arrival order — the order the items reach their targets (the `send` fanout)
// — is the batch's input order, python's effective order.

/// A releases on B's start. `notify_one` keeps a permit, so B starting BEFORE A
/// waits still counts.
struct Overlap {
    b_started: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::bundle::Bundle for Overlap {
    fn name(&self) -> &str {
        "overlap"
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &serde_json::Value,
        _kernel: &Arc<crate::kernel::Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        let verb = payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        Ok(Some(match verb {
            "boot" | "shutdown" => serde_json::Value::Null,
            "reflect" => json!({"id": agent_id.0, "sentence": "overlap probe", "verbs": {}}),
            "wait_for_b" => {
                let saw =
                    tokio::time::timeout(Duration::from_secs(3), self.b_started.notified()).await;
                json!({"overlapped": saw.is_ok()})
            }
            "b" => {
                self.b_started.notify_one();
                json!({"b": true})
            }
            other => json!({"error": format!("unknown verb {other}")}),
        }))
    }
}

/// A kernel with the overlap bundle and one agent per id; the arrival log is the
/// `send` fanout's own record, in the order the items reached their targets.
async fn overlap_kernel(
    ids: &[&str],
) -> (
    Arc<crate::kernel::Kernel>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let mut kernel = crate::kernel::Kernel::new();
    kernel.bundles.register(
        "overlap.tools",
        Overlap {
            b_started: Arc::new(tokio::sync::Notify::new()),
        },
    );
    let kernel = Arc::new(kernel);
    let root = crate::agent::Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        serde_json::Map::new(),
        std::path::PathBuf::new(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    for id in ids {
        let r = kernel
            .send(vec![Item::new(
                AgentId::from("kernel_state"),
                json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "overlap.tools", "id": id}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["id"], json!(id), "create_agent failed: {r}");
    }
    let arrivals = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&arrivals);
    kernel.add_state_subscriber(Arc::new(move |ev: &serde_json::Value, _| {
        let verb = ev["payload"]["type"].as_str().unwrap_or("");
        if ev["kind"] == "send" && (verb == "wait_for_b" || verb == "b") {
            log.lock()
                .expect("arrivals poisoned")
                .push(format!("{}:{verb}", ev["agent_id"].as_str().unwrap_or("")));
        }
    }));
    (kernel, arrivals)
}

async fn assert_overlap(kernel: &Arc<crate::kernel::Kernel>, a: &str, b: &str) -> Vec<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        kernel.send(vec![
            Item::new(AgentId::from(a), json!({"type": "wait_for_b"})),
            Item::new(AgentId::from(b), json!({"type": "b"})),
        ]),
    )
    .await
    .expect("the batch must settle");
    assert_eq!(out[1].result["b"], true, "{:?}", out[1].result);
    assert_eq!(
        out[0].result["overlapped"], true,
        "A never saw B start — the batch EXECUTED one item after the other: {:?}",
        out[0].result
    );
    vec![format!("{a}:wait_for_b"), format!("{b}:b")]
}

#[tokio::test]
async fn two_items_to_one_agent_execute_at_the_same_time() {
    let (kernel, arrivals) = overlap_kernel(&["ov_one"]).await;
    let want = assert_overlap(&kernel, "ov_one", "ov_one").await;
    assert_eq!(*arrivals.lock().unwrap(), want, "arrival = input order");
}

#[tokio::test]
async fn two_items_to_two_agents_execute_at_the_same_time() {
    let (kernel, arrivals) = overlap_kernel(&["ov_a", "ov_b"]).await;
    let want = assert_overlap(&kernel, "ov_a", "ov_b").await;
    assert_eq!(*arrivals.lock().unwrap(), want, "arrival = input order");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_overlap_holds_on_a_multi_threaded_runtime() {
    let (kernel, arrivals) = overlap_kernel(&["ov_m1", "ov_m2"]).await;
    let want = assert_overlap(&kernel, "ov_m1", "ov_m2").await;
    assert_eq!(*arrivals.lock().unwrap(), want, "arrival = input order");
}

#[tokio::test]
async fn a_returned_raised_dict_settles_errored_not_failed() {
    // python marks FAILED only when the handler RAISES; a returned dict — even
    // `{error, raised: true}`, a relayed peer's answer — settles ERRORED: not
    // retried, not a dead letter (`_agent.py` `_settle_once`).
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_raised").await;
    let out = kernel
        .send(vec![
            Item::new(a.clone(), json!({"type": "raised_reply"})).with_retries(2)
        ])
        .await
        .remove(0);
    assert_eq!(out.status, Status::Errored, "{out:?}");
    assert_eq!(out.result["raised"], true);
    assert_eq!(sh.run_count(), 1, "an errored reply is never retried");
    assert_eq!(out.attempts, 1);
}

#[tokio::test]
async fn the_unnamed_kernel_reflect_honours_its_timeout() {
    // python awaits even the pid-less identity read under `asyncio.wait(timeout)`:
    // on expiry it is cancelled and settles `timed_out` with `task_id: "-"`.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel_state"),
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "slow.tools",
                   "id": "b_slow_reflect", "reflect_ms": 5_000}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "b_slow_reflect", "{r}");
    let started = Instant::now();
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect"}),
        )
        .with_timeout(0.1)])
        .await
        .remove(0);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the deadline was ignored"
    );
    assert_eq!(out.status, Status::TimedOut, "{out:?}");
    assert!(out.pid.is_none());
    assert_eq!(out.result["reason"], "timeout");
    assert_eq!(out.result["task_id"], "-");
    assert_eq!(
        out.result["error"],
        "send: kernel_state 'reflect' exceeded its 0.1s timeout; task cancelled"
    );
}

#[tokio::test]
async fn timeout_prose_and_row_keep_the_number_the_caller_gave() {
    // `f"{d.timeout}s"` keeps python's number: an int prints `1s`, a float `0.1s`
    // / `1e-07s`; the call row stores the value as given.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "b_prose").await;
    let out = kernel
        .send(vec![
            Item::new(a.clone(), json!({"type": "slow"})).with_raw_timeout(json!(1)),
            Item::new(a.clone(), json!({"type": "slow"})).with_raw_timeout(json!(1e-7)),
            Item::new(a.clone(), json!({"type": "slow"})).with_timeout(0.1),
        ])
        .await;
    let text = |i: usize| out[i].result["error"].as_str().unwrap_or("").to_string();
    assert_eq!(
        text(0),
        "send: b_prose 'slow' exceeded its 1s timeout; task cancelled"
    );
    assert_eq!(
        text(1),
        "send: b_prose 'slow' exceeded its 1e-07s timeout; task cancelled"
    );
    assert_eq!(
        text(2),
        "send: b_prose 'slow' exceeded its 0.1s timeout; task cancelled"
    );
    let pid = out[0].pid.expect("dispatched").0;
    let row = kernel.rows.get_one(pid).expect("the call row");
    assert_eq!(
        row["timeout"].to_string(),
        "1",
        "the row keeps the int: {row:?}"
    );
}

#[tokio::test]
async fn a_named_kernel_reflect_is_ledgered_but_not_dispatched() {
    // python's caller-NAMED `kernel` reflect opens the ledger and then runs the
    // flags-applied identity read as a bare task: no task row, no watcher
    // fanout, no `send`/`drain` state events (`_launch`'s `flags_done` branch).
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let tap = AgentId::from("tap_named_reflect");
    let mut rx = kernel.ensure_inbox(&tap);
    assert!(kernel
        .watch(&AgentId::from("kernel_state"), tap.clone())
        .await
        .is_none());
    let seen: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
    let sink = Arc::clone(&seen);
    let _t = kernel.add_state_subscriber(Arc::new(move |ev, _| {
        sink.lock().unwrap().push(ev.clone());
    }));
    let out = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect"}),
        )
        .with_id("frame-7")])
        .await
        .remove(0);
    assert_eq!(out.status, Status::Completed, "{out:?}");
    let pid = out.pid.expect("a named execution carries its id").0;
    assert_eq!(out.result["id"], "kernel_state");
    assert!(rx.try_recv().is_none(), "the root's watcher was mirrored");
    let traffic: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|e| {
            e["agent_id"] == "kernel_state" && (e["kind"] == "send" || e["kind"] == "drain")
        })
        .cloned()
        .collect();
    assert!(
        traffic.is_empty(),
        "dispatch telemetry for a pure read: {traffic:?}"
    );
    // The ledger still applies: the call row exists and the name joins it.
    assert_eq!(
        kernel.rows.get_one(pid).expect("call row")["verb"],
        "reflect"
    );
    let again = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect"}),
        )
        .with_id("frame-7")])
        .await
        .remove(0);
    assert_eq!(
        again.pid.map(|p| p.0),
        Some(pid),
        "the name joins its execution"
    );
}

#[tokio::test]
async fn a_dead_letter_of_a_departed_caller_is_announced_on_the_root() {
    // python `announce_deadletter` calls `src._fanout(entry, kind="emit")`
    // directly — never gated. When the caller has left the tree the src falls
    // back to the root; a GATED emit asked the departed caller's grant (none)
    // and dropped the announcement.
    let sh = Arc::new(Shared::default());
    let kernel = mk_kernel(Arc::clone(&sh));
    let a = mk_agent(&kernel, "dl_caller").await;
    let r = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": a.as_str(),
                   "handler_module": "slow.tools", "id": "dl_child"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["id"], "dl_child", "{r}");
    let tap = AgentId::from("tap_deadletter");
    let mut rx = kernel.ensure_inbox(&tap);
    assert!(kernel
        .watch(&AgentId::from("kernel_state"), tap.clone())
        .await
        .is_none());
    let k = Arc::clone(&kernel);
    let call = tokio::spawn(crate::send::with_sender(a.clone(), async move {
        k.send(vec![Item::new(
            AgentId::from("dl_child"),
            json!({"type": "slow"}),
        )
        .with_timeout(0.3)])
            .await
            .remove(0)
    }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The caller leaves the index while its call is in flight.
    kernel.agents.remove(&a);
    let out = call.await.expect("the batch settles");
    assert_eq!(out.status, Status::TimedOut, "{out:?}");
    let mut announced = None;
    while let Some(item) = rx.try_recv() {
        if item.payload["type"] == "on_deadletter" {
            announced = Some(item.payload);
        }
    }
    let ev = announced.expect("the dead letter was announced on the root");
    assert_eq!(ev["caller"], "dl_caller");
    assert_eq!(ev["status"], "timed_out");
}
