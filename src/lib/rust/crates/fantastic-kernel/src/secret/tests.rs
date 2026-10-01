//! The secret mark's pure half — every text and shape pinned against python
//! `kernel/_secret.py` (measured with the python kernel, 2026-10-01).

use super::*;
use crate::outcome::Status;
use crate::tasks::TaskId;

fn payload() -> Value {
    json!({"type": "set_key", "ref": "openai", "key": "sk-1",
           "list": [{"a": "x1"}, "y2"], "n": 5})
}

fn item(secret: Value) -> Item {
    Item::new("vault", payload()).with_secret(secret)
}

fn refused(secret: Value) -> Value {
    check(&item(secret)).expect_err("a malformed mark is refused")
}

#[test]
fn an_absent_or_null_mark_is_nothing_secret() {
    assert!(check(&Item::new("vault", payload())).unwrap().is_none());
    assert!(check(&item(Value::Null)).unwrap().is_none());
}

#[test]
fn every_malformed_mark_is_refused_with_pythons_text() {
    for (secret, text) in [
        (
            json!("payload.key"),
            "send: secret must be a non-empty list of paths, got 'payload.key'",
        ),
        (
            json!([]),
            "send: secret must be a non-empty list of paths, got []",
        ),
        (
            json!({"a": 1}),
            "send: secret must be a non-empty list of paths, got {'a': 1}",
        ),
        (
            json!(["key"]),
            "send: secret path 'key' must start with 'payload.' or be 'result[.…]'",
        ),
        (
            json!(["payload"]),
            "send: secret path 'payload' must start with 'payload.' or be 'result[.…]'",
        ),
        (
            json!(["payload.type"]),
            "send: secret cannot hide payload.type — the verb is how every row is read",
        ),
        (
            json!(["payload.nope"]),
            "send: secret path 'payload.nope' names nothing in the payload",
        ),
        (
            json!(["payload.type.x"]),
            "send: secret path 'payload.type.x' names nothing in the payload",
        ),
        (
            json!(["payload.list.2"]),
            "send: secret path 'payload.list.2' names nothing in the payload",
        ),
        (
            json!(["payload..key"]),
            "send: secret path 'payload..key' has an empty step",
        ),
        (
            json!([1]),
            "send: secret path must be a non-empty string, got 1",
        ),
        (
            json!([""]),
            "send: secret path must be a non-empty string, got ''",
        ),
    ] {
        let r = refused(secret.clone());
        assert_eq!(
            r,
            json!({"error": text, "reason": "bad_arg", "arg": "secret"}),
            "{secret}"
        );
    }
}

#[test]
fn paths_walk_objects_and_list_indices() {
    for (secret, values) in [
        (json!(["payload.list.1"]), vec!["y2"]),
        (json!(["payload.list.01"]), vec!["y2"]),
        (json!(["payload.list.0.a"]), vec!["x1"]),
        (json!(["result.x.y"]), vec![]),
        (json!(["result"]), vec![]),
    ] {
        let s = check(&item(secret.clone())).unwrap().unwrap();
        assert_eq!(s.values, values, "{secret}");
        assert_eq!(
            s.paths,
            vec![secret[0].as_str().unwrap().to_string()],
            "{secret}"
        );
    }
}

#[test]
fn the_shown_payload_holds_placeholders_and_scrubs_every_string() {
    let s = check(&item(json!(["payload.key", "payload.n", "result.tok"])))
        .unwrap()
        .unwrap();
    let mut p = payload();
    p["note"] = json!("key is sk-1 and 5");
    let shown = shown_payload(&p, None, &s);
    assert!(!shown.body_hidden);
    assert_eq!(
        shown.payload,
        json!({"type": "set_key", "ref": "openai", "key": "<secret>",
               "list": [{"a": "x1"}, "y2"], "n": "<secret>",
               "note": "key is <secret> and <secret>"})
    );
    let list = json!({"type": "t", "list": ["a", "b"]});
    let s = check(&Item::new("v", list.clone()).with_secret(json!(["payload.list.1"])))
        .unwrap()
        .unwrap();
    assert_eq!(
        shown_payload(&list, None, &s).payload,
        json!({"type": "t", "list": ["a", "<secret>"]})
    );
}

#[test]
fn the_shown_outcome_masks_result_paths_and_scrubs_payload_and_result_values() {
    let s = check(&item(json!(["payload.key", "payload.n", "result.tok"])))
        .unwrap()
        .unwrap();
    let out = Outcome::new(
        Some(TaskId(7)),
        Status::Failed,
        json!({"error": "rejected sk-1 tok-9", "tok": "tok-9", "n": 5}),
    );
    let shown = shown_outcome(&out, &s);
    assert_eq!(
        shown.result,
        json!({"error": "rejected <secret> <secret>", "tok": "<secret>", "n": 5})
    );
    assert_eq!(shown.status, Status::Failed);
    assert_eq!(shown.pid, Some(TaskId(7)));
}

#[test]
fn a_marked_request_body_is_hidden_and_scrubbed_as_its_bytes_repr() {
    let body: &[u8] = b"sk-'x\n\xff";
    let it = Item::new("v", json!({"type": "w"}))
        .with_binary(body.to_vec())
        .with_secret(json!(["payload.bytes"]));
    let s = check(&it).unwrap().unwrap();
    // python `str(b"sk-'x\n\xff")`
    assert_eq!(s.values, vec!["b\"sk-'x\\n\\xff\"".to_string()]);
    let shown = shown_payload(&it.payload, Some(body), &s);
    assert!(shown.body_hidden);
    assert_eq!(shown.payload, json!({"type": "w", "bytes": "<secret>"}));
    // nothing lies below the body
    let deeper = Item::new("v", json!({"type": "w"}))
        .with_binary(body.to_vec())
        .with_secret(json!(["payload.bytes.x"]));
    assert!(check(&deeper).is_err());
}

#[test]
fn a_marked_whole_result_drops_the_reply_body() {
    let s = check(&item(json!(["result"]))).unwrap().unwrap();
    let out = Outcome::new(
        Some(TaskId(1)),
        Status::Completed,
        json!({"bytes": null, "n": "q"}),
    )
    .with_body(Some(b"abc".to_vec()));
    let shown = shown_outcome(&out, &s);
    assert_eq!(shown.result, json!("<secret>"));
    assert!(shown.body.is_none());
    let s = check(&item(json!(["result.bytes"]))).unwrap().unwrap();
    let shown = shown_outcome(&out, &s);
    assert_eq!(shown.result, json!({"bytes": "<secret>", "n": "q"}));
    assert!(shown.body.is_none());
}

#[test]
fn scrub_replaces_the_longest_value_first_and_leaves_keys() {
    let got = scrub(
        &json!({"a": "xxabcxx", "k": ["abc"], "abc": "ab"}),
        &["ab".to_string(), "abc".to_string()],
    );
    assert_eq!(
        got,
        json!({"a": "xx<secret>xx", "k": ["<secret>"], "abc": "<secret>"})
    );
}

#[test]
fn a_replay_refusal_names_the_execution_like_python() {
    assert_eq!(
        replay_refused(&json!("key-1")),
        json!({"error": "send: execution 'key-1' carried secrets — its record holds <secret> placeholders, not values, so it cannot be answered from the record; send again under a new id", "reason": "bad_arg", "arg": "id"})
    );
    assert_eq!(
        replay_refused(&json!(42))["error"],
        json!("send: execution 42 carried secrets — its record holds <secret> placeholders, not values, so it cannot be answered from the record; send again under a new id")
    );
}
