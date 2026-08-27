//! The execution ledger — dedup, join, retry, dead-letter, the ring.

use super::*;
use crate::outcome::Outcome;
use std::sync::Arc;

fn tid(s: &str) -> AgentId {
    AgentId::from(s)
}

fn done(v: &str) -> Outcome {
    Outcome::new(None, Status::Completed, json!({ "v": v }))
}

#[test]
fn an_unknown_id_opens_fresh_and_a_settled_one_returns_its_outcome() {
    let l = Ledger::default();
    assert!(matches!(
        l.open("job-1", &tid("desk"), "probe", 0, true, None),
        Opened::Fresh
    ));
    l.attempt("job-1");
    l.settle("job-1", done("first"));

    // C11: the recorded outcome comes back and NOTHING runs again.
    match l.open("job-1", &tid("desk"), "probe", 0, true, None) {
        Opened::Settled(o, _) => {
            assert_eq!(o.result["v"], "first");
            assert_eq!(o.attempts, 1);
        }
        _ => panic!("a settled id must return its outcome"),
    }
}

#[tokio::test]
async fn a_second_caller_joins_instead_of_running_twice() {
    // C10: once-only under concurrency. The joiner waits on the OWNER's
    // outcome; it never gets a dispatch of its own.
    let l = Arc::new(Ledger::default());
    assert!(matches!(
        l.open("job-2", &tid("desk"), "probe", 0, true, None),
        Opened::Fresh
    ));
    l.attempt("job-2");

    let Opened::Join(mut rx, _) = l.open("job-2", &tid("desk"), "probe", 0, true, None) else {
        panic!("an in-flight id must be joinable");
    };
    assert!(rx.borrow_and_update().is_none(), "not settled yet");

    let l2 = Arc::clone(&l);
    tokio::spawn(async move {
        l2.settle("job-2", done("owner"));
    });

    rx.changed().await.expect("the settle must wake the joiner");
    let got = rx.borrow().clone().expect("an outcome");
    assert_eq!(got.result["v"], "owner");
}

#[test]
fn an_id_is_bound_to_its_first_target() {
    // The same id aimed elsewhere is a different intent wearing the same name.
    // Returning the other execution's outcome would be silently wrong.
    let l = Ledger::default();
    let _ = l.open("job-3", &tid("desk"), "probe", 0, true, None);
    match l.open("job-3", &tid("elsewhere"), "probe", 0, true, None) {
        Opened::Refused(r) => {
            assert_eq!(r["reason"], "bad_arg");
            assert_eq!(r["bound_to"], "desk");
        }
        _ => panic!("a re-aimed id must refuse"),
    }
}

#[test]
fn a_colon_id_references_and_never_creates() {
    // `<agent>:<hex>` is the kernel's namespace. An unknown one is a DANGLING
    // REFERENCE, not a new job — a caller cannot mint into that space.
    let l = Ledger::default();
    match l.open("desk:0badf00d", &tid("desk"), "probe", 0, true, None) {
        Opened::Refused(r) => {
            assert_eq!(r["reason"], "bad_arg");
            assert!(r["error"].as_str().unwrap().contains("REFERENCES"));
        }
        _ => panic!("an unknown colon id must refuse"),
    }
    // An UNNAMED execution is ledgered under its minted pid, which does contain
    // a colon — that is the kernel doing it, not a caller.
    assert!(matches!(
        l.open("desk:0badf00d", &tid("desk"), "probe", 0, false, None),
        Opened::Fresh
    ));
}

#[test]
fn the_retry_budget_is_spent_then_the_row_is_a_dead_letter() {
    let l = Ledger::default();
    let _ = l.open("flaky", &tid("desk"), "probe", 2, true, None);

    // Two attempts have budget left...
    assert_eq!(l.attempt("flaky"), 1);
    assert!(l.should_retry("flaky", Status::Failed));
    assert_eq!(l.attempt("flaky"), 2);
    assert!(l.should_retry("flaky", Status::TimedOut));
    // ...the third spends it.
    assert_eq!(l.attempt("flaky"), 3);
    assert!(!l.should_retry("flaky", Status::Failed), "budget overrun");

    // A CANCEL is absorbing at every point — an operator kill is a decision.
    let l2 = Ledger::default();
    let _ = l2.open("killed", &tid("desk"), "probe", 5, true, None);
    l2.attempt("killed");
    assert!(!l2.should_retry("killed", Status::Cancelled));
    // ...and a success never retries either.
    assert!(!l2.should_retry("killed", Status::Completed));
}

#[test]
fn a_settled_outcome_carries_what_the_work_cost() {
    let l = Ledger::default();
    let _ = l.open("costly", &tid("desk"), "probe", 3, true, None);
    l.attempt("costly");
    l.attempt("costly");
    let o = l.settle("costly", Outcome::new(None, Status::Failed, json!({})));
    assert_eq!(o.attempts, 2, "the caller must see what it actually cost");
}

