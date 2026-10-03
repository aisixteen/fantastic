//! The secret mark's pure half — every text and shape pinned against python
//! `kernel/_secret.py` (measured with the python kernel, 2026-10-01; the
//! array-of-steps marks, 2026-10-03).

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
            "send: secret path must be a dotted string or an array of steps, got 1",
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
        assert_eq!(s.paths, vec![secret[0].clone()], "{secret}");
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
    // `n` echoes the marked `payload.n` (5) as a number: its text is a secret
    // value, so it is the placeholder too (audit S7)
    assert_eq!(
        shown.result,
        json!({"error": "rejected <secret> <secret>", "tok": "<secret>", "n": "<secret>"})
    );
    assert_eq!(shown.status, Status::Failed);
    assert_eq!(shown.pid, Some(TaskId(7)));
}

#[test]
fn a_marked_request_body_is_hidden_and_scrubbed_as_its_bytes_repr() {
    let body: &[u8] = b"sk-'x\n\xff";
    let it = Item::new("v", json!({"type": "w"}))
        .with_binary_key("bytes", body.to_vec())
        .with_secret(json!(["payload.bytes"]));
    let s = check(&it).unwrap().unwrap();
    // python `str(b"sk-'x\n\xff")`
    assert_eq!(s.values, vec!["b\"sk-'x\\n\\xff\"".to_string()]);
    let shown = shown_payload(&it.payload, it.binary.as_ref(), &s);
    assert!(shown.body_hidden);
    assert_eq!(shown.payload, json!({"type": "w", "bytes": "<secret>"}));
    // nothing lies below the body
    let deeper = Item::new("v", json!({"type": "w"}))
        .with_binary_key("bytes", body.to_vec())
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
    .with_body(Some(crate::outcome::Blob::key("bytes", b"abc".to_vec())));
    let shown = shown_outcome(&out, &s);
    assert_eq!(shown.result, json!("<secret>"));
    assert!(shown.body.is_none());
    let s = check(&item(json!(["result.bytes"]))).unwrap().unwrap();
    let shown = shown_outcome(&out, &s);
    assert_eq!(shown.result, json!({"bytes": "<secret>", "n": "q"}));
    assert!(shown.body.is_none());
}

#[test]
fn scrub_replaces_the_longest_value_first_and_a_key_number_or_bool_equal_to_one() {
    // audit S7 (owner-approved 2026-10-03): a KEY, a number or a bool whose
    // (python) text EQUALS a secret value is the placeholder; a key that only
    // contains one is structure, kept; null holds no secret text.
    let got = scrub(
        &json!({"a": "xxabcxx", "k": ["abc"], "abc": "ab", "xabcx": 1, "n": 123,
                "m": 1234, "f": 1.5, "t": true, "u": false, "z": null}),
        &[
            "ab".to_string(),
            "abc".to_string(),
            "123".to_string(),
            "1.5".to_string(),
            "True".to_string(),
        ],
    );
    assert_eq!(
        got,
        json!({"a": "xx<secret>xx", "k": ["<secret>"], "<secret>": "<secret>",
               "xabcx": 1, "n": "<secret>", "m": 1234, "f": "<secret>",
               "t": "<secret>", "u": false, "z": null})
    );
}

#[test]
fn a_short_secret_never_mangles_a_payload_key() {
    // the verb's TEXT is scrubbed as before; the `type` KEY survives
    let p = json!({"type": "set_key", "key": "y"});
    let s = check(&Item::new("vault", p.clone()).with_secret(json!(["payload.key"])))
        .unwrap()
        .unwrap();
    assert_eq!(
        shown_payload(&p, None, &s).payload,
        json!({"type": "set_ke<secret>", "key": "<secret>"})
    );
}

