//! The ONE call grammar: `<call name="…">` with bare child tags, args inline,
//! `bytes` verbatim, no JSON anywhere. Parse or refuse with the fault named —
//! never repair.
//!
//! Case for case the twin of python's
//! `bundled_agents/ai/ai_core/tests/test_tool_parse.py`, which is the contract.

use serde_json::{json, Map, Value};

use super::grammar::{code_masked, envelopes_in};
use super::{defang, extract_tool_calls, malformed_calls, render_call, ToolCall};

const WRITE: &str = r#"<call name="send">
<target_id>notes</target_id>
<reply_to>self</reply_to>
<title>write the report</title>
<type>write</type>
<path>report.md</path>
<bytes>PRJ-1BCA4F

# Report
- the phrase "quick brown fox", verbatim
</bytes>
</call>"#;

const PATCH: &str = r#"<call name="send">
<target_id>notes</target_id>
<type>patch</type>
<edits><item><path>hello.py</path><hunks>2</hunks></item></edits>
<bytes><<<<<<< SEARCH
    msg = "Hello, " + name
=======
    return f"Hello, {name}!"
>>>>>>> REPLACE
</bytes>
</call>"#;

fn one(text: &str) -> ToolCall {
    let calls = extract_tool_calls(text);
    assert_eq!(calls.len(), 1, "{calls:?} / {:?}", malformed_calls(text));
    calls.into_iter().next().unwrap()
}

fn payload(call: &ToolCall) -> &Map<String, Value> {
    call.arguments["payload"].as_object().unwrap()
}

// ── the taught shapes (the probe's fixtures, 12/12 on qwen3.8-27b) ───────────

#[test]
fn write_args_inline_routing_out_bytes_verbatim() {
    let c = one(WRITE);
    assert_eq!(c.name, "send");
    assert!(c.id.starts_with("call_"));
    assert_eq!(c.arguments["target_id"], json!("notes"));
    assert_eq!(c.arguments["reply_to"], json!("self"));
    assert_eq!(c.arguments["title"], json!("write the report"));
    assert_eq!(
        Value::Object(payload(&c).clone()),
        json!({
            "type": "write",
            "path": "report.md",
            "bytes": "PRJ-1BCA4F\n\n# Report\n- the phrase \"quick brown fox\", verbatim\n",
        })
    );
}

#[test]
fn patch_nested_list_and_typed_hunks() {
    let c = one(PATCH);
    let p = payload(&c);
    assert_eq!(p["type"], json!("patch"));
    assert_eq!(p["edits"], json!([{"path": "hello.py", "hunks": 2}]));
    assert!(p["bytes"]
        .as_str()
        .unwrap()
        .starts_with("<<<<<<< SEARCH\n    msg = "));
}

#[test]
fn json_looking_content_rides_untouched() {
    let text = concat!(
        r#"<call name="send"><target_id>notes</target_id><type>write</type>"#,
        "<path>c.json</path><bytes>{\"name\": \"demo\"}\n{\"tags\": [\"a\", \"b\"]}\n",
        "</bytes></call>"
    );
    let c = one(text);
    assert_eq!(
        payload(&c)["bytes"],
        json!("{\"name\": \"demo\"}\n{\"tags\": [\"a\", \"b\"]}\n")
    );
}

#[test]
fn lists_are_item_children_any_count() {
    let text = concat!(
        r#"<call name="send"><target_id>me</target_id><type>grep</type>"#,
        "<query>marker</query><ids><item>12</item><item>15</item></ids></call>"
    );
    assert_eq!(payload(&one(text))["ids"], json!([12, 15]));
    let lone = text.replace("<item>15</item>", "");
    assert_eq!(payload(&one(&lone))["ids"], json!([12]));
    let allowed = concat!(
        r#"<call name="send"><target_id>kernel</target_id><type>update_agent</type>"#,
        r"<id>worker</id><allowed><item>python3 \S+</item></allowed></call>"
    );
    assert_eq!(payload(&one(allowed))["allowed"], json!([r"python3 \S+"]));
}

