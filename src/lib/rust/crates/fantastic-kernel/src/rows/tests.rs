//! The store's contract. Twin of python's `tests/kernel/test_rows.py` plus the
//! tail cases `test_message_persistence.py` pins.

use super::*;
use serde_json::json;

fn row(kind: &str, content: &str) -> Row {
    json!({"kind": kind, "content": content})
        .as_object()
        .unwrap()
        .clone()
}

fn aid(s: &str) -> AgentId {
    AgentId::from(s)
}

#[test]
fn ids_come_from_one_counter_starting_at_one() {
    let s = RowStore::new();
    assert_eq!(s.next_id(), 1);
    assert_eq!(s.mint_id(), 1);
    assert_eq!(s.mint_id(), 2);
    // A put with no id takes the next one — the SAME counter, not a second.
    assert_eq!(s.put(&aid("ai"), row("turn", "hi")).unwrap(), 3);
    assert_eq!(s.next_id(), 4);
}

#[test]
fn a_pre_minted_id_is_accepted_and_anything_else_refused() {
    let s = RowStore::new();
    let reserved = s.mint_id(); // 1
    let _ = s.mint_id(); // 2
    let mut r = row("turn", "late");
    r.insert("id".to_string(), json!(reserved));
    assert_eq!(s.put(&aid("ai"), r.clone()).unwrap(), reserved);
    // The same id twice is the hydrator's own mistake, said loudly.
    assert!(s.put(&aid("ai"), r).unwrap_err().contains("pre-minted"));
    for bad in [json!(0), json!(-1), json!(99), json!(true), json!("7")] {
        let mut r = row("turn", "x");
        r.insert("id".to_string(), bad.clone());
        assert!(s.put(&aid("ai"), r).is_err(), "{bad} must be refused");
    }
}

#[test]
fn every_row_carries_its_agent_and_a_timestamp() {
    let s = RowStore::new();
    let id = s.put(&aid("ai"), row("turn", "hi")).unwrap();
    let got = s.get_one(id).unwrap();
    assert_eq!(got["agent"], json!("ai"));
    assert_eq!(got["id"], json!(id));
    assert!(got["ts"].is_number());
}

#[test]
fn rows_are_per_agent_in_append_order_and_get_keeps_the_order_asked() {
    let s = RowStore::new();
    let a1 = s.put(&aid("a"), row("turn", "a1")).unwrap();
    let b1 = s.put(&aid("b"), row("turn", "b1")).unwrap();
    let a2 = s.put(&aid("a"), row("turn", "a2")).unwrap();
    let mine: Vec<i64> = s
        .for_agent(&aid("a"))
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(mine, vec![a1, a2]);
    let asked: Vec<i64> = s
        .get(&[a2, b1, 999])
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(asked, vec![a2, b1], "order as asked, missing ids skipped");
}

#[test]
fn a_body_pages_by_its_parents_pid() {
    let s = RowStore::new();
    let pid = s.mint_id();
    s.body_put(&aid("ai"), pid, "hello world").unwrap();
    assert_eq!(
        s.body_get(pid, 0, 5),
        Some(("hello".to_string(), 5, false, 11))
    );
    assert_eq!(
        s.body_get(pid, 5, 100),
        Some((" world".to_string(), 11, true, 11))
    );
    assert_eq!(s.body_get(404, 0, 5), None);
}

#[test]
fn a_body_pages_by_characters_not_bytes() {
    let s = RowStore::new();
    let pid = s.mint_id();
    s.body_put(&aid("ai"), pid, "héllo").unwrap();
    let (chunk, next, eof, size) = s.body_get(pid, 0, 2).unwrap();
    assert_eq!(chunk, "hé");
    assert_eq!((next, eof, size), (2, false, 5));
}

#[test]
fn the_newest_view_is_current_and_strips_its_own_bookkeeping() {
    let s = RowStore::new();
    let first = json!({"ids": [1, 2]}).as_object().unwrap().clone();
    s.view_put(&aid("ai"), first).unwrap();
    let second = json!({"ids": [3]}).as_object().unwrap().clone();
    s.view_put(&aid("ai"), second).unwrap();
    let v = s.view_get(&aid("ai")).unwrap();
    assert_eq!(v["ids"], json!([3]));
    for k in ["kind", "agent", "id", "parent"] {
        assert!(!v.contains_key(k), "{k} leaked into the view");
    }
    assert!(s.view_get(&aid("nobody")).is_none());
}

