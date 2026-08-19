//! The scanner: document order, unterminated acceptance, and mention-vs-attempt.
//! Mirrors `bundled_agents/ai/ai_core/tests/test_tool_parse.py`.

use super::scan::scan;
use super::{extract_tool_calls, CHANNEL_OPEN, INVOKE_OPEN, OPEN};

/// The taught envelope around a canonical `send`, for composing multi-call texts.
fn taught(target: &str, verb: &str) -> String {
    format!(
        r#"<tool_call>{{"name":"send","arguments":{{"target_id":"{target}","payload":{{"type":"{verb}"}}}}}}</tool_call>"#
    )
}

#[test]
fn extract_pulls_every_call_in_document_order() {
    let text = format!(
        "doing it {} and {}",
        taught("self", "recall"),
        taught("mem", "set")
    );
    let calls = extract_tool_calls(&text);
    let verbs: Vec<&str> = calls
        .iter()
        .map(|c| c.arguments["payload"]["type"].as_str().expect("verb"))
        .collect();
    assert_eq!(verbs, ["recall", "set"]);
    let targets: Vec<&str> = calls
        .iter()
        .map(|c| c.arguments["target_id"].as_str().expect("target"))
        .collect();
    assert_eq!(targets, ["self", "mem"]);
}

#[test]
fn a_body_that_does_not_parse_never_dispatches() {
    assert!(extract_tool_calls("<tool_call>{bad}</tool_call>").is_empty());
    assert!(
        extract_tool_calls("<tool_call>{unterminated").is_empty(),
        "call-SHAPED but unreadable: no dispatch (it is reported instead)"
    );
}

#[test]
fn an_unterminated_but_call_shaped_block_is_accepted() {
    // REWRITE of the old `extract_pulls_every_call_ignores_bad`, which asserted
    // that an unterminated envelope yields nothing — it pinned the bug. The
    // scanner used to `break` the moment the closing tag was missing.
    // nvidia/nemotron-3-ultra-550b emits a complete, valid call object and simply
    // stops without the closer; dropping it loses an unambiguous instruction, and
    // the envelope is the LAST thing in the completion so there is nothing after
    // it to misread.
    let mut calls = extract_tool_calls(
        r#"<tool_call>{"name":"send","arguments":{"target_id":"m","payload":{"type":"one"}}}"#,
    );
    assert_eq!(
        calls.len(),
        1,
        "an unterminated valid call still dispatches"
    );
    let c = calls.pop().expect("the call");
    assert_eq!(c.arguments["target_id"], "m");
}

#[test]
fn a_stray_opener_in_prose_does_not_eat_the_real_call() {
    // THE ACTIVE BUG this rewrite fixes. A `<tool_call>` mentioned in a sentence
    // used to pair with the closing tag of the GENUINE call further down, so the
    // scanner consumed both and the real instruction was never seen. The mention
    // steps over the OPENER ONLY.
    let text = format!(
        "My previous call was: `<tool_call>` and it failed. Retrying now.{}",
        taught("m", "one")
    );
    let calls = extract_tool_calls(&text);
    assert_eq!(calls.len(), 1, "the real call must survive the mention");
    assert_eq!(calls[0].arguments["target_id"], "m");
}

#[test]
fn three_envelopes_interleave_in_document_order() {
    // All three shapes coexist; `<|tool_call>` must not shadow or be shadowed by the
    // taught `<tool_call>` envelope. Each grammar's own body parses, so this asserts
    // both the scan ORDER and that no envelope is a stub.
    let text = format!(
        "{}<|tool_call>call:n.two{{}}<tool_call|>{}",
        taught("m", "one"),
        concat!(
            r#"<invoke name="send"><parameter name="arguments">"#,
            r#"{"target_id":"o","payload":{"type":"three"}}</parameter></invoke>"#
        )
    );
    let found = scan(&text);
    let openers: Vec<&str> = found.iter().map(|f| f.envelope.open).collect();
    assert_eq!(openers, [OPEN, CHANNEL_OPEN, INVOKE_OPEN]);
    assert!(found.iter().all(|f| f.terminated));
    let calls = extract_tool_calls(&text);
    let verbs: Vec<&str> = calls
        .iter()
        .map(|c| c.arguments["payload"]["type"].as_str().expect("verb"))
        .collect();
    assert_eq!(verbs, ["one", "two", "three"]);
    let targets: Vec<&str> = calls
        .iter()
        .map(|c| c.arguments["target_id"].as_str().expect("target"))
        .collect();
    assert_eq!(targets, ["m", "n", "o"]);
}

#[test]
fn scan_keeps_an_opened_envelope_whose_body_did_not_parse() {
    // The contract the whole malformed report rests on: `(envelope, block, call,
    // terminated)`, with `call: None` meaning "opened and unreadable" — which a
    // caller must be able to tell apart from "no envelope at all".
    let found = scan(r#"<tool_call>{"name":"send","code":"oops"#);
    assert_eq!(found.len(), 1);
    assert!(found[0].call.is_none(), "unreadable, not absent");
    assert!(!found[0].terminated);
    assert_eq!(found[0].envelope.open, OPEN);
    assert!(found[0].block.starts_with('{'));
    // Prose that merely names the tag opens nothing.
    assert!(scan("You send it in a <tool_call> block.").is_empty());
}
