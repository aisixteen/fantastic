//! The taught JSON envelope: [`parse_one`]'s leniency ladder and the
//! verb-in-the-tool-name translation. Mirrors
//! `bundled_agents/ai/ai_core/tests/test_tool_parse.py`.

use super::shape::inline_send;
use super::{extract_tool_calls, parse_one};
use serde_json::json;

#[test]
fn parse_one_canonical() {
    let c = parse_one(
        r#"{"name": "send", "arguments": {"target_id": "core", "payload": {"type": "list_agents"}}}"#,
    )
    .expect("canonical call");
    assert_eq!(c.name, "send");
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "core", "payload": {"type": "list_agents"}})
    );
    assert!(c.id.starts_with("call_"), "minted id: {}", c.id);
}

#[test]
fn parse_one_flattened_and_tool_alias() {
    // tiny models drift: `tool` alias for name, flattened args
    let c = parse_one(r#"{"tool": "send", "target_id": "foo", "payload": {"type": "reflect"}}"#)
        .expect("flattened call");
    assert_eq!(c.name, "send");
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "foo", "payload": {"type": "reflect"}})
    );
}

#[test]
fn parse_one_stringified_arguments() {
    let c = parse_one(r#"{"name": "send", "arguments": "{\"target_id\": \"x\"}"}"#)
        .expect("double-encoded call");
    assert_eq!(c.arguments_value(), json!({"target_id": "x"}));
}

#[test]
fn parse_one_malformed_returns_none() {
    assert!(parse_one("{not json").is_none());
    assert!(parse_one("[1,2,3]").is_none(), "an array names no call");
}

#[test]
fn stray_top_level_keys_merge_into_the_arguments_object() {
    // Models routinely put `reply_to` one level too high. This runtime used to
    // DISCARD every top-level key once `arguments` was an object, so the reply of
    // a call the model addressed deliberately went nowhere.
    let c = parse_one(
        r#"{"name":"send","reply_to":"self","arguments":{"target_id":"m","payload":{"type":"one"}}}"#,
    )
    .expect("call with a stray");
    assert_eq!(c.arguments["reply_to"], "self");
    assert_eq!(c.arguments["target_id"], "m");
}

#[test]
fn arguments_win_over_a_stray_of_the_same_name() {
    let c = parse_one(r#"{"name":"send","target_id":"outer","arguments":{"target_id":"inner"}}"#)
        .expect("conflicting keys");
    assert_eq!(
        c.arguments["target_id"], "inner",
        "`arguments` wins on conflict"
    );
}

#[test]
fn trailing_junk_after_a_complete_object_is_recovered() {
    // Observed live from gpt-oss:120b: it closes `arguments` early and then emits
    // `,"reply_to":"self"}}` — one brace too many. A strict parse throws the whole
    // call away; the object that IS there is taken and the tail is re-read, because
    // the model's real intent is usually in it.
    let c = parse_one(
        r#"{"name":"send","arguments":{"target_id":"m","payload":{"type":"one"}},"reply_to":"self"}}"#,
    )
    .expect("recovered call");
    assert_eq!(c.arguments["target_id"], "m");
    assert_eq!(
        c.arguments["reply_to"], "self",
        "the stray tail carried the routing"
    );
}

#[test]
fn the_verb_in_the_tool_name_becomes_the_payload() {
    // VERBATIM from gemma-4-12B, probe 05 run 3, 2026-08-06. `send` is the only
    // tool, so any other `name` is verb+tool compressed — the same compression
    // `call:<agent>.<verb>` gets, in the JSON envelope. Untranslated it dispatched
    // `send("clock", None)` and raised `'NoneType' object has no attribute 'get'`.
    let c = extract_tool_calls(
        r#"<tool_call>{"name":"reflect","arguments":{"target_id":"clock"}}</tool_call>"#,
    )
    .pop()
    .expect("verb-in-name call");
    assert_eq!(c.name, "send");
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "clock", "payload": {"type": "reflect"}})
    );
}

#[test]
fn a_named_verb_keeps_its_other_arguments_in_the_payload() {
    let c = extract_tool_calls(
        r#"<tool_call>{"name":"write","arguments":{"target_id":"a","path":"f.txt","reply_to":"self"}}</tool_call>"#,
    )
    .pop()
    .expect("named verb");
    assert_eq!(
        c.arguments["payload"],
        json!({"type": "write", "path": "f.txt"})
    );
    assert_eq!(
        c.arguments["reply_to"], "self",
        "reply_to is routing, not payload"
    );
}

#[test]
fn an_explicit_payload_beats_the_tool_name() {
    let c = extract_tool_calls(
        r#"<tool_call>{"name":"reflect","arguments":{"target_id":"a","payload":{"type":"list"}}}</tool_call>"#,
    )
    .pop()
    .expect("explicit payload");
    assert_eq!(c.arguments["payload"], json!({"type": "list"}));
}

#[test]
fn an_explicit_type_among_the_args_beats_the_tool_name() {
    // THE ARGS DECIDE: the name is only consulted when the args named no verb.
    let c = extract_tool_calls(
        r#"<tool_call>{"name":"reflect","arguments":{"target_id":"a","type":"list"}}</tool_call>"#,
    )
    .pop()
    .expect("inline type");
    assert_eq!(c.arguments["payload"], json!({"type": "list"}));
}

#[test]
fn a_named_verb_without_a_target_is_left_alone() {
    // `{"name":"list_agents","arguments":{}}` names no agent. We do not guess one —
    // it earns the `no target_id` error, which teaches.
    let c = extract_tool_calls(r#"<tool_call>{"name":"list_agents","arguments":{}}</tool_call>"#)
        .pop()
        .expect("targetless call");
    assert!(!c.arguments.contains_key("target_id"));
    assert_eq!(c.name, "list_agents");
}

#[test]
fn inline_send_keeps_only_routing_at_the_top_level() {
    let args = json!({"target_id": " totals ", "type": "set", "key": "k", "reply_to": "self"});
    let c = inline_send(&json!("  totals  "), args.as_object().expect("args object"))
        .expect("inline send");
    assert_eq!(c.name, "send");
    assert_eq!(c.arguments["target_id"], "totals", "the target is trimmed");
    assert_eq!(c.arguments["payload"], json!({"type": "set", "key": "k"}));
    assert_eq!(c.arguments["reply_to"], "self");
    // No target ⇒ no call. We do not invent somewhere to send it.
    assert!(inline_send(&json!("   "), args.as_object().unwrap()).is_none());
    assert!(inline_send(&json!(42), args.as_object().unwrap()).is_none());
}
