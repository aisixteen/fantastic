//! The execution-row table's contract — python `tests/kernel/test_memory_store.py`
//! (the tail, the id counter's block reservation) at the table level.

use super::*;
use serde_json::json;

fn row(kind: &str) -> Row {
    json!({"kind": kind}).as_object().unwrap().clone()
}

fn aid(s: &str) -> AgentId {
    AgentId::from(s)
}

#[test]
fn ids_come_from_one_counter_starting_at_one() {
    let s = RowStore::new();
    assert_eq!(s.next_id(), 1);
    assert_eq!(s.mint().0, 1);
    assert_eq!(s.mint().0, 2);
    // A put with no id takes the next one — the SAME counter, not a second.
    assert_eq!(s.put(&aid("ai"), row("call")).unwrap().0, 3);
    assert_eq!(s.next_id(), 4);
}

#[test]
fn a_pre_minted_id_is_accepted_and_anything_else_refused() {
    let s = RowStore::new();
    let reserved = s.mint().0; // 1
    let _ = s.mint(); // 2
    let mut r = row("call");
    r.insert("id".to_string(), json!(reserved));
    assert_eq!(s.put(&aid("ai"), r.clone()).unwrap().0, reserved);
    assert!(s.put(&aid("ai"), r).unwrap_err().contains("pre-minted"));
    for bad in [json!(0), json!(-1), json!(99), json!(true), json!("7")] {
        let mut r = row("call");
        r.insert("id".to_string(), bad.clone());
        assert!(s.put(&aid("ai"), r).is_err(), "{bad} must be refused");
    }
}

#[test]
fn every_row_carries_its_agent_and_a_timestamp() {
    let s = RowStore::new();
    let (id, _) = s.put(&aid("ai"), row("call")).unwrap();
    let got = s.get_one(id).unwrap();
    assert_eq!(got["agent"], json!("ai"));
    assert_eq!(got["id"], json!(id));
    assert!(got["ts"].is_number());
}

#[test]
fn a_flushed_row_leaves_ram() {
    let s = RowStore::new();
    let (a, _) = s.put(&aid("ai"), row("call")).unwrap();
    let (b, _) = s.put(&aid("ai"), row("settle")).unwrap();
    assert_eq!(s.ids(), vec![a, b]);
    assert!(s.remove(a).is_some());
    assert!(!s.contains(a));
    assert_eq!(s.ids(), vec![b]);
    // a stamp on a flushed row edits nothing
    s.stamp(a, &row("x"));
    assert!(s.get_one(a).is_none());
}

#[test]
fn the_first_mint_reserves_a_block_and_the_bound_is_reported() {
    let s = RowStore::new();
    // A never-loaded table reserves on its first mint.
    let (n, bound) = s.mint();
    assert_eq!(n, 1);
    assert_eq!(bound, Some(1 + ID_BLOCK));
    for _ in 0..(ID_BLOCK - 1) {
        assert_eq!(s.mint().1, None, "inside the block: nothing to persist");
    }
    let (n, bound) = s.mint();
    assert_eq!(n, 1 + ID_BLOCK);
    assert_eq!(bound, Some(1 + 2 * ID_BLOCK), "a spent block reserves the next");
}

#[test]
fn restore_resumes_at_the_persisted_bound_and_never_lowers() {
    let s = RowStore::new();
    s.restore_counter(Some(5000));
    assert_eq!(s.next_id(), 5000);
    assert_eq!(s.id_reserved(), 5000 + ID_BLOCK);
    s.restore_counter(Some(10));
    assert_eq!(s.next_id(), 5000, "never lowered");
    let t = RowStore::new();
    t.restore_counter(None);
    assert_eq!(t.id_reserved(), 1 + ID_BLOCK, "no bound: the first block in memory");
}

#[tokio::test]
async fn the_tail_replays_above_the_cursor_then_follows_live() {
    let s = RowStore::new();
    let (a, _) = s.put(&aid("probe"), row("call")).unwrap();
    let (b, _) = s.put(&aid("probe"), row("call")).unwrap();
    let mut tail = s.subscribe(a);
    let (c, _) = s.put(&aid("probe"), row("call")).unwrap();
    let (d, _) = s.put(&aid("other"), row("call")).unwrap();
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(tail.recv().await.unwrap());
    }
    assert_eq!(got, vec![b, c, d], "replay (> after) then live, once each");
}

#[tokio::test]
async fn a_dropped_tail_is_pruned() {
    let s = RowStore::new();
    let tail = s.subscribe(0);
    assert_eq!(s.tail_count(), 1);
    drop(tail);
    let _ = s.put(&aid("a"), row("call")).unwrap();
    assert_eq!(s.tail_count(), 0, "a consumer that stopped is dropped");
}
