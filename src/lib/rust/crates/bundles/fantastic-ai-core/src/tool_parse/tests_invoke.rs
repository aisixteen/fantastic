//! Claude's own `<invoke name="send"><parameter …>` envelope. The python suite
//! covers it through the shared scan and malformed cases only
//! (`test_three_envelopes_interleave_in_document_order`,
//! `test_every_envelope_gets_the_same_voice`); the grammar's own rungs are pinned
//! here, because the anthropic backends are the exposure and refusing this shape
//! dropped whole runs at pass 1 on claude-haiku-4.5 — after which the model, never
//! receiving a reply, invented the entire remaining session.

use super::invoke_envelope::parse_invoke;
use super::{extract_tool_calls, malformed_calls};
use serde_json::json;

/// The body a scanner hands the parser: everything after `<invoke`, up to but not
/// including `</invoke>`.
fn body(inner: &str) -> String {
    format!(r#" name="send">{inner}"#)
}

#[test]
fn the_claude_shape_carries_the_whole_argument_object_in_one_parameter() {
    // The shape observed live: the arguments are perfectly correct JSON and only the
    // wrapper differs from the taught envelope.
    let c = extract_tool_calls(concat!(
        r#"<invoke name="send"><parameter name="arguments">"#,
        r#"{"target_id":"notes","payload":{"type":"set","key":"k"},"reply_to":"self"}"#,
        r#"</parameter></invoke>"#
    ))
    .pop()
    .expect("the claude-shaped call");
    assert_eq!(c.name, "send");
    assert_eq!(
        c.arguments_value(),
        json!({
            "target_id": "notes",
            "payload": {"type": "set", "key": "k"},
            "reply_to": "self",
        })
    );
}

#[test]
fn split_out_parameters_are_arguments_too() {
    // Models split them either way. Each value is JSON-decoded so objects and numbers
    // survive; anything that is not JSON stays the text the model typed — `self` is
    // not a JSON document, and turning it into nothing would lose the routing.
    let c = parse_invoke(&body(concat!(
        r#"<parameter name="target_id">notes</parameter>"#,
        r#"<parameter name="payload">{"type":"set","n":2}</parameter>"#,
        r#"<parameter name="reply_to">self</parameter>"#
    )))
    .expect("split parameters");
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "notes", "payload": {"type": "set", "n": 2}, "reply_to": "self"})
    );
}

#[test]
fn a_split_out_parameter_wins_over_the_blob() {
    // The blob lands first and the named parameters are applied over it, so a model
    // that wrote both meant the explicit one.
    let c = parse_invoke(&body(concat!(
        r#"<parameter name="arguments">{"target_id":"old","payload":{"type":"x"}}</parameter>"#,
        r#"<parameter name="target_id">new</parameter>"#
    )))
    .expect("blob plus override");
    assert_eq!(c.arguments["target_id"], "new");
    assert_eq!(c.arguments["payload"], json!({"type": "x"}));
}

#[test]
fn the_value_may_span_lines_and_is_trimmed() {
    // `re.DOTALL` on python's pattern: a program body written across lines is one
    // value. The surrounding whitespace of a pretty-printed block is not part of it.
    let c = parse_invoke(&body(
        "<parameter name=\"arguments\">\n  {\"target_id\":\"m\",\n   \"payload\":{\"type\":\"one\"}}\n</parameter>",
    ))
    .expect("multiline blob");
    assert_eq!(
        c.arguments_value(),
        json!({"target_id": "m", "payload": {"type": "one"}})
    );
}

#[test]
fn a_missing_name_attribute_means_send() {
    // `send` is the only tool there is, so an absent or empty attribute names it
    // rather than failing the parse.
    for head in ["", r#" name=""#.to_string().as_str()] {
        let block = format!(r#"{head}><parameter name="target_id">m</parameter>"#);
        let c = parse_invoke(&block).expect("defaulted name");
        assert_eq!(c.name, "send", "{block}");
    }
}

#[test]
fn an_invoke_with_no_parameters_is_not_a_call() {
    // Nothing to send and nothing to say. It is also not call-SHAPED, so the scanner
    // treats it as a mention rather than reporting it — an `<invoke` written inside a
    // sentence must not earn an error.
    assert!(parse_invoke(r#" name="send"></invoke>"#).is_none());
    assert!(parse_invoke(" name=\"send\">no parameters here").is_none());
    assert!(malformed_calls(r#"You wrap it in an <invoke> tag, like so."#).is_empty());
}

#[test]
fn an_unreadable_blob_with_no_other_parameter_is_not_a_call() {
    // The blob is the only argument source and it did not decode, so there are no
    // arguments — but the body IS call-shaped (`<parameter` is present), so the
    // scanner keeps it and `malformed_calls` gives the model its voice.
    let text = r#"<invoke name="send"><parameter name="arguments">{oops</parameter></invoke>"#;
    assert!(extract_tool_calls(text).is_empty());
    let bad = malformed_calls(text);
    assert_eq!(bad.len(), 1);
    assert!(bad[0].error.contains("NOT dispatched"), "{}", bad[0].error);
}

#[test]
fn a_truncated_opening_tag_has_no_arguments_to_read() {
    // No `>` at all: python's `partition` leaves the whole block as the tag head and
    // an EMPTY body, so there are no parameters and no call. Mirrored, rather than
    // reaching into the head for something that looks like an argument.
    assert!(parse_invoke(r#" name="send""#).is_none());
}
