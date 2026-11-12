//! The one refusal shape — the CLOSED code set and the facts each carries.

use super::*;

#[test]
fn every_refusal_carries_both_fields() {
    let v = refusal(MISSING_ARG, "create_agent: parent_id required");
    assert_eq!(v["error"], "create_agent: parent_id required");
    assert_eq!(v["reason"], "missing_arg");
    // Nothing else unless asked for — `hint`/`see` are opt-in.
    assert_eq!(v.as_object().unwrap().len(), 2);
}

#[test]
#[should_panic(expected = "unknown refusal code")]
fn an_unknown_code_is_a_programming_error() {
    // The point of a closed set is that it cannot quietly grow at a call site.
    let _ = refusal("made_up_code", "nope");
}

#[test]
fn missing_and_bad_name_the_argument() {
    let m = missing("create_agent", "parent_id");
    assert_eq!(m["reason"], "missing_arg");
    assert_eq!(m["arg"], "parent_id");
    assert_eq!(m["error"], "create_agent: parent_id required");

    let b = bad("send", "timeout", "a positive number");
    assert_eq!(b["reason"], "bad_arg");
    assert_eq!(b["arg"], "timeout");
    assert_eq!(b["error"], "send: timeout must be a positive number");
}

#[test]
fn not_here_points_at_the_root() {
    let v = not_here("create_agent", "some_child");
    assert_eq!(v["reason"], "not_here");
    // PYTHON'S WORDS. One code must not mean two sentences across runtimes.
    assert_eq!(
        v["error"],
        "create_agent is a kernel verb — some_child does not answer it"
    );
    // The fix is IN the refusal — the operand/target confusion is the whole
    // reason this code exists.
    assert!(v["hint"].as_str().unwrap().contains("kernel"));
    assert!(v["hint"].as_str().unwrap().contains("`parent_id` / `id`"));
    // The machine field: which verb was misaddressed, without parsing prose.
    assert_eq!(v["verb"], "create_agent");
}

#[test]
fn unknown_agent_carries_the_live_index() {
    let v = unknown_agent(
        "typo",
        vec![
            "kernel_state".into(),
            "cli".into(),
            "file_bridge_ab12".into(),
        ],
    );
    assert_eq!(v["reason"], "unknown_agent");
    assert_eq!(v["agent_count"], 3);
    // sorted, so a caller reading them gets a stable list
    assert_eq!(v["agents"][0], "cli");
    assert_eq!(v["agents"][1], "file_bridge_ab12");
    assert_eq!(v["agents"][2], "kernel_state");
    assert!(
        v.get("hint").is_none(),
        "a real id needs no placeholder hint"
    );
}

#[test]
fn a_placeholder_id_is_named_as_one() {
    // A model that copies `<id>` out of an example and receives the same bare
    // refusal repeats it without correcting.
    let v = unknown_agent("<id>", vec!["kernel_state".into()]);
    let hint = v["hint"].as_str().unwrap();
    assert!(hint.contains("PLACEHOLDER"));
    assert!(hint.contains("agents"));
}

#[test]
fn the_id_index_is_bounded() {
    let ids: Vec<String> = (0..200).map(|i| format!("agent_{i:03}")).collect();
    let v = unknown_agent("nope", ids);
    // The count is honest even though the list is clipped — one typo must not
    // turn into a context dump.
    assert_eq!(v["agent_count"], 200);
    assert_eq!(v["agents"].as_array().unwrap().len(), UNKNOWN_AGENT_IDS_MAX);
}

#[test]
fn extra_facts_ride_at_the_top_level() {
    let mut v = refusal(UNAUTHORIZED, "a may not call \"b\" on c");
    with(&mut v, "side", serde_json::json!("egress"));
    // A caller reads `side` beside `reason`, never nested.
    assert_eq!(v["side"], "egress");
    assert_eq!(v["reason"], "unauthorized");
}

// ─── python repr, not rust Debug ───────────────────────────────

#[test]
fn py_repr_matches_python() {
    // The kernel's refusal texts are python's, byte for byte: python's `!r`
    // prefers SINGLE quotes and rust's `{:?}` always uses double ones, so the
    // two runtimes said different sentences on the same refusal.
    assert_eq!(py_repr("ghost"), "'ghost'");
    // Double quotes only when the text carries a `'` and no `"`.
    assert_eq!(py_repr("it's"), "\"it's\"");
    assert_eq!(py_repr("both'\""), "'both\\'\"'");
    // `\t`/`\r`/`\n` escape; `é` stays raw (rust Debug would emit `\u{e9}`).
    assert_eq!(py_repr("a\tb\r\n"), "'a\\tb\\r\\n'");
    assert_eq!(py_repr("café"), "'café'");
}

