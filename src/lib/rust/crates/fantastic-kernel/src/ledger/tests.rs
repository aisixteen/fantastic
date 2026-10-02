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
    let opened = l.open(
        id,
        &json!(name),
        &tid(target),
        &json!({"type": "probe"}),
        retries,
        Some(name),
        None,
    );
    (id, opened)
}

/// A `call` ROW as the kernel's store holds it — what `put_settled` reads the
/// target, the payload, the retry budget, the name and the caller off.
fn call_row(id: i64, agent: &str, retries: u32, name: Option<&str>) -> crate::rows::Row {
    let mut r = Map::new();
    r.insert("id".to_string(), json!(id));
    r.insert("agent".to_string(), json!(agent));
    r.insert("kind".to_string(), json!("call"));
    r.insert("payload".to_string(), json!({"type": "probe"}));
    r.insert("retries".to_string(), json!(retries));
    r.insert("named".to_string(), json!(name.is_some()));
    if let Some(n) = name {
        r.insert("name".to_string(), json!(n));
    }
    r
}

/// The `{status, result}` a `settle` row carries.
fn settle_outcome(status: &str, result: Value) -> Map<String, Value> {
    let mut o = Map::new();
    o.insert("status".to_string(), json!(status));
    o.insert("result".to_string(), result);
    o
}

/// The RAM entries a viewer sees — the `ledger` verb's caller scope (`None` =
/// the operator, who sees all).
fn ids_for(l: &Ledger, viewer: Option<&str>) -> Vec<i64> {
    l.entries()
        .into_iter()
        .filter(|(_, e, _)| match viewer {
            None => true,
            Some(v) => e["_caller"].as_str() == Some(v),
        })
        .map(|(id, _, _)| id)
        .collect()
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
    //
    // PYTHON'S PROSE, verbatim (C31 r2 correction): the CALLER's raw id
    // (`py_repr`-quoted for a string name), never the minted exec_id; both
    // target names via `py_repr`; no `bound_to` key.
    let l = Ledger::default();
    let _ = open(&l, "job-3", "desk", 0).1;
    match open(&l, "job-3", "elsewhere", 0).1 {
        Opened::Refused(r) => {
            assert_eq!(r["reason"], "bad_arg");
            assert_eq!(r["arg"], "id");
            assert_eq!(
                r["error"],
                "send: id 'job-3' already names an execution on 'desk', not \
                 'elsewhere' — an id is bound to its first execution; a \
                 retry aimed elsewhere needs a new id"
            );
            assert!(r.get("bound_to").is_none());
        }
        _ => panic!("a re-aimed id must refuse"),
    }
}