#[test]
fn object_with_one_field_is_not_a_list() {
    let text = concat!(
        r#"<call name="send"><target_id>notes</target_id><type>patch</type>"#,
        "<edit><path>a.py</path></edit><bytes>x</bytes></call>"
    );
    assert_eq!(payload(&one(text))["edit"], json!({"path": "a.py"}));
}

#[test]
fn scalars_typed_by_the_one_rule() {
    let text = concat!(
        r#"<call name="send"><target_id>me</target_id><type>pull</type>"#,
        "<offset>0</offset><length>1.5</length><checkpoint>true</checkpoint>",
        "<why>keep</why><empty></empty></call>"
    );
    let c = one(text);
    assert_eq!(
        Value::Object(payload(&c).clone()),
        json!({
            "type": "pull", "offset": 0, "length": 1.5,
            "checkpoint": true, "why": "keep", "empty": "",
        })
    );
}

#[test]
fn done_and_inquiry_answer() {
    let c = one(r#"<call name="done"></call>"#);
    assert_eq!(c.name, "done");
    assert!(c.arguments.is_empty());
    let c = one(r#"<call name="inquiry_answer"><answer>yes, "both"</answer></call>"#);
    assert_eq!(c.name, "inquiry_answer");
    assert_eq!(c.arguments["answer"], json!("yes, \"both\""));
}

#[test]
fn await_is_a_control_call() {
    let c = one(r#"<call name="await"></call>"#);
    assert_eq!(c.name, "await");
    assert!(c.arguments.is_empty());
}

// ── verbatim mode ────────────────────────────────────────────────────────────

#[test]
fn a_closing_call_tag_inside_bytes_does_not_end_the_call() {
    let text = concat!(
        r#"<call name="send"><target_id>notes</target_id><type>write</type>"#,
        "<path>a.md</path><bytes>the text says </call> and goes on\n</bytes>",
        "<checkpoint>true</checkpoint></call>"
    );
    let c = one(text);
    assert_eq!(
        payload(&c)["bytes"],
        json!("the text says </call> and goes on\n")
    );
    assert_eq!(payload(&c)["checkpoint"], json!(true));
}

#[test]
fn bytes_is_never_trimmed_and_other_values_are() {
    let text = concat!(
        r#"<call name="send"><target_id>notes</target_id><type>write</type>"#,
        "<path>  a.md  </path><bytes>  two spaces \n</bytes></call>"
    );
    let c = one(text);
    assert_eq!(payload(&c)["path"], json!("a.md"));
    assert_eq!(payload(&c)["bytes"], json!("  two spaces \n"));
}

/// No `</call>`, no call: the model stopped generating
/// before it closed — nothing dispatches, nothing is "malformed"; the cut marker
/// names it.
#[test]
fn an_unterminated_call_is_not_a_call() {
    let text = r#"<call name="send"><target_id>notes</target_id><type>reflect</type>"#;
    assert!(extract_tool_calls(text).is_empty() && malformed_calls(text).is_empty());
}

/// A call INSIDE `<think>` sits inside a tag, so it is not extracted (#188: a call
/// inside any wrapper is refused); the top-level call after `</think>` is.
#[test]
fn a_call_inside_think_is_not_extracted_and_one_after_it_is() {
    let text = format!("<think>first {WRITE}</think>then\n{PATCH}\n");
    let verbs: Vec<String> = extract_tool_calls(&text)
        .iter()
        .map(|c| payload(c)["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(verbs, vec!["patch"]);
}

#[test]
fn a_mention_in_prose_is_stepped_over_not_reported() {
    let text = format!("I will emit a <call soon. {PATCH}");
    assert_eq!(extract_tool_calls(&text).len(), 1);
    assert!(malformed_calls(&text).is_empty());
    assert!(extract_tool_calls("a <caller> is not an envelope").is_empty());
}

// ── refusals: fault named, shape shown, nothing repaired ─────────────────────

fn refused(text: &str, needles: &[&str]) -> super::MalformedCall {
    assert!(extract_tool_calls(text).is_empty());
    let bad = malformed_calls(text);
    assert_eq!(bad.len(), 1, "{bad:?}");
    let err = &bad[0].error;
    for n in needles {
        assert!(err.contains(n), "{n} not in {err}");
    }
    assert!(err.starts_with("call NOT dispatched: "));
    assert!(err.contains("The shape:"));
    assert!(
        !bad[0].your_block.contains("<call"),
        "the echoed block is defanged"
    );
    bad.into_iter().next().unwrap()
}

#[test]
fn bytes_closer_inside_a_document_ends_the_value_early_and_is_refused() {
    let text = concat!(
        r#"<call name="send"><target_id>notes</target_id><type>write</type>"#,
        "<path>a.md</path><bytes>first </bytes> no\nreal end</bytes></call>"
    );
    refused(text, &["text after the last tag", "no"]);
}

#[test]
fn bytes_never_closed_is_a_cut_not_a_call() {
    // an open `<bytes>` swallows any `</call>`, so the envelope never closes
    let text =
        r#"<call name="send"><target_id>notes</target_id><type>write</type><bytes>open</call>"#;
    assert!(extract_tool_calls(text).is_empty() && malformed_calls(text).is_empty());
}

#[test]
fn element_never_closed() {
    refused(
        r#"<call name="send"><target_id>notes</target_id><type>write<path>a</path></call>"#,
        &["`<type>` opened and never closed"],
    );
}

#[test]
fn duplicate_scalar_tag() {
    refused(
        r#"<call name="send"><target_id>n</target_id><path>a</path><path>b</path></call>"#,
        &["duplicate `<path>`", "<path><item>"],
    );
}

#[test]
fn text_between_tags() {
    refused(
        r#"<call name="send"><target_id>n</target_id> stray words <type>x</type></call>"#,
        &["text outside a tag"],
    );
}

#[test]
fn send_without_target() {
    refused(
        r#"<call name="send"><type>reflect</type></call>"#,
        &["a send names its target"],
    );
}

#[test]
fn a_send_with_neither_target_nor_to_is_refused_naming_both() {
    refused(
        r#"<call name="send"><type>reflect</type></call>"#,
        &["<target_id>", "<to><item>"],
    );
}

#[test]
fn a_huge_numeric_target_is_refused() {
    // C74/D13: coerce_scalar types a numeric-shaped target_id as a NUMBER
    // (wide integers included, once arbitrary-precision lands) — python's
    // `isinstance(target, str)` then fails it exactly as a bare `<target_id>
    // 123</target_id>` does, so the send is malformed, never dispatched to an
    // agent literally named "123456789012345678901234567890".
    refused(
        r#"<call name="send"><target_id>123456789012345678901234567890</target_id><type>reflect</type></call>"#,
        &["a send names its target"],
    );
    // A small numeric-shaped target_id is refused the same way — it is a
    // Number; this pins it stays that way.
    refused(
        r#"<call name="send"><target_id>42</target_id><type>reflect</type></call>"#,
        &["a send names its target"],
    );
}

#[test]
fn head_with_extra_attribute() {
    refused(
        r#"<call name="send" id="7"><target_id>n</target_id></call>"#,
        &["the call head must be exactly `name=\"<tool>\"`"],
    );
}

#[test]
fn a_verb_in_the_tool_slot_is_refused_with_the_shape() {
    refused(
        r#"<call name="write"><target_id>n</target_id></call>"#,
        &[
            "`write` is not a tool",
            "send, done, await, inquiry_answer",
            "<type>write</type>",
        ],
    );
}

#[test]
fn defang_breaks_every_delimiter() {
    let d = defang(r#"<call name="x"><bytes>a</bytes></call>"#);
    assert!(!d.contains("<call"));
    assert!(!d.contains("</call>"));
    assert!(!d.contains("<bytes>"));
    assert!(d.contains("\u{2039}call"));
}

#[test]
fn the_broken_block_is_shown_whole() {
    let big = "x".repeat(40_000);
    let text = format!(
        concat!(
            r#"<call name="send"><target_id>n</target_id><type>write</type><bytes>{}</bytes>"#,
            "<path>a</path><path>b</path></call>"
        ),
        big
    );
    let bad = malformed_calls(&text);
    assert_eq!(bad.len(), 1);
    assert!(bad[0].your_block.contains(&big));
}

// ── the renderer is the parser's inverse ─────────────────────────────────────

#[test]
fn render_call_round_trips_the_parser_shape() {
    let args = json!({
        "target_id": "notes",
        "reply_to": "self",
        "title": "t",
        "payload": {
            "type": "patch",
            "edits": [{"path": "a.py", "hunks": 2}, {"path": "b.py", "hunks": 1}],
            "ids": [3],
            "dry_run": true,
            "bytes": "x = \"1\"\n</call>\n",
        },
    });
    let args = args.as_object().unwrap().clone();
    let text = render_call("send", &args);
    assert_eq!(one(&text).arguments, args);
    assert_eq!(one(&render_call("done", &Map::new())).arguments, Map::new());
    let answer = json!({"answer": "ok"}).as_object().unwrap().clone();
    assert_eq!(
        one(&render_call("inquiry_answer", &answer)).arguments,
        answer
    );
}

#[test]
fn grant_maps_keyed_by_ids_and_relative_tokens_are_tags() {
    let text = concat!(
        r#"<call name="send"><target_id>kernel</target_id><type>update_agent</type>"#,
        "<id>vault</id><callable_by><$self>*</$self><$parent>*</$parent>",
        "<ai-rev.2>*</ai-rev.2><notes><item>read</item><item>write</item></notes>",
        "</callable_by></call>"
    );
    let c = one(text);
    let p = payload(&c).clone();
    assert_eq!(
        p["callable_by"],
        json!({"$self": "*", "$parent": "*", "ai-rev.2": "*", "notes": ["read", "write"]})
    );
    let round = json!({"target_id": "k", "payload": Value::Object(p.clone())});
    let round = render_call("send", round.as_object().unwrap());
    assert_eq!(*payload(&one(&round)), p);
}

#[test]
fn a_send_may_name_several_recipients_with_to() {
    let text = concat!(
        r#"<call name="send"><to><item>solver_2</item><item>solver_3</item></to>"#,
        "<re>412</re><type>send</type><text>CANDIDATE 586</text></call>"
    );
    let c = one(text);
    assert_eq!(c.arguments["to"], json!(["solver_2", "solver_3"]));
    assert_eq!(c.arguments["re"], json!(412));
    assert!(!c.arguments.contains_key("target_id"));
    assert_eq!(
        c.arguments["payload"],
        json!({"type": "send", "text": "CANDIDATE 586"})
    );
    assert_eq!(
        one(&render_call("send", &c.arguments)).arguments,
        c.arguments
    );
}

#[test]
fn item_beside_other_tags_is_a_field() {
    let c = one(concat!(
        r#"<call name="send"><target_id>user</target_id><type>inquiry</type>"#,
        "<action><payload><type>buy</type><item>rocket</item></payload></action></call>"
    ));
    assert_eq!(
        payload(&c)["action"]["payload"],
        json!({"type": "buy", "item": "rocket"})
    );
}

#[test]
fn a_call_written_inside_bytes_is_document_not_envelope() {
    // A model may write another agent's error text VERBATIM into a file — text
    // that quotes `<call name="inquiry_answer">…</call>`. Python parses ONE `send`
    // with a `bytes` payload, and so must this: `<bytes>` is verbatim to its FIRST
    // closer, nothing inside it is an envelope.
    let t = "<call name=\"send\">\n<target_id>editor</target_id>\n<type>write</type>\n<path>review.md</path>\n<bytes><error>you were sent an inquiry and did not answer it: emit <call name=\n\"inquiry_answer\"><answer>what you concluded</answer></call></error>\n<reason>unhandled</reason>\n<request_id></request_id>\n</bytes>\n</call>";
    let calls = crate::tool_parse::extract_tool_calls(t);
    assert_eq!(
        calls.len(),
        1,
        "one envelope, not {}: {calls:?}",
        calls.len()
    );
    assert_eq!(calls[0].name, "send");
    let bytes = calls[0]
        .arguments
        .get("payload")
        .and_then(|p| p.get("bytes"))
        .and_then(|b| b.as_str())
        .unwrap_or("");
    assert!(
        bytes.contains("<call name=\n\"inquiry_answer\">"),
        "the inner call is DOCUMENT: {bytes:?}"
    );
    let bad = crate::tool_parse::malformed_calls(t);
    assert!(bad.is_empty(), "nothing is malformed: {bad:?}");
}

#[test]
fn a_truthy_non_list_to_is_kept_raw() {
    // python keeps `to` verbatim unless it is a string or a list, and refuses at
    // PARSE only when there is no string target AND `to` is falsy. `<to>42</to>`
    // therefore parses and routing answers with its own no-target reply — never
    // the malformed-parse text.
    let calls = extract_tool_calls("<call name=\"send\"><to>42</to><type>reflect</type></call>");
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].arguments.get("to"), Some(&json!(42)));
    assert!(calls[0].arguments.get("target_id").is_none());
    // …and a FALSY one with no target is still the refusal.
    let bad = malformed_calls("<call name=\"send\"><to>0</to><type>reflect</type></call>");
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(
        bad[0].error.contains("a send names its target"),
        "{:?}",
        bad[0].error
    );
}

#[test]
fn to_items_are_python_str() {
    // Recipients are `str(t).strip()`: `True`, `1e+20`, and a structure's python
    // repr. `Value::to_string` gave `true`, `1e20` and JSON.
    let calls = extract_tool_calls(
        "<call name=\"send\"><to><item>true</item><item>1e20</item>\
         <item><a>x</a></item><item><item>y</item></item></to><type>reflect</type></call>",
    );
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        calls[0].arguments.get("to"),
        Some(&json!(["True", "1e+20", "{'a': 'x'}", "['y']"]))
    );
}

#[test]
fn name_and_space_classes_follow_python() {
    // python `\w` is `_` + `str.isalnum()`. `char::is_alphanumeric` also admits
    // Other_Alphabetic, so `<k_नाम>` and `<xⒶ>` dispatched here and were `text
    // after the last tag` to python.
    let src = "<call name=\"send\"><target_id>a</target_id><type>t</type>\
               <k_\u{93e}>v</k_\u{93e}></call>";
    assert!(
        extract_tool_calls(src).is_empty(),
        "U+093E is not a name character to python — this does not dispatch"
    );
    let bad = malformed_calls(src);
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(
        bad[0].error.contains("text after the last tag"),
        "{:?}",
        bad[0].error
    );
    // python `\s` is `str.isspace()`, which includes `\x1c`–`\x1f`: this IS an
    // envelope.
    let calls = extract_tool_calls(
        "<call\u{1c}name=\"send\"><target_id>a</target_id><type>reflect</type></call>",
    );
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].arguments.get("target_id"), Some(&json!("a")));
}

/// #188 review: code is not markup. Blanked spans and fences keep their byte length
/// and newlines; an unmatched backtick is a plain character.
#[test]
fn code_masked_blanks_spans_and_fences_and_keeps_offsets() {
    let text = "a `<b>` c\n```html\n<div>\n```\nd ``x`y`` e ` f <i>";
    let masked = code_masked(text);
    assert_eq!(masked.len(), text.len());
    assert_eq!(
        masked.matches('\n').count(),
        text.matches('\n').count(),
        "newlines are kept"
    );
    assert!(
        !masked.contains("<b>") && !masked.contains("<div>"),
        "{masked}"
    );
    assert!(
        masked.contains("<i>"),
        "an unmatched backtick hides nothing: {masked}"
    );
}

/// A code span does not reach across a blank line, so a backtick there is literal.
#[test]
fn a_code_span_does_not_cross_a_blank_line() {
    let text = "a ` b\n\n<tool_call> `c`";
    assert!(code_masked(text).contains("<tool_call>"), "{text}");
}

/// A tag with bare words where its attributes would be is prose, not a wrapper.
#[test]
fn a_bare_word_tag_is_not_a_wrapper() {
    let text = format!("a<b and c>d\n{WRITE}");
    assert!(envelopes_in(&text)[0].wrapper.is_none());
}
