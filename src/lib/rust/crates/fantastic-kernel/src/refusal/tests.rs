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
    // The fix is IN the refusal — the operand/target confusion is the whole
    // reason this code exists.
    assert!(v["hint"].as_str().unwrap().contains("kernel"));
    assert!(v["hint"].as_str().unwrap().contains("OPERAND"));
}

#[test]
fn unknown_agent_carries_the_live_index() {
    let v = unknown_agent(
        "typo",
        vec!["core".into(), "cli".into(), "file_bridge_ab12".into()],
    );
    assert_eq!(v["reason"], "unknown_agent");
    assert_eq!(v["agent_count"], 3);
    // sorted, so a caller reading them gets a stable list
    assert_eq!(v["agents"][0], "cli");
    assert_eq!(v["agents"][1], "core");
    assert!(
        v.get("hint").is_none(),
        "a real id needs no placeholder hint"
    );
}

#[test]
fn a_placeholder_id_is_named_as_one() {
    // Measured 2026-08-04: a model copied `<id>` out of an example and received
    // the same bare refusal 34 times without correcting.
    let v = unknown_agent("<id>", vec!["core".into()]);
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
