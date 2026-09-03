//! A broken envelope must have a VOICE. Mirrors the `malformed_calls` half of
//! `bundled_agents/ai/ai_core/tests/test_tool_parse.py`.

use super::hints::{CHANNEL_HINT, JSON_HINT};
use super::{extract_tool_calls, malformed_calls, CHANNEL_OPEN, OPEN};

/// The advice is a PROMPT, not a diagnostic: a model reads it and retries from
/// it, and all four runtimes must say the same words. Pinned as LITERAL TEXT
/// (this module owns the wording; [`crate::route`] delivers it verbatim), so
/// "tidying" a sentence shows up as a failing test rather than as silent
/// cross-runtime drift.
#[test]
fn the_two_hints_are_verbatim() {
    assert_eq!(
        JSON_HINT,
        "Your program broke its own JSON envelope. Do NOT debug the escaping — \
         switch to the unbreakable FENCED form: keep the call's JSON small and \
         complete with \"@body\" where the contents go, then write the program \
         verbatim between two column-0 marker lines, still inside the same \
         <tool_call>:\n\
         {\"name\":\"send_binary\",\"arguments\":{\"target_id\":\"<id>\",\"payload\":\
         {…,\"bytes\":\"@body\"}}}\n\
         *** Begin Bytes\n\
         <the program, verbatim — real newlines, real quotes, nothing escaped>\n\
         *** End Bytes"
    );
    assert_eq!(
        CHANNEL_HINT,
        "The `<|tool_call>` body is NOT JSON: write it as `call:<agent>{key: \
         value, …}`. Strings may use ' or \" as long as the SAME character closes \
         them, and every { [ ( must be closed too."
    );
}

/// Verbatim from gemma-4-12B, probe 05, 2026-08-06 — the call that ended the run.
/// The program body is escaped correctly; the `code` value is never closed, so
/// every brace after it is string content and the object runs off the end.
const GEMMA_UNCLOSED_CODE: &str = concat!(
    r#"{"name":"send","arguments":{"target_id":"kernel_state","payload":"#,
    r#"{"type":"create_agent","parent_id":"kernel","handler_module":"exec_bridge.tools","#,
    r#""id":"reading_aggregator","code":"def update_totals():\n"#,
    r#"    worker_res = send(\"reading_worker\", {\"type\": \"compute_total\"})\n"#,
    r#"    return f\"updated to {worker_res}\"}}"#
);

#[test]
fn the_unclosed_code_string_that_ended_the_gemma_run_is_reported() {
    // The regression this exists for. Silence here read as "the model is done"
    // while the task sat untouched, so the assertion is not that we PARSE it — we
    // must not — but that the model is TOLD.
    let text = format!("{OPEN}{GEMMA_UNCLOSED_CODE}");
    assert!(
        extract_tool_calls(&text).is_empty(),
        "a broken call must never dispatch"
    );
    let bad = malformed_calls(&text);
    assert_eq!(bad.len(), 1, "{bad:#?}");
    assert!(bad[0].error.contains("NOT dispatched"));
    assert!(
        bad[0].error.contains("EOF while parsing a string"),
        "name the JSON fault: {}",
        bad[0].error
    );
    assert!(
        bad[0].error.contains("send_binary"),
        "name the RECOVERY (go raw), not more escaping: {}",
        bad[0].error
    );
    assert!(
        bad[0].your_block.contains("reading_aggregator"),
        "show its own text back"
    );
    assert_eq!(bad[0].envelope, OPEN);
}

#[test]
fn prose_with_no_envelope_is_silence_not_an_error() {
    // The distinction the whole change rests on: emitting no call is a legitimate
    // way to stop. Only an envelope that OPENED and failed is a fault.
    assert!(malformed_calls("I have finished wiring the aggregator.").is_empty());
    assert!(malformed_calls("").is_empty());
}

#[test]
fn a_good_call_beside_a_broken_one_reports_only_the_broken_one() {
    // A partially-read turn dispatched half the model's intent. Which half was lost
    // is not inferable from the replies it got, so the loss is named.
    let text = concat!(
        r#"<tool_call>{"name":"send","arguments":{"target_id":"m","payload":{"type":"one"}}}</tool_call>"#,
        r#"<tool_call>{"name":"send","arguments":{"target_id":"n","code":"oops}}"#
    );
    let targets: Vec<String> = extract_tool_calls(text)
        .into_iter()
        .map(|c| {
            c.arguments["target_id"]
                .as_str()
                .expect("target")
                .to_string()
        })
        .collect();
    assert_eq!(targets, ["m"]);
    assert_eq!(malformed_calls(text).len(), 1);
}