#[test]
fn a_mark_on_a_create_or_update_agent_field_is_refused_like_python() {
    // audit S2 (owner-approved 2026-10-03): those fields become the agent's record.
    let why = "a record field cannot be secret — use a credential_store ref";
    let create = json!({"type": "create_agent", "parent_id": "kernel_state",
                        "handler_module": "p.tools", "id": "leaky",
                        "env": {"TOKEN": "sk-1"}});
    for (payload, secret, text) in [
        (
            create.clone(),
            json!(["payload.env.TOKEN"]),
            format!(
                "send: secret path 'payload.env.TOKEN' names a field create_agent \
                 writes into the agent record: {why}"
            ),
        ),
        (
            create.clone(),
            json!([["payload", "env"]]),
            format!(
                "send: secret path ['payload', 'env'] names a field create_agent \
                 writes into the agent record: {why}"
            ),
        ),
        (
            json!({"type": "update_agent", "id": "vault", "note": "sk-1"}),
            json!(["payload.note"]),
            format!(
                "send: secret path 'payload.note' names a field update_agent writes \
                 into the agent record: {why}"
            ),
        ),
        (
            // `payload.type` is judged first, as everywhere
            json!({"type": "update_agent", "id": "vault"}),
            json!(["payload.type"]),
            "send: secret cannot hide payload.type — the verb is how every row is read".to_string(),
        ),
    ] {
        let r = check(&Item::new("kernel", payload).with_secret(secret.clone()))
            .expect_err("a record-field mark is refused");
        assert_eq!(
            r,
            json!({"error": text, "reason": "bad_arg", "arg": "secret"}),
            "{secret}"
        );
    }
    // a RESULT mark on create_agent is unchanged
    assert!(
        check(&Item::new("kernel", create).with_secret(json!(["result"])))
            .unwrap()
            .is_some()
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

// ─── a mark as an ARRAY OF STEPS (owner, 2026-10-03) ─────────────────────────

#[test]
fn every_malformed_array_mark_is_refused_with_pythons_text() {
    let step = "is neither a string (an object key) nor a non-negative integer (a list index)";
    let root = "must start with 'payload' and a step under it, or with 'result'";
    for (secret, text) in [
        (json!([[]]), "path [] has no steps".to_string()),
        (
            json!([["payload", true]]),
            format!("path ['payload', True] step True {step}"),
        ),
        (
            json!([["payload", 1.5]]),
            format!("path ['payload', 1.5] step 1.5 {step}"),
        ),
        (
            json!([["payload", null]]),
            format!("path ['payload', None] step None {step}"),
        ),
        (
            json!([["payload", -1]]),
            format!("path ['payload', -1] step -1 {step}"),
        ),
        (
            json!([["payload", ["a"]]]),
            format!("path ['payload', ['a']] step ['a'] {step}"),
        ),
        (
            json!([["payload", {"a": 1}]]),
            format!("path ['payload', {{'a': 1}}] step {{'a': 1}} {step}"),
        ),
        // the first bad step is named, before the first-step rule
        (
            json!([[true, "payload"]]),
            format!("path [True, 'payload'] step True {step}"),
        ),
        (json!([["key"]]), format!("path ['key'] {root}")),
        (json!([[0, "key"]]), format!("path [0, 'key'] {root}")),
        (json!([["payload"]]), format!("path ['payload'] {root}")),
        (
            json!([["payload", "type"]]),
            "cannot hide payload.type — the verb is how every row is read".to_string(),
        ),
        (
            json!([["payload", "nope"]]),
            "path ['payload', 'nope'] names nothing in the payload".to_string(),
        ),
        (
            json!([["payload", "list", "1"]]),
            "path ['payload', 'list', '1'] names nothing in the payload".to_string(),
        ),
        (
            json!([["payload", "list", 2]]),
            "path ['payload', 'list', 2] names nothing in the payload".to_string(),
        ),
        (
            json!(["payload.key", ["payload", "nope"]]),
            "path ['payload', 'nope'] names nothing in the payload".to_string(),
        ),
        (
            json!([{"payload": "key"}]),
            "path must be a dotted string or an array of steps, got {'payload': 'key'}".to_string(),
        ),
        (
            json!([null]),
            "path must be a dotted string or an array of steps, got None".to_string(),
        ),
        (
            json!([true]),
            "path must be a dotted string or an array of steps, got True".to_string(),
        ),
    ] {
        let r = refused(secret.clone());
        assert_eq!(
            r,
            json!({"error": format!("send: secret {text}"), "reason": "bad_arg", "arg": "secret"}),
            "{secret}"
        );
    }
}

#[test]
fn array_steps_are_exact_and_the_marks_are_kept_as_given() {
    let p = json!({"type": "t", "a.b": "lit", "a": {"b": "nested"},
                   "items": ["i0", "i1"], "m": {"0": "m0"}});
    let it = |secret: Value| Item::new("v", p.clone()).with_secret(secret);
    for (secret, values, shown) in [
        (
            json!([["payload", "a.b"]]),
            vec!["lit"],
            json!({"type": "t", "a.b": "<secret>", "a": {"b": "nested"},
                   "items": ["i0", "i1"], "m": {"0": "m0"}}),
        ),
        (
            json!(["payload.a.b"]),
            vec!["nested"],
            json!({"type": "t", "a.b": "lit", "a": {"b": "<secret>"},
                   "items": ["i0", "i1"], "m": {"0": "m0"}}),
        ),
        (
            json!([["payload", "items", 1]]),
            vec!["i1"],
            json!({"type": "t", "a.b": "lit", "a": {"b": "nested"},
                   "items": ["i0", "<secret>"], "m": {"0": "m0"}}),
        ),
        (
            json!([["payload", "m", "0"]]),
            vec!["m0"],
            json!({"type": "t", "a.b": "lit", "a": {"b": "nested"},
                   "items": ["i0", "i1"], "m": {"0": "<secret>"}}),
        ),
        (
            json!(["payload.items.0"]),
            vec!["i0"],
            json!({"type": "t", "a.b": "lit", "a": {"b": "nested"},
                   "items": ["<secret>", "i1"], "m": {"0": "m0"}}),
        ),
    ] {
        let item = it(secret.clone());
        let s = check(&item).unwrap().unwrap();
        assert_eq!(s.values, values, "{secret}");
        assert_eq!(Value::Array(s.paths.clone()), secret, "{secret}");
        assert_eq!(shown_payload(&p, None, &s).payload, shown, "{secret}");
    }
    // a string step is never an index, an int step never a key
    for secret in [
        json!([["payload", "items", "0"]]),
        json!([["payload", "m", 0]]),
    ] {
        assert!(check(&it(secret.clone())).is_err(), "{secret}");
    }
    // a mixed list keeps every mark as given, in order
    let mixed = json!([
        "payload.a.b",
        ["payload", "a.b"],
        ["result", "x", 0],
        "result.y"
    ]);
    let s = check(&it(mixed.clone())).unwrap().unwrap();
    assert_eq!(Value::Array(s.paths.clone()), mixed);
    assert_eq!(s.values, vec!["nested", "lit"]);
}

#[test]
fn an_array_result_mark_masks_exactly_what_it_names() {
    let reply = json!({"a.b": "lit", "a": {"b": "nested"}, "items": ["x", "tok"]});
    let out = Outcome::new(Some(TaskId(3)), Status::Completed, reply);
    for (secret, shown) in [
        (
            json!([["result", "a.b"], ["result", "items", 1]]),
            json!({"a.b": "<secret>", "a": {"b": "nested"}, "items": ["x", "<secret>"]}),
        ),
        (
            json!(["result.a.b"]),
            json!({"a.b": "lit", "a": {"b": "<secret>"}, "items": ["x", "tok"]}),
        ),
        // a result mark that names nothing masks nothing
        (
            json!([["result", "items", "1"]]),
            json!({"a.b": "lit", "a": {"b": "nested"}, "items": ["x", "tok"]}),
        ),
        (json!([["result"]]), json!("<secret>")),
    ] {
        let s = check(&item(secret.clone())).unwrap().unwrap();
        assert_eq!(shown_outcome(&out, &s).result, shown, "{secret}");
    }
}

#[test]
fn an_array_mark_names_a_request_body_by_its_exact_steps() {
    let body: &[u8] = b"raw-key";
    let at = |secret: Value| {
        Item::new("v", json!({"type": "w", "meta": {}}))
            .with_binary(
                vec![Step::Key("meta".into()), Step::Key("a.b".into())],
                body.to_vec(),
            )
            .with_secret(secret)
    };
    let it = at(json!([["payload", "meta", "a.b"]]));
    let s = check(&it).unwrap().unwrap();
    assert_eq!(s.values, vec!["b'raw-key'".to_string()]);
    let shown = shown_payload(&it.payload, it.binary.as_ref(), &s);
    assert!(shown.body_hidden);
    assert_eq!(
        shown.payload,
        json!({"type": "w", "meta": {"a.b": "<secret>"}})
    );
    // the dotted spelling is `meta` → `a` → `b`: it names nothing
    assert!(check(&at(json!(["payload.meta.a.b"]))).is_err());
}

/// An INTEGER step wider than 64 bits (the number keeps its digits — the
/// workspace's serde_json is `arbitrary_precision`) is python's `int`: a list
/// index that names nothing, never "neither a string nor a non-negative
/// integer". A float spelling of the same value stays refused as a float.
/// Texts measured with python `kernel/_secret.check` (2026-10-03).
#[test]
fn an_integer_step_past_u64_is_an_index_that_names_nothing() {
    let wide = |raw: &str| -> Value { serde_json::from_str(raw).expect("valid json") };
    let step = "is neither a string (an object key) nor a non-negative integer (a list index)";
    for (raw, text) in [
        (
            r#"[["payload", "list", 18446744073709551616]]"#,
            "path ['payload', 'list', 18446744073709551616] names nothing in the payload"
                .to_string(),
        ),
        (
            r#"[["payload", "list", 18446744073709551615]]"#,
            "path ['payload', 'list', 18446744073709551615] names nothing in the payload"
                .to_string(),
        ),
        (
            r#"[["payload", "ref", 18446744073709551616]]"#,
            "path ['payload', 'ref', 18446744073709551616] names nothing in the payload"
                .to_string(),
        ),
        (
            r#"[["payload", 18446744073709551616]]"#,
            "path ['payload', 18446744073709551616] names nothing in the payload".to_string(),
        ),
        (
            r#"[["payload", "list", 99999999999999999999999999999]]"#,
            "path ['payload', 'list', 99999999999999999999999999999] names nothing in the payload"
                .to_string(),
        ),
        (
            r#"[["payload", "list", -18446744073709551617]]"#,
            format!(
                "path ['payload', 'list', -18446744073709551617] step -18446744073709551617 {step}"
            ),
        ),
        (
            r#"[["payload", "list", 18446744073709551616.0]]"#,
            format!(
                "path ['payload', 'list', 1.8446744073709552e+19] step 1.8446744073709552e+19 {step}"
            ),
        ),
        (
            r#"[["payload", "list", 1e20]]"#,
            format!("path ['payload', 'list', 1e+20] step 1e+20 {step}"),
        ),
    ] {
        assert_eq!(
            refused(wide(raw)),
            json!({"error": format!("send: secret {text}"), "reason": "bad_arg", "arg": "secret"}),
            "{raw}"
        );
    }
    // a RESULT mark is not checked against anything: python keeps it, as given,
    // and it masks nothing
    for raw in [
        r#"[["result", "items", 18446744073709551616]]"#,
        r#"["payload.key", ["result", 18446744073709551616]]"#,
    ] {
        let s = check(&item(wide(raw))).expect(raw).expect(raw);
        assert_eq!(Value::Array(s.paths.clone()), wide(raw), "{raw}");
        assert_eq!(
            py_repr_value(&Value::Array(s.paths.clone())),
            py_repr_value(&wide(raw))
        );
    }
    let s = check(&item(wide(
        r#"[["result", "items", 18446744073709551616]]"#,
    )))
    .unwrap()
    .unwrap();
    assert!(s.values.is_empty());
    let reply = json!({"items": ["x", "tok"]});
    let out = Outcome::new(Some(TaskId(3)), Status::Completed, reply.clone());
    assert_eq!(shown_outcome(&out, &s).result, reply);
}

/// `-0` decodes to python's `int` 0 — a non-negative index, kept and shown as
/// `0` (python `json.loads` then `repr`).
#[test]
fn a_minus_zero_step_is_index_zero() {
    let mark: Value = serde_json::from_str(r#"[["payload", "list", -0]]"#).unwrap();
    let s = check(&item(mark.clone())).expect("-0 is an index").unwrap();
    assert_eq!(s.values, vec!["x1".to_string()]);
    assert_eq!(
        py_repr_value(&Value::Array(s.paths.clone())),
        "[['payload', 'list', 0]]"
    );
}