#[test]
fn the_view_is_origin_scoped() {
    // Unscoped, `ledger` would be a side-channel around `callable_by`: an agent
    // could read result BODIES of agents it was never granted an edge to.
    let l = Ledger::default();
    let _ = l.open("mine", &tid("desk"), "probe", 0, true, Some(tid("alice")));
    let _ = l.open("yours", &tid("desk"), "probe", 0, true, Some(tid("bob")));
    let _ = l.open("operators", &tid("desk"), "probe", 0, true, None);

    let alice: Vec<String> = l
        .rows_for(Some(&tid("alice")))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(alice, vec!["mine"]);

    // The operator sees everything.
    assert_eq!(l.rows_for(None).len(), 3);
}

#[test]
fn an_in_flight_row_reads_as_in_flight() {
    let l = Ledger::default();
    let _ = l.open("live", &tid("desk"), "probe", 0, true, None);
    let rows = l.rows_for(None);
    assert_eq!(rows[0]["status"], "in_flight");
    assert!(rows[0].get("outcome").is_none(), "nothing to report yet");
}

#[test]
fn the_ring_evicts_settled_unnamed_before_named_and_never_in_flight() {
    let l = Ledger::new(Some(3));
    // one live row that must survive every pass
    let _ = l.open("live", &tid("d"), "probe", 0, true, None);
    for i in 0..3 {
        let id = format!("unnamed-{i}");
        let _ = l.open(&id, &tid("d"), "probe", 0, false, None);
        l.attempt(&id);
        l.settle(&id, done("u"));
    }
    for i in 0..3 {
        let id = format!("named-{i}");
        let _ = l.open(&id, &tid("d"), "probe", 0, true, None);
        l.attempt(&id);
        l.settle(&id, done("n"));
    }
    let ids: Vec<String> = l
        .rows_for(None)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();

    assert!(ids.contains(&"live".to_string()), "in-flight was evicted");
    // Unnamed went first: a named row is a dedup CONTRACT, and evicting it
    // silently re-arms a side effect.
    assert!(
        !ids.iter().any(|i| i.starts_with("unnamed-")),
        "named rows were evicted before unnamed: {ids:?}"
    );
    assert!(ids.iter().any(|i| i.starts_with("named-")));
}

#[test]
fn an_unbounded_ledger_keeps_everything() {
    let l = Ledger::new(None);
    for i in 0..50 {
        let id = format!("j{i}");
        let _ = l.open(&id, &tid("d"), "probe", 0, true, None);
        l.attempt(&id);
        l.settle(&id, done("x"));
    }
    assert_eq!(l.rows_for(None).len(), 50);
}

#[test]
fn only_settled_rows_persist_and_they_round_trip() {
    // Work in flight at a crash never completed, so a post-reboot re-send
    // re-running it is the TRUTH, not a loss.
    let l = Ledger::default();
    let _ = l.open("done-1", &tid("desk"), "probe", 1, true, Some(tid("alice")));
    l.attempt("done-1");
    l.settle("done-1", done("kept"));
    let _ = l.open("live-1", &tid("desk"), "probe", 0, true, None);

    let snap = l.settled_snapshot();
    assert_eq!(snap["v"], 1);
    assert!(snap["rows"].get("done-1").is_some());
    assert!(
        snap["rows"].get("live-1").is_none(),
        "in-flight work must not persist"
    );

    // A FRESH ledger rehydrates it DONE, so dedup holds across the reboot.
    let l2 = Ledger::default();
    assert_eq!(l2.rehydrate(&snap), 1);
    match l2.open("done-1", &tid("desk"), "probe", 0, true, None) {
        Opened::Settled(o, _) => {
            assert_eq!(o.result["v"], "kept");
            assert_eq!(o.status, Status::Completed);
        }
        _ => panic!("a rehydrated row must dedup a re-send"),
    }
    // ...and its provenance survived.
    let rows = l2.rows_for(Some(&tid("alice")));
    assert_eq!(rows.len(), 1, "origin scoping survived the round-trip");
    assert_eq!(rows[0]["retries"], 1);
}

#[test]
fn a_parked_dead_letter_stays_dead_across_a_reboot() {
    let l = Ledger::default();
    let _ = l.open("dead-1", &tid("desk"), "probe", 1, true, None);
    l.attempt("dead-1");
    l.attempt("dead-1");
    l.settle(
        "dead-1",
        Outcome::new(None, Status::Failed, json!({"error": "kaput"})),
    );

    let l2 = Ledger::default();
    l2.rehydrate(&l.settled_snapshot());
    // Re-sending the SAME id returns the recorded failure — it is not retried
    // silently. A retry is an explicit NEW id.
    match l2.open("dead-1", &tid("desk"), "probe", 5, true, None) {
        Opened::Settled(o, _) => {
            assert_eq!(o.status, Status::Failed);
            assert_eq!(o.attempts, 2);
        }
        _ => panic!("a dead letter must not re-arm"),
    }
}

#[test]
fn a_rehydrate_of_garbage_keeps_the_kernel_alive() {
    let l = Ledger::default();
    // Rows missing their target/outcome are skipped, not fatal.
    assert_eq!(l.rehydrate(&json!({"rows": {"x": {"target": "d"}}})), 0);
    assert_eq!(l.rehydrate(&json!({"rows": []})), 0);
    assert_eq!(l.rehydrate(&json!({})), 0);
    assert!(l.rows_for(None).is_empty());
}