#[test]
fn every_envelope_gets_the_same_voice() {
    // `<|tool_call>` was the envelope whose silent drop was documented and unfixed;
    // it must report like the others now.
    for text in [
        "<|tool_call>call:a.b{unterminated<tool_call|>",
        r#"<invoke name="send"><parameter name="arguments">{oops</parameter></invoke>"#,
    ] {
        assert!(extract_tool_calls(text).is_empty(), "{text}");
        let bad = malformed_calls(text);
        assert_eq!(bad.len(), 1, "{text}");
        assert!(bad[0].error.contains("NOT dispatched"), "{text}");
    }
}

#[test]
fn prose_mentioning_the_tag_is_not_reported_as_a_broken_call() {
    // Measured 2026-08-06: gemma wrote `<tool_call>` inside a sentence and the first
    // version of this feature answered with a lecture about JSON, costing a turn. An
    // opener with no call-shaped body is a MENTION.
    let text = "You send it in a <tool_call> block. The `kernel_state` agent is root.";
    assert!(malformed_calls(text).is_empty());
}

#[test]
fn an_unterminated_but_call_shaped_block_still_reports() {
    // The other side of that rule — nemotron-3-ultra-550b omits the closing tag on
    // real calls, so a `{`-shaped body is an attempt however it ends.
    assert_eq!(
        malformed_calls(r#"<tool_call>{"name":"send","arguments":{oops"#).len(),
        1
    );
}

#[test]
fn prose_is_not_an_attempt_even_when_a_later_closer_terminates_it() {
    // A stray opener in a sentence, plus the closing tag of a REAL call further
    // down, forms a "terminated" block out of prose. Requiring the closer let that
    // through and CLOSED A LOOP: the error echoes the block, the model quotes the
    // error, the quotation contains the tag, the next scan finds a fresh opener.
    // Three of run 4's errors on 2026-08-06 were this feeding itself.
    let text = concat!(
        "My previous call was: `<tool_call>` and it failed. Retrying now.",
        r#"<tool_call>{"name":"send","arguments":{"target_id":"m","payload":{"type":"one"}}}</tool_call>"#
    );
    let targets: Vec<String> = extract_tool_calls(text)
        .into_iter()
        .map(|c| {
            c.arguments["target_id"]
                .as_str()
                .expect("target")
                .to_string()
        })
        .collect();
    assert_eq!(targets, ["m"]);
    assert!(
        malformed_calls(text).is_empty(),
        "the prose mention must not be reported"
    );
}

#[test]
fn the_echoed_block_cannot_re_arm_the_scanner() {
    // Whatever we echo becomes text the model quotes back. A live `<tool_call>` in
    // it is indistinguishable from an emitted one, so the delimiters are defanged.
    let bad = malformed_calls(r#"<tool_call>{"name":"send","note":"see <tool_call> oops"#)
        .pop()
        .expect("a report");
    let echoed = bad.your_block;
    assert!(echoed.contains("oops"), "the text must stay legible");
    assert!(
        !echoed.contains(OPEN),
        "the tag must not survive verbatim: {echoed}"
    );
    assert!(
        malformed_calls(&echoed).is_empty(),
        "echoing it must not produce a new report"
    );
}

#[test]
fn the_channel_error_does_not_lecture_about_json() {
    // Its body is not JSON. Advice that does not match the grammar the model used
    // teaches the wrong lesson — measured live on 2026-08-06.
    let bad = malformed_calls("<|tool_call>call:a.b{k: 'unclosed}<tool_call|>")
        .pop()
        .expect("a report");
    assert_eq!(bad.envelope, CHANNEL_OPEN);
    assert!(bad.error.contains("NOT JSON"));
    assert!(
        !bad.error.contains(r"\n"),
        "the JSON escaping hint must not appear here: {}",
        bad.error
    );
}

/// THE BLOCK IS SHOWN WHOLE — python's behaviour, and the reason is the fault's
/// LOCATION: an unterminated string breaks at the END of a long payload, so a
/// head-only echo shows the model text that looks perfectly fine and it re-sends
/// the identical broken envelope (measured live_build 2026-08-24). Rust and swift
/// capped at 400 chars until 2026-09-02; the cost of the cap was a stuck model,
/// the cost of the whole block is context the model can act on.
#[test]
fn malformed_your_block_is_shown_whole() {
    let text = format!(
        r#"<tool_call>{{"name":"send","arguments":{{"code":"{}"#,
        "x".repeat(40000)
    );
    let bad = malformed_calls(&text).pop().expect("a report");
    assert!(
        bad.your_block.chars().count() > 40000,
        "the tail must survive: {} chars",
        bad.your_block.chars().count()
    );
}
