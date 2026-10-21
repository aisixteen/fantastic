//! A broken envelope must have a VOICE. Mirrors the `malformed_calls` half of
//! `bundled_agents/ai/ai_core/tests/test_tool_parse.py`.
//!
//! Why the module exists at all: a call that cannot be read returns `[]` from
//! `extract_tool_calls`, which is byte-identical to a turn that emitted no call
//! — and that means "done": nothing parsed, nothing dispatched, no error came
//! back, and the run ends looking finished with the task untouched.

use super::{extract_tool_calls, malformed_calls, HINT, OPEN};

/// The advice is a PROMPT, not a diagnostic: a model reads it and retries from
/// it, and every runtime must say the same words. Pinned as LITERAL TEXT (this
/// module owns the wording; [`crate::route`] delivers it verbatim), so "tidying"
/// a sentence shows up as a failing test rather than as silent cross-runtime
/// drift. Byte-for-byte python's `_HINT`.
#[test]
fn the_hint_is_verbatim() {
    assert_eq!(
        HINT,
        "The shape: <call name=\"send\">\n\
         <target_id>agent id</target_id>\n\
         <type>verb</type>\n\
         <path>a.md</path>\n\
         <bytes>document, verbatim, to the first </bytes>\n\
         </bytes>\n\
         </call> — one child tag per argument, a list is one tag whose children \
         are all <item>, nothing is escaped."
    );
}

/// The three fields are python's dict, and the composed sentence is this
/// module's: `error` is the whole reply body so the wording has ONE owner.
#[test]
fn the_report_is_three_fields_and_one_composed_sentence() {
    let text = r#"<call name="send"><target_id>n</target_id><path>a</path><path>b</path></call>"#;
    let bad = malformed_calls(text);
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].envelope, OPEN);
    assert_eq!(
        bad[0].error,
        format!(
            "call NOT dispatched: duplicate `<path>` — a list is one tag whose \
             children are all `<item>`: `<path><item>…</item><item>…</item></path>`. \
             Nothing ran. {HINT}"
        )
    );
    // The block is what sits BETWEEN the opener and the closer, whole and
    // defanged — neither delimiter is part of it.
    assert_eq!(
        bad[0].your_block,
        "name=\"send\"><target_id>n</target_id><path>a</path><path>b</path>"
    );
}

/// PLAIN TEXT, not JSON — the model↔harness path carries none in either
/// direction, and a JSON-dumped refusal shows doubled backslashes.
#[test]
fn the_body_delivered_to_the_model_is_plain_text() {
    let bad = malformed_calls(r#"<call name="send"><type>reflect</type></call>"#);
    let body = bad[0].body();
    assert!(body.starts_with("call NOT dispatched: a send names its target"));
    assert!(body.contains("\n\nyour block:\n"));
    assert!(!body.starts_with('{'));
}

/// Two broken envelopes are two reports, in document order — a batch of calls is
/// answered as a batch, and a model that broke two of them must see both.
#[test]
fn every_broken_envelope_gets_its_own_report() {
    let text = concat!(
        r#"<call name="send"><type>reflect</type></call>"#,
        "\n",
        r#"<call name="nope"><target_id>n</target_id></call>"#
    );
    let bad = malformed_calls(text);
    assert_eq!(bad.len(), 2);
    assert!(bad[0].error.contains("a send names its target"));
    assert!(bad[1].error.contains("`nope` is not a tool"));
}

/// A turn that parsed is silent here: nothing to report, nothing to nudge.
#[test]
fn a_good_turn_reports_nothing() {
    let text = r#"<call name="send"><target_id>n</target_id><type>reflect</type></call>"#;
    assert_eq!(extract_tool_calls(text).len(), 1);
    assert!(malformed_calls(text).is_empty());
}

/// A trailing code fence after the last call is prose, not a call: the calls
/// before it dispatch, and the fence is no fault (#177).
#[test]
fn a_trailing_fence_after_the_calls_leaves_them_dispatchable() {
    let text = "<call name=\"send\"><target_id>exec</target_id><type>tail</type>\
                <text>go</text></call>\n\
                <call name=\"await\"></call>\n\
                ```\n";
    let calls = extract_tool_calls(text);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].name, "send");
    assert_eq!(calls[1].name, "await");
    assert!(
        malformed_calls(text).is_empty(),
        "{:?}",
        malformed_calls(text)
    );
}

/// A leftover native tool-call fragment (`<invoke …>`) BEFORE valid calls is not
/// an envelope: the valid calls after it still dispatch (#177).
#[test]
fn a_leading_invoke_fragment_leaves_the_valid_calls_after_it() {
    let text = "<invoke name=\"x\"></invoke>\n\
                <call name=\"send\"><target_id>exec</target_id><type>tail</type>\
                <text>go</text></call>\n\
                <call name=\"await\"></call>";
    let calls = extract_tool_calls(text);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].name, "send");
    assert_eq!(calls[1].name, "await");
    assert!(
        malformed_calls(text).is_empty(),
        "{:?}",
        malformed_calls(text)
    );
}

/// Both at once: an `<invoke …>` fragment before the calls and a fence after
/// them. Every valid call dispatches, in order, and nothing is reported (#177).
#[test]
fn a_leading_invoke_fragment_and_a_trailing_fence_together() {
    let text = "<invoke name=\"x\"></invoke>\n\
                <call name=\"send\"><target_id>exec</target_id><type>tail</type>\
                <text>go</text></call>\n\
                <call name=\"await\"></call>\n\
                ```\n";
    let calls = extract_tool_calls(text);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].name, "send");
    assert_eq!(calls[1].name, "await");
    assert!(
        malformed_calls(text).is_empty(),
        "{:?}",
        malformed_calls(text)
    );
}