#[test]
fn unknown_agent_error_is_python_repr() {
    // `no agent 'ghost'` (python's repr quoting) — never Debug's `no agent
    // "ghost"`.
    let v = unknown_agent("ghost", vec!["kernel_state".into()]);
    assert_eq!(v["error"], "no agent 'ghost'");
    // Same helper behind the placeholder hint.
    let p = unknown_agent("<id>", vec!["kernel_state".into()]);
    assert_eq!(
        p["hint"],
        "'<id>' is a PLACEHOLDER from an example, not an id — substitute one of `agents`."
    );
}

#[test]
fn the_declared_id_pattern_refuses_the_unsafe_and_accepts_colons() {
    use crate::verbs::check_agent_id;
    let refused = |id: &str| check_agent_id("create_agent", "id", id).is_err();
    // An all-digit id reads as a MESSAGE ID in prompt text; names are words.
    assert!(refused("123"));
    // …but a digit-LED name is not all digits.
    assert!(!refused("1a"));
    // `:` is legal.
    assert!(!refused("ai:7f"));
    // separators, traversal, hidden, foreign `guid|agent`, empty
    for bad in ["../x", "a/b", "a\\b", "a..b", ".hidden", "a|b", ""] {
        assert!(refused(bad), "{bad:?}");
    }
    assert!(!refused("kernel_notes"));
    // the refusal names the verb and the argument
    let v = check_agent_id("create_agent", "id", "../x").unwrap_err();
    assert_eq!(v["reason"], "bad_arg");
    assert_eq!(v["arg"], "id");
}

/// Every builder's KEY ORDER, as text —
/// `preserve_order` makes it part of the wire, and `Value` equality ignores it.
#[test]
fn every_builder_serializes_in_pythons_key_order() {
    let s = |v: Value| serde_json::to_string(&v).unwrap();
    assert_eq!(
        s(refusal(BAD_ARG, "x: y")),
        r#"{"error":"x: y","reason":"bad_arg"}"#
    );
    assert_eq!(
        s(refusal_hint(NOT_FOUND, "x: y", "do z")),
        r#"{"error":"x: y","reason":"not_found","hint":"do z"}"#
    );
    assert_eq!(
        s(missing("create_agent", "parent_id")),
        r#"{"error":"create_agent: parent_id required","reason":"missing_arg","arg":"parent_id"}"#
    );
    assert_eq!(
        s(bad("send", "timeout", "a positive number")),
        r#"{"error":"send: timeout must be a positive number","reason":"bad_arg","arg":"timeout"}"#
    );
    let mut v = refusal_hint(BAD_ARG, "e", "h");
    with(&mut v, "arg", Value::String("a".into()));
    assert_eq!(
        s(v),
        r#"{"error":"e","reason":"bad_arg","hint":"h","arg":"a"}"#
    );
    assert_eq!(
        s(unknown_agent("ghost", vec!["b".into(), "a".into()])),
        r#"{"error":"no agent 'ghost'","reason":"unknown_agent","agent_count":2,"agents":["a","b"]}"#
    );
    assert_eq!(
        s(unknown_agent("<id>", vec![])),
        r#"{"error":"no agent '<id>'","reason":"unknown_agent","agent_count":0,"agents":[],"hint":"'<id>' is a PLACEHOLDER from an example, not an id — substitute one of `agents`."}"#
    );
}

#[test]
fn new_bundle_codes_are_in_closed_set() {
    for code in [
        LIMIT,
        NOT_ALLOWED,
        GRANT_FAILED,
        UNKNOWN_TIER,
        BAD_TIER,
        NO_ROOT,
        NOT_SPAWNED,
        DISABLED,
        STORE_REFUSED,
        ALREADY_HELD,
        NOT_HELD,
        NOT_IN_SUBTREE,
    ] {
        assert!(CODES.contains(&code), "{code}");
        assert_eq!(
            refusal(code, "x"),
            serde_json::json!({"error": "x", "reason": code})
        );
    }
    assert_eq!(CODES.len(), 28);
}

#[test]
fn error_text_reads_the_error_and_appends_the_message() {
    assert_eq!(error_text(&serde_json::json!({"ok": true})), None);
    assert_eq!(
        error_text(&serde_json::json!({"error": "boom"})).as_deref(),
        Some("boom")
    );
    assert_eq!(
        error_text(&serde_json::json!({"error": "boom", "message": "why"})).as_deref(),
        Some("boom: why")
    );
    assert_eq!(
        error_text(&serde_json::json!({"error": 7})).as_deref(),
        Some("7")
    );
}
