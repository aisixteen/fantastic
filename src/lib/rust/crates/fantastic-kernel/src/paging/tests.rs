//! Paging — the default must be unchanged, and the cursor must be opaque.
#![allow(clippy::ptr_arg)] // `page` hands the identity fn a `&T`, and T is String here.

use super::*;
use serde_json::json;

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

fn id_of(s: &String) -> String {
    s.clone()
}

#[test]
fn no_limit_returns_everything_and_no_cursor() {
    // The whole point: a caller that never learned about paging is unaffected.
    let (p, next) = page(ids(&["a", "b", "c"]), &json!({}), id_of);
    assert_eq!(p, ids(&["a", "b", "c"]));
    assert_eq!(next, None);
}

#[test]
fn a_limit_bounds_the_page_and_names_the_cursor() {
    let (p, next) = page(ids(&["a", "b", "c", "d"]), &json!({"limit": 2}), id_of);
    assert_eq!(p, ids(&["a", "b"]));
    // The cursor is the last item's IDENTITY, not an offset — a caller cannot
    // compute the next one by arithmetic.
    assert_eq!(next.as_deref(), Some("b"));
}

#[test]
fn the_last_page_reports_no_cursor() {
    let (p, next) = page(ids(&["a", "b"]), &json!({"limit": 2}), id_of);
    assert_eq!(p, ids(&["a", "b"]));
    assert_eq!(next, None, "a full-but-final page must not promise more");
}

#[test]
fn after_resumes_strictly_past_the_named_item() {
    let (p, next) = page(
        ids(&["a", "b", "c", "d"]),
        &json!({"after": "b", "limit": 2}),
        id_of,
    );
    assert_eq!(p, ids(&["c", "d"]));
    assert_eq!(next, None);
}

#[test]
fn walking_the_whole_list_by_cursor_visits_each_item_once() {
    let all = ids(&["a", "b", "c", "d", "e"]);
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let payload = match &cursor {
            Some(c) => json!({"limit": 2, "after": c}),
            None => json!({"limit": 2}),
        };
        let (p, next) = page(all.clone(), &payload, id_of);
        seen.extend(p);
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(seen, all);
}

#[test]
fn a_cursor_naming_something_gone_returns_empty_not_the_top() {
    // Silently restarting from the top would look like a successful walk and
    // hand the caller the same items forever.
    let (p, next) = page(ids(&["a", "b"]), &json!({"after": "deleted"}), id_of);
    assert!(p.is_empty());
    assert_eq!(next, None);
}

#[test]
fn a_nonsense_limit_means_everything() {
    for bad in [json!(0), json!(-3), json!("2"), json!(true), Value::Null] {
        let (p, next) = page(ids(&["a", "b", "c"]), &json!({ "limit": bad }), id_of);
        assert_eq!(p.len(), 3, "limit {bad:?} should not bound the page");
        assert_eq!(next, None);
    }
}

#[test]
fn an_empty_after_is_ignored() {
    let (p, _) = page(ids(&["a", "b"]), &json!({"after": ""}), id_of);
    assert_eq!(p, ids(&["a", "b"]));
}