#[test]
fn a_stamp_edits_the_row_and_does_not_re_notify() {
    let s = RowStore::new();
    let id = s.put(&aid("ai"), row("call", "")).unwrap();
    let mut tail = s.subscribe(id); // opens AFTER the row, so replay is empty
    s.stamp(id, json!({"status": "ok"}).as_object().unwrap());
    assert_eq!(s.get_one(id).unwrap()["status"], json!("ok"));
    // Nothing was appended, so nothing is owed to the tail. A stamp that
    // notified would write the same row to the log twice.
    assert!(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(std::time::Duration::from_millis(50), tail.recv())
                    .await
                    .is_err()
            }),
        "a stamp must not reach the tail"
    );
}

#[tokio::test]
async fn the_tail_replays_above_the_cursor_then_follows_live() {
    let s = RowStore::new();
    let a = s.put(&aid("ai"), row("turn", "one")).unwrap();
    let b = s.put(&aid("ai"), row("turn", "two")).unwrap();
    let mut tail = s.subscribe(a);
    let c = s.put(&aid("ai"), row("turn", "three")).unwrap();
    assert_eq!(
        tail.recv().await,
        Some(b),
        "replay skips what is at or below"
    );
    assert_eq!(tail.recv().await, Some(c), "then the live append");
}

#[tokio::test]
async fn a_pre_minted_row_that_lands_late_still_reaches_the_tail() {
    // IDS DO NOT ARRIVE IN ORDER: a turn's id is minted at its start and its row
    // lands at its end, after the calls it made. A high-water filter drops it.
    let s = RowStore::new();
    let mut tail = s.subscribe(0);
    let turn = s.mint_id(); // 1, reserved
    let call = s.put(&aid("ai"), row("call", "")).unwrap(); // 2, lands first
    let mut late = row("turn", "the turn");
    late.insert("id".to_string(), json!(turn));
    s.put(&aid("ai"), late).unwrap();
    assert_eq!(tail.recv().await, Some(call));
    assert_eq!(tail.recv().await, Some(turn), "the late row is not dropped");
}

#[tokio::test]
async fn a_burst_before_the_first_recv_is_not_dropped() {
    // THE ANTI-`broadcast` GUARD. A fixed-capacity channel would drop the oldest
    // under a burst and rows would go missing from the durable log with no error
    // — the one loss this store exists to make impossible.
    let s = RowStore::new();
    let mut tail = s.subscribe(0);
    for i in 0..10_000 {
        s.put(&aid("ai"), row("turn", &i.to_string())).unwrap();
    }
    for want in 1..=10_000i64 {
        assert_eq!(tail.recv().await, Some(want));
    }
}

#[tokio::test]
async fn two_tails_each_see_everything_and_a_dropped_one_is_pruned() {
    let s = RowStore::new();
    let mut one = s.subscribe(0);
    let two = s.subscribe(0);
    let id = s.put(&aid("ai"), row("turn", "hi")).unwrap();
    assert_eq!(one.recv().await, Some(id));
    drop(two);
    // The dead sender goes on the next append, not on a sweep.
    let id2 = s.put(&aid("ai"), row("turn", "again")).unwrap();
    assert_eq!(one.recv().await, Some(id2));
}

#[test]
fn the_hydrator_raises_the_counter_and_never_lowers_it() {
    let s = RowStore::new();
    s.set_next_id(10);
    assert_eq!(s.next_id(), 10);
    s.set_next_id(3);
    assert_eq!(
        s.next_id(),
        10,
        "a lower hydration must not rewind the counter"
    );
    assert_eq!(s.mint_id(), 10);
}

#[test]
fn rows_outlive_their_agent() {
    // Agent id IS conversation id: a re-created id continues where it left off,
    // and a clean slate is a NEW id. The store never hears about deletion.
    let s = RowStore::new();
    s.put(&aid("ai"), row("turn", "before")).unwrap();
    assert_eq!(s.for_agent(&aid("ai")).len(), 1);
}