#[test]
fn an_int_id_rebind_renders_bare_digits() {
    // The r2 correction's other half: an INT caller id renders as bare
    // digits (python `{exec_id!r}` on an int has no quotes), not as a
    // string. Exercises the `caller_id` parameter directly, since the int
    // JOIN path itself is wired at `send::Kernel::resolve_exec_id` (C38).
    let l = Ledger::default();
    let _ = l.open(
        301,
        &json!(301),
        &tid("desk"),
        &json!({"type": "probe"}),
        0,
        None,
        None,
    );
    match l.open(
        301,
        &json!(301),
        &tid("elsewhere"),
        &json!({"type": "probe"}),
        0,
        None,
        None,
    ) {
        Opened::Refused(r) => {
            assert_eq!(
                r["error"],
                "send: id 301 already names an execution on 'desk', not \
                 'elsewhere' — an id is bound to its first execution; a \
                 retry aimed elsewhere needs a new id"
            );
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
fn the_view_is_caller_scoped() {
    // Unscoped, `ledger` would be a side-channel around `callable_by`: an agent
    // could read result BODIES of agents it was never granted an edge to.
    let l = Ledger::default();
    let _ = l.open(
        101,
        &json!("mine"),
        &tid("desk"),
        &json!({"type": "probe"}),
        0,
        Some("mine"),
        Some(tid("alice")),
    );
    let _ = l.open(
        102,
        &json!("yours"),
        &tid("desk"),
        &json!({"type": "probe"}),
        0,
        Some("yours"),
        Some(tid("bob")),
    );
    let _ = open(&l, "operators", "desk", 0).1;

    assert_eq!(ids_for(&l, Some("alice")), vec![101]);

    // The operator sees everything.
    assert_eq!(ids_for(&l, None).len(), 3);
}

#[test]
fn an_in_flight_row_reads_as_in_flight() {
    let l = Ledger::default();
    let _ = open(&l, "live", "desk", 0).1;
    let rows = l.entries();
    assert_eq!(rows[0].1["status"], "in_flight");
    assert!(rows[0].1.get("outcome").is_none(), "nothing to report yet");
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
        let _ = l.open(
            id,
            &Value::Null,
            &tid("d"),
            &json!({"type": "probe"}),
            0,
            None,
            None,
        );
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
    let ids: Vec<i64> = ids_for(&l, None);

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
    assert_eq!(ids_for(&l, None).len(), 50);
}

#[test]
fn a_settled_call_comes_back_from_its_store_rows() {
    // A `call` row plus its `settle` row IS the durable execution: the recalled
    // entry is DONE, so a re-send of the name returns the recorded outcome and
    // the handler never runs again — after the flush, and across a restart.
    let l = Ledger::default();
    assert!(l.put_settled(
        201,
        &call_row(201, "desk", 1, Some("done-1")),
        &settle_outcome("completed", json!({"v": "kept"})),
        1,
    ));
    match open(&l, "done-1", "desk", 0).1 {
        Opened::Settled(o, payload) => {
            assert_eq!(o.result["v"], "kept");
            assert_eq!(o.status, Status::Completed);
            // THE LIVE SHAPE: a joiner after a restart is handed exactly what a
            // joiner before it was handed.
            assert_eq!(o.pid, Some(crate::tasks::TaskId(201)));
            assert_eq!(o.attempts, 1);
            // THE ORIGINAL CALL's payload: a joiner is authorized against it.
            assert_eq!(payload, json!({"type": "probe"}));
        }
        _ => panic!("a recalled entry must dedup a re-send"),
    }
    // ...and its provenance came back with it.
    let mut call = call_row(202, "desk", 2, Some("caller-1"));
    call.insert("caller".to_string(), json!("alice"));
    assert!(l.put_settled(202, &call, &settle_outcome("completed", json!({})), 3));
    let alice: Vec<(i64, Map<String, Value>, bool)> = l
        .entries()
        .into_iter()
        .filter(|(_, e, _)| e["_caller"] == json!("alice"))
        .collect();
    assert_eq!(alice.len(), 1, "caller scoping survived the recall");
    assert_eq!(alice[0].1["retries"], 2);
    assert_eq!(alice[0].1["attempts"], 3);
}

#[test]
fn an_unnamed_settle_comes_back_as_what_it_kept() {
    // An UNNAMED call's settle kept only `{status, bytes}` — the recalled result
    // says so (the kept fields minus `status`), it does not pretend to a body.
    let l = Ledger::default();
    let mut oc = Map::new();
    oc.insert("status".to_string(), json!("completed"));
    oc.insert("bytes".to_string(), json!(12));
    assert!(l.put_settled(301, &call_row(301, "desk", 0, None), &oc, 1));
    assert!(l.is_done(301));
    // Joined by its int id, the recalled outcome IS the kept digest.
    match l.open(
        301,
        &json!(301),
        &tid("desk"),
        &json!({"type": "probe"}),
        0,
        None,
        None,
    ) {
        Opened::Settled(o, _) => assert_eq!(o.result, json!({"bytes": 12})),
        _ => panic!("a recalled execution returns what the store kept"),
    }
}

#[test]
fn a_live_row_and_an_unknown_status_are_left_alone() {
    let l = Ledger::default();
    let (live, _) = open(&l, "live-1", "desk", 0);
    // A LIVE ENTRY ALWAYS WINS over the store — the process knows better.
    assert!(!l.put_settled(
        live,
        &call_row(live, "elsewhere", 0, Some("live-1")),
        &settle_outcome("completed", json!({})),
        1
    ));
    assert!(matches!(open(&l, "live-1", "desk", 0).1, Opened::Join(..)));
    // AN UNKNOWN STATUS IS NOT GUESSED AT.
    assert!(!l.put_settled(
        900,
        &call_row(900, "desk", 0, None),
        &settle_outcome("in_flight", json!({})),
        1
    ));
    // Neither is a row with no target.
    assert!(!l.put_settled(901, &Map::new(), &settle_outcome("completed", json!({})), 1));
    assert!(!l.has(900) && !l.has(901));
}

#[test]
fn a_parked_dead_letter_stays_dead_across_a_restart() {
    let l = Ledger::default();
    let _ = open(&l, "dead-1", "desk", 1).1;
    l.attempt(id_of(&l, "dead-1"));
    l.attempt(id_of(&l, "dead-1"));
    l.settle(
        id_of(&l, "dead-1"),
        Outcome::new(None, Status::Failed, json!({"error": "kaput"})),
    );

    let l2 = Ledger::default();
    assert!(l2.put_settled(
        id_of(&l, "dead-1"),
        &call_row(id_of(&l, "dead-1"), "desk", 1, Some("dead-1")),
        &settle_outcome("failed", json!({"error": "kaput"})),
        2,
    ));
    // Re-sending the SAME NAME returns the recorded failure — it is not retried
    // silently. A retry is an explicit NEW name.
    match open(&l2, "dead-1", "desk", 5).1 {
        Opened::Settled(o, _) => {
            assert_eq!(o.status, Status::Failed);
            assert_eq!(o.attempts, 2);
        }
        _ => panic!("a dead letter must not re-arm"),
    }
}

#[test]
fn forget_drops_the_entry_and_its_name() {
    let l = Ledger::default();
    let (id, _) = open(&l, "gone-1", "desk", 0);
    l.attempt(id);
    l.settle(id, done("x"));
    assert!(l.is_done(id));
    l.forget(id);
    assert!(!l.has(id));
    assert_eq!(l.resolve_name("gone-1"), None, "the name left with it");
}

#[test]
fn stored_outcome_keeps_a_named_result_a_dead_error_else_a_digest() {
    use crate::jsonsafe::BodyAs;
    let ok = Outcome::new(None, Status::Completed, json!({"ok": true}));
    assert_eq!(
        stored_outcome(&ok, true, BodyAs::Len),
        json!({"status": "completed", "result": {"ok": true}})
    );
    // python `len(json.dumps({"ok": True}))` — separators and all.
    assert_eq!(
        stored_outcome(&ok, false, BodyAs::Len),
        json!({"status": "completed", "bytes": 12})
    );
    let long = "x".repeat(900);
    let dead = Outcome::new(
        None,
        Status::Failed,
        json!({"error": long, "reason": "unhandled", "raised": true}),
    );
    let kept = stored_outcome(&dead, false, BodyAs::Len);
    assert_eq!(kept["status"], json!("failed"));
    assert_eq!(kept["error"].as_str().unwrap().len(), 500, "cut at 500");
    assert_eq!(kept["reason"], json!("unhandled"));
    assert!(kept.get("result").is_none() && kept.get("raised").is_none());
    // a raw body is recorded as its length in the store, a marker on the wire
    let read = Outcome::new(None, Status::Completed, json!({"bytes": null, "eof": true}))
        .with_body(Some(crate::outcome::Blob::key("bytes", b"abc".to_vec())));
    assert_eq!(
        stored_outcome(&read, true, BodyAs::Len)["result"]["bytes"],
        json!({"len": 3})
    );
    assert_eq!(
        stored_outcome(&read, true, BodyAs::Marker)["result"]["bytes"],
        json!("<bytes:3>")
    );
}

/// D10 — a reply with a BINARY BODY is kept WHOLE in the ledger.
///
/// The store records the body as its LENGTH, never the bytes; the live entry
/// keeps the bytes, so a joiner in this process still gets them.
#[test]
fn a_reply_with_a_binary_body_stays_whole_in_the_ledger() {
    let l = Ledger::default();
    let (streamed, _) = open(&l, "read-1", "store", 0);
    l.attempt(streamed);
    l.settle(
        streamed,
        Outcome::new(
            None,
            Status::Completed,
            json!({"path": "agent.yaml", "offset": 0, "bytes": null,
                   "next_offset": 326, "eof": true, "size": 326}),
        )
        .with_body(Some(crate::outcome::Blob::key(
            "bytes",
            b"{\"id\": \"kernel_state\"}".to_vec(),
        ))),
    );

    match open(&l, "read-1", "store", 0).1 {
        Opened::Settled(o, _) => {
            assert_eq!(o.body_bytes(), Some(&b"{\"id\": \"kernel_state\"}"[..]));
        }
        _ => panic!("expected the settled outcome"),
    }
    // A RECALLED entry has no body: the store kept the length, never the bytes.
    let l2 = Ledger::default();
    assert!(l2.put_settled(
        streamed,
        &call_row(streamed, "store", 0, Some("read-1")),
        &settle_outcome("completed", json!({"bytes": {"len": 22}})),
        1,
    ));
    match open(&l2, "read-1", "store", 0).1 {
        Opened::Settled(o, _) => {
            assert!(o.body.is_none());
            assert_eq!(o.result["bytes"]["len"], 22);
        }
        _ => panic!("expected the recalled outcome"),
    }
}
