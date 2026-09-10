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

/// THE KERNEL'S COUNTER, stood in for. In production `send` mints from
/// `kernel.rows`; here the test is the allocator, and the point of the helper is
/// the rule it enforces: a NAME the ledger already knows resolves to that
/// execution's id, and only an unknown name gets a fresh one.
fn open(l: &Ledger, name: &str, target: &str, retries: u32) -> (i64, Opened) {
    use std::sync::atomic::{AtomicI64, Ordering};
    static NEXT: AtomicI64 = AtomicI64::new(1);
    let id = l
        .resolve_name(name)
        .unwrap_or_else(|| NEXT.fetch_add(1, Ordering::Relaxed));
    let opened = l.open(id, &tid(target), "probe", retries, Some(name), None);
    (id, opened)
}

/// The id a name is bound to — for the tests that then `attempt`/`settle` it.
fn id_of(l: &Ledger, name: &str) -> i64 {
    l.resolve_name(name).expect("no execution under that name")
}

#[test]
fn an_unknown_id_opens_fresh_and_a_settled_one_returns_its_outcome() {
    let l = Ledger::default();
    assert!(matches!(open(&l, "job-1", "desk", 0).1, Opened::Fresh));
    l.attempt(id_of(&l, "job-1"));
    l.settle(id_of(&l, "job-1"), done("first"));

    // C11: the recorded outcome comes back and NOTHING runs again.
    match open(&l, "job-1", "desk", 0).1 {
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
    assert!(matches!(open(&l, "job-2", "desk", 0).1, Opened::Fresh));
    l.attempt(id_of(&l, "job-2"));

    let Opened::Join(mut rx, _) = open(&l, "job-2", "desk", 0).1 else {
        panic!("an in-flight id must be joinable");
    };
    assert!(rx.borrow_and_update().is_none(), "not settled yet");

    let l2 = Arc::clone(&l);
    tokio::spawn(async move {
        l2.settle(id_of(&l2, "job-2"), done("owner"));
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
    let _ = open(&l, "job-3", "desk", 0).1;
    match open(&l, "job-3", "elsewhere", 0).1 {
        Opened::Refused(r) => {
            assert_eq!(r["reason"], "bad_arg");
            assert_eq!(r["bound_to"], "desk");
        }
        _ => panic!("a re-aimed id must refuse"),
    }
}

#[test]
fn a_name_is_just_a_name_and_the_kernel_owns_every_id() {
    // THE NAMESPACE IS GONE. There used to be a `<agent>:<hex>` id space a
    // caller could trespass in, and a rule refusing an unknown colon id as a
    // dangling reference. Neither is needed now: a caller supplies only a NAME,
    // the kernel supplies every id, and a name that looks like an old pid is
    // just an unusual name.
    let l = Ledger::default();
    let (id, opened) = open(&l, "desk:0badf00d", "desk", 0);
    assert!(matches!(opened, Opened::Fresh));
    assert_eq!(l.resolve_name("desk:0badf00d"), Some(id));
    // And re-sending it JOINS, like any other name.
    assert!(matches!(
        open(&l, "desk:0badf00d", "desk", 0).1,
        Opened::Join(..)
    ));
}

#[test]
fn the_view_is_origin_scoped() {
    // Unscoped, `ledger` would be a side-channel around `callable_by`: an agent
    // could read result BODIES of agents it was never granted an edge to.
    let l = Ledger::default();
    let _ = l.open(
        101,
        &tid("desk"),
        "probe",
        0,
        Some("mine"),
        Some(tid("alice")),
    );
    let _ = l.open(
        102,
        &tid("desk"),
        "probe",
        0,
        Some("yours"),
        Some(tid("bob")),
    );
    let _ = open(&l, "operators", "desk", 0).1;

    let alice: Vec<i64> = l
        .rows_for(Some(&tid("alice")))
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(alice, vec![101]);

    // The operator sees everything.
    assert_eq!(l.rows_for(None).len(), 3);
}

#[test]
fn an_in_flight_row_reads_as_in_flight() {
    let l = Ledger::default();
    let _ = open(&l, "live", "desk", 0).1;
    let rows = l.rows_for(None);
    assert_eq!(rows[0]["status"], "in_flight");
    assert!(rows[0].get("outcome").is_none(), "nothing to report yet");
}

#[test]
fn the_ring_evicts_settled_unnamed_before_named_and_never_in_flight() {
    let l = Ledger::new(Some(3));
    // one live row that must survive every pass
    let (live, _) = open(&l, "live", "d", 0);
    // UNNAMED: no caller handle, so no dedup contract — the ring takes these
    // first.
    let mut unnamed = Vec::new();
    for i in 0..3 {
        let id = 1000 + i;
        let _ = l.open(id, &tid("d"), "probe", 0, None, None);
        l.attempt(id);
        l.settle(id, done("u"));
        unnamed.push(id);
    }
    let mut named = Vec::new();
    for i in 0..3 {
        let (id, _) = open(&l, &format!("named-{i}"), "d", 0);
        l.attempt(id);
        l.settle(id, done("n"));
        named.push(id);
    }
    let ids: Vec<i64> = l
        .rows_for(None)
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();

    assert!(ids.contains(&live), "in-flight was evicted");
    // Unnamed went first: a named row is a dedup CONTRACT, and evicting it
    // silently re-arms a side effect.
    assert!(
        !unnamed.iter().any(|u| ids.contains(u)),
        "named rows were evicted before unnamed: {ids:?}"
    );
    assert!(named.iter().any(|n| ids.contains(n)));
    // AND THE NAME BINDING WENT WITH THE ROW: an evicted execution's name must
    // not resolve to a row that is gone.
    for i in 0..3 {
        let name = format!("named-{i}");
        if let Some(id) = l.resolve_name(&name) {
            assert!(ids.contains(&id), "{name} resolves to an evicted row");
        }
    }
}

#[test]
fn an_unbounded_ledger_keeps_everything() {
    let l = Ledger::new(None);
    for i in 0..50 {
        let (id, _) = open(&l, &format!("j{i}"), "d", 0);
        l.attempt(id);
        l.settle(id, done("x"));
    }
    assert_eq!(l.rows_for(None).len(), 50);
}

#[test]
fn only_settled_rows_persist_and_they_round_trip() {
    // Work in flight at a crash never completed, so a post-reboot re-send
    // re-running it is the TRUTH, not a loss.
    let l = Ledger::default();
    let _ = l.open(
        201,
        &tid("desk"),
        "probe",
        1,
        Some("done-1"),
        Some(tid("alice")),
    );
    l.attempt(id_of(&l, "done-1"));
    l.settle(id_of(&l, "done-1"), done("kept"));
    let _ = open(&l, "live-1", "desk", 0).1;

    let live = id_of(&l, "live-1");
    let snap = l.settled_snapshot();
    assert_eq!(snap["v"], 1);
    // The sidecar keys rows by the KERNEL id (as a decimal string — JSON object
    // keys are strings) and carries the caller's name as a field, which is what
    // lets a name still join its execution after a reboot.
    assert_eq!(snap["rows"]["201"]["name"], "done-1");
    assert!(
        snap["rows"].get(live.to_string()).is_none(),
        "in-flight work must not persist"
    );

    // A FRESH ledger rehydrates it DONE, so dedup holds across the reboot.
    let l2 = Ledger::default();
    assert_eq!(l2.rehydrate(&snap), 1);
    match open(&l2, "done-1", "desk", 0).1 {
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
    let _ = open(&l, "dead-1", "desk", 1).1;
    l.attempt(id_of(&l, "dead-1"));
    l.attempt(id_of(&l, "dead-1"));
    l.settle(
        id_of(&l, "dead-1"),
        Outcome::new(None, Status::Failed, json!({"error": "kaput"})),
    );

    let l2 = Ledger::default();
    l2.rehydrate(&l.settled_snapshot());
    // Re-sending the SAME NAME returns the recorded failure — it is not retried
    // silently. A retry is an explicit NEW name. The name binding survives the
    // sidecar, which is what makes that hold across a reboot.
    match open(&l2, "dead-1", "desk", 5).1 {
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
