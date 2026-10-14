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

#[test]
fn int_ids_page_with_a_decimal_cursor() {
    // `ps`/`ledger` row identities are ints (`str(ident(item))` in
    // python's `page`); a JSON-integer `after` must resume past the item
    // whose ident STRINGIFIES to the same decimal digits — not just an
    // already-string cursor.
    let (p, next) = page(ids(&["1", "2", "3", "4"]), &json!({"after": 2}), id_of);
    assert_eq!(p, ids(&["3", "4"]));
    assert_eq!(next, None);

    // The cursor page() HANDS BACK is itself a plain string, so the two
    // representations of the same id (the caller's int, the page's string
    // cursor) are interchangeable across pages.
    let (p, next) = page(
        ids(&["1", "2", "3", "4"]),
        &json!({"after": 2, "limit": 1}),
        id_of,
    );
    assert_eq!(p, ids(&["3"]));
    assert_eq!(next.as_deref(), Some("3"));
    let (p, _) = page(
        ids(&["1", "2", "3", "4"]),
        &json!({"after": next.unwrap()}),
        id_of,
    );
    assert_eq!(p, ids(&["4"]));
}

#[test]
fn a_non_string_cursor_compares_as_python_str() {
    // Python's rules: `after=True` is IGNORED (bools are never a cursor);
    // `after=1.5`/`after=["c1"]` compare as `str(1.5)`/`str(["c1"])` and, naming
    // no item, page EMPTY rather than the whole list; `after=2.0` renders `"2.0"`
    // — NOT the same string as an item literally named `"2"`.
    let items = ids(&["1", "2", "3"]);

    let (p, _) = page(items.clone(), &json!({"after": true}), id_of);
    assert_eq!(p, items, "a bool cursor is never a cursor at all");

    let (p, next) = page(items.clone(), &json!({"after": 1.5}), id_of);
    assert!(p.is_empty(), "{p:?}");
    assert_eq!(next, None);

    let (p, _) = page(items.clone(), &json!({"after": ["c1"]}), id_of);
    assert!(p.is_empty());

    let (p, _) = page(items.clone(), &json!({"after": 2.0}), id_of);
    assert!(p.is_empty(), "'2.0' must not match the item '2': {p:?}");
}
