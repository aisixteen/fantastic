//! The `hermes` type (#766, W4): R1-R8, R12-R15, S8, S9, S13-S16, S21, S22, S24, S25, K38,
//! H6-H10.
//!
//! Round trips go internal text -> [`Hermes::render`] -> the hermes reader -> internal
//! text. The two goldens that pin the form are checked-in text, not the code's output:
//! `res/test/hermes_block.txt` (R15, written by hand from spec 1.4 before `hermes.rs`)
//! and `res/test/hermes_template.txt` (R14, rendered from the public template named in
//! its header).

use serde_json::{json, Map, Value};

use super::default::{self, Tags};
use super::hermes::{
    Hermes, CALL_CLOSE, CALL_OPEN, FORM, FORM_OPEN, HINT, HOWTO_SEND, NAME, REPORT_BLOCK,
    REPORT_BYTES_IN_JSON, REPORT_FOREIGN, REPORT_JSON, REPORT_NOT_A_TOOL, REPORT_NO_OPENER,
    REPORT_NO_TARGET, REPORT_ROUTING, REPORT_ROUTING_KEY, REPORT_STOP_REASON,
};
use super::{entry, fill, Out, Reader, Renderer};
use crate::tool_parse::grammar::ROUTING;
use crate::tool_parse::{extract_tool_calls, render_call, TOOLS};

const BLOCK: &str = include_str!("res/test/hermes_block.txt");
const TEMPLATE: &str = include_str!("res/test/hermes_template.txt");

/// Every `Out` of `deltas` fed in order, then the end.
fn run(deltas: &[&str]) -> Vec<Out> {
    let mut o = Hermes.outbound();
    let mut out = Vec::new();
    for d in deltas {
        out.extend(o.feed(d));
    }
    out.extend(o.finish());
    out
}

/// Adjacent texts joined, empty texts dropped.
fn joined(outs: Vec<Out>) -> Vec<Out> {
    let mut out: Vec<Out> = Vec::new();
    for o in outs {
        match (out.last_mut(), o) {
            (_, Out::Text(t)) if t.is_empty() => {}
            (Some(Out::Text(last)), Out::Text(t)) => last.push_str(&t),
            (_, o) => out.push(o),
        }
    }
    out
}

/// `text` fed as two deltas, split at every char boundary.
fn every_split(text: &str) -> impl Iterator<Item = Vec<Out>> + '_ {
    (0..=text.len())
        .filter(|&i| text.is_char_boundary(i))
        .map(move |i| joined(run(&[&text[..i], &text[i..]])))
}

/// The bytes the reader gave back: the text forwarded and the calls in internal form.
fn bytes(outs: &[Out]) -> String {
    outs.iter()
        .map(|o| match o {
            Out::Text(t) | Out::Call { text: t } => t.as_str(),
            _ => "",
        })
        .collect()
}

/// The calls of an internal text as name and arguments.
fn calls(text: &str) -> Vec<(String, Map<String, Value>)> {
    extract_tool_calls(text)
        .into_iter()
        .map(|c| (c.name, c.arguments))
        .collect()
}

fn bad(outs: &[Out]) -> &str {
    outs.iter()
        .find_map(|o| match o {
            Out::Bad { report } => Some(report.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Bad in {outs:?}"))
}

fn obj(v: &Value) -> &Map<String, Value> {
    v.as_object().expect("an object")
}

fn send(args: &Value) -> String {
    render_call("send", obj(args))
}

/// A hermes call as a model writes it, the JSON given whole.
fn h_call(json: &str) -> String {
    format!("{CALL_OPEN}\n{json}\n{CALL_CLOSE}")
}

/// R1-R8: internal text, rendered, read back as it streams: the reader gives the
/// prose and the calls back byte for byte, in one delta and split anywhere, and the
/// calls are `want`. The hermes text holds no internal envelope.
fn round_trip(text: &str, want: &[(&str, Value)]) {
    let h = Hermes.render(text);
    assert!(!h.contains("<call name="), "{h}");
    let one = joined(run(&[h.as_str()]));
    assert!(
        one.iter()
            .all(|o| matches!(o, Out::Text(_) | Out::Call { .. })),
        "{one:?}"
    );
    assert_eq!(bytes(&one), text, "{h}");
    if h.len() <= 700 {
        for split in every_split(&h) {
            assert_eq!(split, one, "{h}");
        }
    }
    let want: Vec<(String, Map<String, Value>)> = want
        .iter()
        .map(|(n, a)| ((*n).to_string(), obj(a).clone()))
        .collect();
    assert_eq!(calls(&bytes(&one)), want, "{h}");
}

/// The call JSON line of a rendered call.
fn json_line(h: &str) -> Value {
    let line = h.lines().nth(1).expect("the JSON line");
    serde_json::from_str(line).expect("the JSON line parses")
}

/// A report filled from `template`: its text before and after the one `{error}`.
fn filled_from(report: &str, template: &str) -> bool {
    let (a, b) = template
        .split_once("{error}")
        .expect("a template with {error}");
    report.starts_with(a) && report.ends_with(b) && report.len() > a.len() + b.len()
}

// ── R1-R8 ────────────────────────────────────────────────────────────────────

// R1
#[test]
fn roundtrip_routing() {
    let args = json!({
        "target_id": "notes", "to": ["x", "y"], "re": "y", "reply_to": "self",
        "title": "write the report", "timeout": "30", "retries": "2", "id": "c1",
        "payload": {"type": "read", "path": "a.md"},
    });
    let text = send(&args);
    round_trip(&text, &[("send", args.clone())]);
    let j = json_line(&Hermes.render(&text));
    let routing: Vec<&str> = obj(&j["arguments"]["routing"])
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(routing, ROUTING.to_vec());
}

// R2
#[test]
fn roundtrip_reply_to() {
    for reply_to in [Some("null"), None, Some("lead-7")] {
        let mut args = json!({"target_id": "notes", "payload": {"type": "read"}});
        if let Some(r) = reply_to {
            args["reply_to"] = json!(r);
        }
        round_trip(&send(&args), &[("send", args.clone())]);
    }
    // a JSON `null` a model writes is the text `null`, as the default type writes it
    let text = h_call(
        r#"{"name": "send", "arguments": {"routing": {"target_id": "notes", "reply_to": null}, "payload": {"type": "read"}}}"#,
    );
    let outs = joined(run(&[text.as_str()]));
    let want = json!({"target_id": "notes", "reply_to": "null", "payload": {"type": "read"}});
    assert_eq!(
        calls(&bytes(&outs)),
        vec![("send".to_string(), obj(&want).clone())]
    );
}

// R3
#[test]
fn roundtrip_bytes() {
    for body in [
        "line one\nline \"two\" with \\ and \\n\nfn f() { println!(\"{}\", 1); }\n</tool_call> </call> \"}\n",
        "no trailing newline </call>",
        "crlf\r\nlines\r\n",
        "",
        "Привіт, світе: ї й\n",
    ] {
        let args = json!({
            "target_id": "notes",
            "payload": {"type": "write", "path": "a.md", "bytes": body},
        });
        let text = send(&args);
        round_trip(&text, &[("send", args.clone())]);
        let h = Hermes.render(&text);
        let j = json_line(&h);
        assert!(j["arguments"]["payload"].get("bytes").is_none(), "{h}");
        assert!(h.ends_with(&format!("\n<bytes>{body}</bytes>\n{CALL_CLOSE}")), "{h}");
    }
}

// R4
#[test]
fn roundtrip_odd_keys() {
    let args = json!({
        "target_id": "lead|7*",
        "payload": {"type": "update_agent", "can": {"$parent": {"call": ["send"]}}, "id": "a|b*c"},
    });
    round_trip(&send(&args), &[("send", args.clone())]);
}

// R5
#[test]
fn roundtrip_nested() {
    let args = json!({
        "target_id": "notes",
        "payload": {
            "type": "patch",
            "edits": [{"path": "a.py", "hunks": "2"}, {"path": "b.py", "hunks": "1"}],
            "ids": ["1", "2", "3"],
            "opts": {"deep": {"deeper": "x"}},
            "empty": "",
        },
    });
    round_trip(&send(&args), &[("send", args.clone())]);
}

// R6: numbers and booleans a hermes model writes become their text; the router's
// typed reader then reads them as the declared type.
#[test]
fn scalars_become_text() {
    let text = h_call(
        r#"{"name": "send", "arguments": {"routing": {"target_id": "notes", "timeout": 30, "retries": 2}, "payload": {"type": "read", "offset": 12, "length": 1.5, "all": true, "none": null}}}"#,
    );
    let outs = joined(run(&[text.as_str()]));
    let got = calls(&bytes(&outs));
    let want = json!({
        "target_id": "notes", "timeout": "30", "retries": "2",
        "payload": {"type": "read", "offset": "12", "length": "1.5", "all": "true", "none": "null"},
    });
    assert_eq!(got, vec![("send".to_string(), obj(&want).clone())]);
    let args = &got[0].1;
    assert_eq!(
        crate::tool_parse::typed::routing("timeout", &args["timeout"]).expect("a number"),
        json!(30)
    );
    assert_eq!(
        crate::tool_parse::typed::integer("retries", &args["retries"]).expect("an integer"),
        2
    );
}

// R7
#[test]
fn roundtrip_several_calls() {
    let a = json!({"target_id": "a", "payload": {"type": "read"}});
    let b = json!({"target_id": "b", "payload": {"type": "list"}});
    let text = format!("first\n{}\nthen\n{}\n{}", send(&a), send(&b), send(&a));
    round_trip(
        &text,
        &[("send", a.clone()), ("send", b.clone()), ("send", a)],
    );
}

// R8: the five non-send tools of `4fd5bb4d`
#[test]
fn roundtrip_control_calls() {
    for (name, args) in [
        ("done", json!({})),
        ("await", json!({})),
        ("inquiry_answer", json!({"answer": "yes, \"both\""})),
        ("keep", json!({"of": "4", "summary": "the plan"})),
        ("line_summary", json!({"text": "read a.md: 3 sections"})),
    ] {
        let text = render_call(name, obj(&args));
        round_trip(&text, &[(name, args.clone())]);
        let j = json_line(&Hermes.render(&text));
        assert_eq!(j, json!({"name": name, "arguments": args}));
    }
}

// R8: `stop` (needs `stop` in `TOOLS`, W13)
#[test]
fn roundtrip_stop() {
    let args = json!({"reason": "the file is gone: нема файлу"});
    let text = render_call("stop", obj(&args));
    round_trip(&text, &[("stop", args.clone())]);
    assert_eq!(
        json_line(&Hermes.render(&text)),
        json!({"name": "stop", "arguments": {"reason": "the file is gone: нема файлу"}})
    );
}

// ── R12-R15 ──────────────────────────────────────────────────────────────────

// R12: a number keeps the text the model wrote (`arbitrary_precision`).
#[test]
fn number_text_is_literal() {
    for lit in ["1.0", "1e3", "12345678901234567890", "-0", "0.10"] {
        let text = h_call(&format!(
            r#"{{"name": "send", "arguments": {{"routing": {{"target_id": "notes"}}, "payload": {{"type": "read", "n": {lit}}}}}}}"#
        ));
        let outs = joined(run(&[text.as_str()]));
        let got = calls(&bytes(&outs));
        assert_eq!(got.len(), 1, "{lit}: {outs:?}");
        assert_eq!(got[0].1["payload"]["n"], json!(lit), "{lit}");
    }
}

// R13: the writer's output reads back to the call that writes it again; other spacing,
// name/arguments order and routing order read to the same internal text.
#[test]
fn canonical_round_trip() {
    for internal in [
        send(
            &json!({"target_id": "notes", "reply_to": "self", "payload": {"type": "read", "path": "a.md"}}),
        ),
        send(
            &json!({"target_id": "notes", "payload": {"type": "write", "path": "a.md", "bytes": "x\n"}}),
        ),
        render_call("done", &Map::new()),
        render_call("inquiry_answer", obj(&json!({"answer": "так"}))),
    ] {
        let h = Hermes.render(&internal);
        let back = joined(run(&[h.as_str()]));
        assert_eq!(
            back,
            vec![Out::Call {
                text: internal.clone()
            }],
            "{h}"
        );
        assert_eq!(Hermes.render(&bytes(&back)), h);
    }
    let canonical =
        send(&json!({"target_id": "notes", "reply_to": "self", "payload": {"type": "read"}}));
    for other in [
        "<tool_call>{\"name\":\"send\",\"arguments\":{\"routing\":{\"target_id\":\"notes\",\"reply_to\":\"self\"},\"payload\":{\"type\":\"read\"}}}</tool_call>",
        "<tool_call>\n  {\"arguments\": {\"routing\": {\"target_id\": \"notes\", \"reply_to\": \"self\"}, \"payload\": {\"type\": \"read\"}}, \"name\": \"send\"}  \n</tool_call>",
        "<tool_call>\n{\"name\": \"send\", \"arguments\": {\"payload\": {\"type\": \"read\"}, \"routing\": {\"reply_to\": \"self\", \"target_id\": \"notes\"}}}\n</tool_call>",
    ] {
        assert_eq!(
            joined(run(&[other])),
            vec![Out::Call { text: canonical.clone() }],
            "{other}"
        );
    }
}

/// The sections of the recorded template text, its `#` header left out.
fn template_section(name: &str) -> &'static str {
    let body: &'static str = TEMPLATE
        .split_once("\n== call ==\n")
        .map(|(_, b)| b)
        .expect("the call section");
    let (call, rest) = body
        .split_once("\n== response ==\n")
        .expect("the response section");
    let (response, _) = rest.split_once("\n== full ==\n").expect("the full section");
    match name {
        "call" => call,
        "response" => response,
        _ => unreachable!(),
    }
}

// R14: one call and one response, rendered by the adapter, equal the text the public
// template renders for them.
#[test]
fn template_golden() {
    assert!(TEMPLATE.contains("# commit: b968826d9c46dd6066d109eabc6255188de91218"));
    let call = send(&json!({"target_id": "notes", "payload": {"type": "read", "path": "a.md"}}));
    assert_eq!(Hermes.render(&call), template_section("call"));
    let reply = "[reply to 7 · notes read]\nline one\nline two";
    assert_eq!(
        Hermes.render(reply),
        format!(
            "[reply to 7 · notes read]\n{}",
            template_section("response")
        )
    );
}

// R15: the raw block, pinned by a hand-written file.
#[test]
fn raw_block_format() {
    let body = json!({"target_id": "notes", "payload": {"type": "write", "path": "a.md", "bytes": "first line\nsecond line\n"}});
    assert_eq!(Hermes.render(&send(&body)), BLOCK);
    let none = Hermes.render(&send(
        &json!({"target_id": "notes", "payload": {"type": "write", "path": "a.md"}}),
    ));
    assert!(!none.contains("<bytes>"), "{none}");
    assert!(none.ends_with("}}}\n</tool_call>"), "{none}");
    let empty = Hermes.render(&send(
        &json!({"target_id": "notes", "payload": {"type": "write", "bytes": ""}}),
    ));
    assert!(
        empty.ends_with("}}}\n<bytes></bytes>\n</tool_call>"),
        "{empty}"
    );
}

// ── S8, S9, S13-S16, S21, S22, S24, S25 ──────────────────────────────────────

/// One hermes call text through the reader: its `Bad`.
fn bad_of(text: &str) -> String {
    let outs = joined(run(&[text]));
    assert!(
        !outs.iter().any(|o| matches!(o, Out::Call { .. })),
        "{outs:?}"
    );
    bad(&outs).to_string()
}

// S8
#[test]
fn bad_invalid_json() {
    let r = bad_of(&h_call(
        r#"{"name": "send", "arguments": {"routing": , "payload": {}}}"#,
    ));
    assert!(filled_from(&r, REPORT_JSON), "{r}");
}

// S8
#[test]
fn bad_unknown_tool() {
    let r = bad_of(&h_call(r#"{"name": "grep", "arguments": {"query": "x"}}"#));
    assert_eq!(
        r,
        fill(
            REPORT_NOT_A_TOOL,
            &[("name", "grep"), ("tools", TOOLS.join(", ").as_str())]
        )
    );
}

// S8
#[test]
fn bad_missing_routing() {
    let r = bad_of(&h_call(
        r#"{"name": "send", "arguments": {"payload": {"type": "read"}}}"#,
    ));
    assert_eq!(r, REPORT_ROUTING);
}

// S8
#[test]
fn bad_unknown_routing_key() {
    let r = bad_of(&h_call(
        r#"{"name": "send", "arguments": {"routing": {"target_id": "notes", "path": "a.md"}, "payload": {"type": "read"}}}"#,
    ));
    assert_eq!(
        r,
        fill(
            REPORT_ROUTING_KEY,
            &[("key", "path"), ("fields", ROUTING.join(", ").as_str())]
        )
    );
}

// S8
#[test]
fn bad_no_target() {
    for routing in [
        r#"{"reply_to": "self"}"#,
        r#"{"target_id": "  "}"#,
        r#"{"to": []}"#,
    ] {
        let r = bad_of(&h_call(&format!(
            r#"{{"name": "send", "arguments": {{"routing": {routing}, "payload": {{"type": "read"}}}}}}"#
        )));
        assert_eq!(r, REPORT_NO_TARGET, "{routing}");
    }
}

// S8 (section 5, item 2): a body inside the JSON is refused, not read as the body.
#[test]
fn bad_bytes_in_json() {
    let r = bad_of(&h_call(
        r#"{"name": "send", "arguments": {"routing": {"target_id": "notes"}, "payload": {"type": "write", "bytes": "x"}}}"#,
    ));
    assert_eq!(r, REPORT_BYTES_IN_JSON);
}

// S9: a hermes model that writes the default form wrote a foreign call.
#[test]
fn default_form_is_foreign() {
    let text = "I will read it.\n<call name=\"send\"><routing><target_id>notes</target_id>\
                </routing><payload><type>read</type></payload></call>";
    let outs = joined(run(&[text]));
    assert_eq!(
        outs,
        vec![
            Out::Text(text.to_string()),
            Out::Bad {
                report: REPORT_FOREIGN.to_string()
            }
        ]
    );
    for split in every_split(text) {
        assert_eq!(split, outs);
    }
    // and a hermes call after it is not run
    let after = format!("{text}\n{}", h_call(r#"{"name": "done", "arguments": {}}"#));
    assert_eq!(bad_of(&after), REPORT_FOREIGN);
}

/// S13: the call of the cut cases, written by a model, with an escape in its target.
const CUT_FULL: &str = "<tool_call>\n{\"name\": \"send\", \"arguments\": {\"routing\": {\"target_id\": \"no\\u00e9\\\"tes\"}, \"payload\": {\"type\": \"write\"}}}\n<bytes>body</bytes>\n</tool_call>";

/// S13: `prefix` of [`CUT_FULL`] is the whole stream: the head and the `Open`.
fn cut(prefix: &str, name: Option<&str>, target: Option<&str>, head: &str) {
    assert!(CUT_FULL.starts_with(prefix), "{prefix}");
    let want = vec![
        Out::Text(head.to_string()),
        Out::Open {
            name: name.map(str::to_string),
            target: target.map(str::to_string),
        },
    ];
    assert_eq!(joined(run(&[prefix])), want, "{prefix}");
    for split in every_split(prefix) {
        assert_eq!(split, want, "{prefix}");
    }
}

/// The prefix of [`CUT_FULL`] up to the end of `marker` (its first occurrence).
fn upto(marker: &str) -> &'static str {
    let i = CUT_FULL.find(marker).expect("the marker") + marker.len();
    &CUT_FULL[..i]
}

#[test]
fn cut_at_before_name() {
    cut("<tool_call>\n", None, None, "<call name=\"");
    cut("<tool_call>\n{", None, None, "<call name=\"");
    cut("<tool_call>\n{\"na", None, None, "<call name=\"");
}

#[test]
fn cut_at_inside_name() {
    cut(upto("\"name\": \"se"), None, None, "<call name=\"se");
}

#[test]
fn cut_at_after_name() {
    cut(
        upto("\"name\": \"send\""),
        Some("send"),
        None,
        "<call name=\"send\">",
    );
    cut(
        upto("\"arguments\": {"),
        Some("send"),
        None,
        "<call name=\"send\">",
    );
}

#[test]
fn cut_at_inside_routing() {
    cut(
        upto("\"routing\": {"),
        Some("send"),
        None,
        "<call name=\"send\"><routing>",
    );
    cut(
        upto("\"routing\": {\"targ"),
        Some("send"),
        None,
        "<call name=\"send\"><routing>",
    );
    cut(
        upto("\"target_id\": \"no"),
        Some("send"),
        None,
        "<call name=\"send\"><routing><target_id>no",
    );
}

#[test]
fn cut_at_after_target() {
    let head = "<call name=\"send\"><routing><target_id>noé\"tes</target_id>";
    cut(upto("tes\""), Some("send"), Some("noé\"tes"), head);
    cut(upto("\"type\": \"wr"), Some("send"), Some("noé\"tes"), head);
}

#[test]
fn cut_at_escape() {
    let head = "<call name=\"send\"><routing><target_id>no";
    cut(upto("\"target_id\": \"no\\"), Some("send"), None, head);
    cut(upto("\"target_id\": \"no\\u00"), Some("send"), None, head);
    cut(
        upto("\"target_id\": \"no\\u00e9\\"),
        Some("send"),
        None,
        "<call name=\"send\"><routing><target_id>noé",
    );
}

#[test]
fn cut_at_between_json_and_block() {
    let head = "<call name=\"send\"><routing><target_id>noé\"tes</target_id>";
    cut(upto("}}}"), Some("send"), Some("noé\"tes"), head);
    cut(upto("}}}\n"), Some("send"), Some("noé\"tes"), head);
    cut(upto("}}}\n<byt"), Some("send"), Some("noé\"tes"), head);
}

#[test]
fn cut_at_inside_block() {
    let head = "<call name=\"send\"><routing><target_id>noé\"tes</target_id>";
    cut(upto("<bytes>bo"), Some("send"), Some("noé\"tes"), head);
    cut(
        upto("<bytes>body</byt"),
        Some("send"),
        Some("noé\"tes"),
        head,
    );
}

#[test]
fn cut_at_after_block_end() {
    let head = "<call name=\"send\"><routing><target_id>noé\"tes</target_id>";
    cut(upto("</bytes>"), Some("send"), Some("noé\"tes"), head);
    cut(
        upto("</bytes>\n</tool_ca"),
        Some("send"),
        Some("noé\"tes"),
        head,
    );
    // the whole call is a call, never cut
    let whole = joined(run(&[CUT_FULL]));
    assert!(matches!(whole.as_slice(), [Out::Call { .. }]), "{whole:?}");
}

// S14: an opener in a fence, in inline code, in a quoted example: the same decision as
// the default type at the same position (the router reads a call there).
#[test]
fn opener_in_code_same_as_default() {
    for text in [
        "Like this:\n```\n<call name=\"done\"></call>\n```\nthat is all.",
        "Write `<call name=\"done\"></call>` to end.",
        "> <call name=\"done\"></call>\nas quoted.",
    ] {
        let d: Vec<String> = extract_tool_calls(text)
            .into_iter()
            .map(|c| c.name)
            .collect();
        let h = Hermes.render(text);
        let outs = joined(run(&[h.as_str()]));
        let read: Vec<String> = outs
            .iter()
            .filter_map(|o| match o {
                Out::Call { text } => Some(extract_tool_calls(text)[0].name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(read, d, "{text}");
        let at = text.find("<call").expect("the call");
        assert_eq!(
            outs.first(),
            Some(&Out::Text(text[..at].to_string())),
            "{text}"
        );
    }
}

// S15: reasoning markers inside a JSON string are value bytes.
#[test]
fn marker_in_string() {
    for title in [
        "plan <think>b</think> c",
        "only </think> here",
        "open <think> never",
    ] {
        let args = json!({"target_id": "notes", "title": title, "payload": {"type": "read"}});
        let h = format!("<think>plan</think>{}", Hermes.render(&send(&args)));
        let outs = joined(run(&[h.as_str()]));
        assert_eq!(outs.len(), 2, "{outs:?}");
        assert_eq!(outs[0], Out::Text("<think>plan</think>".into()));
        assert_eq!(
            calls(&bytes(&outs[1..])),
            vec![("send".to_string(), obj(&args).clone())]
        );
    }
}

// S15: reasoning markers inside the raw block are value bytes.
#[test]
fn marker_in_block() {
    for body in [
        "a <think>b</think> c",
        "only </think> here",
        "open <think> never closed\n",
    ] {
        let args = json!({"target_id": "notes", "payload": {"type": "write", "path": "a.md", "bytes": body}});
        let h = format!("<think>plan</think>{}", Hermes.render(&send(&args)));
        let outs = joined(run(&[h.as_str()]));
        assert_eq!(outs.len(), 2, "{outs:?}");
        let got = calls(&bytes(&outs[1..]));
        assert_eq!(got.len(), 1, "{body}");
        assert_eq!(got[0].1["payload"]["bytes"], json!(body));
    }
}

// S16: a verb written as the tool: the hermes report lists the tools and shows the
// verb in `type`.
#[test]
fn not_a_tool() {
    let r = bad_of(&h_call(
        r#"{"name": "write", "arguments": {"path": "a.md"}}"#,
    ));
    assert_eq!(
        r,
        fill(
            REPORT_NOT_A_TOOL,
            &[("name", "write"), ("tools", TOOLS.join(", ").as_str())]
        )
    );
    assert!(
        r.contains(&format!("the tools are {}.", TOOLS.join(", "))),
        "{r}"
    );
    assert!(r.contains("\"payload\": {\"type\": \"write\"}"), "{r}");
    assert_eq!(Hermes.tools(), TOOLS.as_slice());
}

// S21: a JSON call with no tags, a closer with no opener, garbled openers.
#[test]
fn no_opener_is_bad() {
    let json = r#"{"name": "send", "arguments": {"routing": {"target_id": "x"}, "payload": {"type": "read"}}}"#;
    for text in [
        format!("Here: {json}"),
        "all done </tool_call>".to_string(),
        format!("<tool_call\n{json}\n</tool_call>"),
        format!("<toolcall>{json}</toolcall>"),
    ] {
        let outs = joined(run(&[text.as_str()]));
        assert_eq!(
            outs,
            vec![
                Out::Text(text.clone()),
                Out::Bad {
                    report: REPORT_NO_OPENER.to_string()
                }
            ],
            "{text}"
        );
    }
}

// S22: the default envelope inside the tags is not JSON, and never a default call.
#[test]
fn default_body_is_bad() {
    let text = format!("{CALL_OPEN}\n<call name=\"done\"></call>\n{CALL_CLOSE}");
    let r = bad_of(&text);
    assert!(filled_from(&r, REPORT_JSON), "{r}");
}

/// S24: a well-formed send's JSON line.
const SEND_JSON: &str = r#"{"name": "send", "arguments": {"routing": {"target_id": "notes"}, "payload": {"type": "write", "path": "a.md"}}}"#;

// S24
#[test]
fn bad_block_prose() {
    let r = bad_of(&format!("{CALL_OPEN}\n{SEND_JSON}\nand then\n{CALL_CLOSE}"));
    assert_eq!(r, REPORT_BLOCK);
}

// S24
#[test]
fn bad_block_two() {
    let r = bad_of(&format!(
        "{CALL_OPEN}\n{SEND_JSON}\n<bytes>a</bytes>\n<bytes>b</bytes>\n{CALL_CLOSE}"
    ));
    assert_eq!(r, REPORT_BLOCK);
}

// S24
#[test]
fn bad_block_before_json() {
    let r = bad_of(&format!(
        "{CALL_OPEN}\n<bytes>a</bytes>\n{SEND_JSON}\n{CALL_CLOSE}"
    ));
    assert_eq!(r, REPORT_BLOCK);
}

// S24
#[test]
fn bad_block_and_json_bytes() {
    let json = r#"{"name": "send", "arguments": {"routing": {"target_id": "notes"}, "payload": {"type": "write", "bytes": "x"}}}"#;
    let r = bad_of(&format!(
        "{CALL_OPEN}\n{json}\n<bytes>a</bytes>\n{CALL_CLOSE}"
    ));
    assert_eq!(r, REPORT_BLOCK);
}

// S25 (H; needs `stop` in `TOOLS`, W13)
#[test]
fn stop_needs_reason() {
    for args in [r"{}", r#"{"reason": ""}"#, r#"{"reason": "   "}"#] {
        let r = bad_of(&h_call(&format!(
            r#"{{"name": "stop", "arguments": {args}}}"#
        )));
        assert_eq!(r, REPORT_STOP_REASON, "{args}");
    }
}

// ── K38, H6-H10 ──────────────────────────────────────────────────────────────

// K38: routing in `ROUTING` order whatever order it was written in; payload in stored
// order; the same bytes every time.
#[test]
fn key_order_stable() {
    let text = "<call name=\"send\"><routing><reply_to>self</reply_to><target_id>notes</target_id>\
                </routing><payload><z>1</z><a>2</a><type>read</type></payload></call>";
    let want = "<tool_call>\n{\"name\": \"send\", \"arguments\": {\"routing\": {\"target_id\": \"notes\", \
                \"reply_to\": \"self\"}, \"payload\": {\"z\": \"1\", \"a\": \"2\", \"type\": \"read\"}}}\n</tool_call>";
    for _ in 0..50 {
        assert_eq!(Hermes.render(text), want);
    }
}

// H6: the `keep` template on the arriving message is a call in the hermes form.
#[test]
fn keep_template_rendered() {
    let marked = crate::history_view::mark_keep("[reply to 7 · ai recall]\nold text");
    assert!(marked.contains("<call name=\"keep\">"), "{marked}");
    let h = Hermes.render(&marked);
    assert!(!h.contains("<call name="), "{h}");
    assert!(
        h.contains("<tool_call>\n{\"name\": \"keep\", \"arguments\": {\"of\": \"7\", \"summary\": \"…\"}}\n</tool_call>"),
        "{h}"
    );
    assert!(
        h.starts_with("[reply to 7 · ai recall]\n<tool_response>\nold text\n"),
        "{h}"
    );
}

/// H7, H8: a notice rendered for hermes holds no internal envelope; the default type
/// leaves it as it is.
fn notice(text: &str) -> String {
    assert_eq!(Tags.render(text), text);
    let h = Hermes.render(text);
    assert!(!h.contains("<call name="), "{h}");
    h
}

// H7: the notice texts. `EMPTY_AWAIT_NOTE` (route.rs) and `NO_CALL_NUDGE` are private
// to `route.rs`: their call text is restated.
#[test]
fn notice_texts_rendered() {
    let done = "<tool_call>\n{\"name\": \"done\", \"arguments\": {}}\n</tool_call>";
    let empty_await = "Otherwise make the call you were waiting on, or end the chain with \
                       <call name=\"done\"></call>.";
    assert!(notice(empty_await).contains(done));
    let no_target = notice(crate::route::NO_TARGET_ERROR);
    assert!(
        no_target.contains("Shape: <tool_call>\n{\"name\": \"send\", \"arguments\": {\"routing\": \
                            {\"target_id\": \"agent\"}, \"payload\": {\"type\": \"verb\"}}}\n</tool_call>."),
        "{no_target}"
    );
    let nudge = "If it is finished, end the chain explicitly: <call name=\"done\"></call>";
    assert!(notice(nudge).ends_with(done));
    assert_eq!(notice(default::HINT), HINT);
    assert_eq!(notice(default::FORM), FORM);
    assert_eq!(notice(default::FORM_OPEN), FORM_OPEN);
}

// H8: texts built at run time, one assertion per site.
#[test]
fn runtime_notice_sites() {
    let open = default::FORM_OPEN;
    // the escalation kinds left (`no_call`, `thinking_only` holds no call form)
    let no_call = format!(
        "Your next answer must START with {open} — the very first characters of the answer. \
         If the task is finished, that call is <call name=\"done\"></call>."
    );
    let h = notice(&no_call);
    assert!(
        h.starts_with(&format!("Your next answer must START with {FORM_OPEN} —")),
        "{h}"
    );
    // the parser's refusal as the router wrote it
    let refused = crate::tool_parse::malformed_calls(
        "<call name=\"send\"><routing><reply_to>self</reply_to></routing><payload><type>reflect</type></payload></call>",
    )
    .remove(0)
    .body();
    assert!(notice(&refused).contains(HINT));
    // an inquiry's head (verbs.rs)
    let head = notice(&crate::verbs::inquiry_head("lead", "5"));
    assert!(
        head.contains("<tool_call>\n{\"name\": \"inquiry_answer\", \"arguments\": {\"answer\": \"…\"}}\n</tool_call>"),
        "{head}"
    );
    // a refusal with an inserted name, delivered as a reply block
    let refusal = format!(
        "[reply to 12 · notes grep]\ncall NOT dispatched: `notes` does not answer `grep`. {}",
        crate::route::NO_TARGET_ERROR
    );
    let h = notice(&refusal);
    assert!(
        h.starts_with("[reply to 12 · notes grep]\n<tool_response>\ncall NOT dispatched: `notes`")
    );
    assert!(h.ends_with("\n</tool_response>"), "{h}");
    // the nudge after a turn that retried
    let nudge = "[no call was dispatched] Your answer emitted no <call> element, so NOTHING ran. \
                 If it is finished, end the chain explicitly: <call name=\"done\"></call>";
    notice(nudge);
}

// H9: an envelope that does not parse in old text, and a stored cut marker, verbatim.
#[test]
fn verbatim_texts() {
    let broken = "your block:\n‹call name=\"send\"›<payload></payload>‹/call›\n\
                  and <call name=\"send\"><payload><type>x</type></payload></call> quoted";
    assert_eq!(Hermes.render(broken), broken);
    let in_reply = format!("[reply to - · - -]\n{broken}");
    assert_eq!(
        Hermes.render(&in_reply),
        format!("[reply to - · - -]\n<tool_response>\n{broken}\n</tool_response>")
    );
    let marker = crate::stream_parts::cut_marker(
        "<call name=\"send\"><routing><target_id>notes</target_id>",
        "generation stopped",
        &[],
    );
    assert_eq!(Hermes.render(&marker), marker);
    let open = "text <call name=\"send\"><routing><target_id>notes</target_id>";
    assert_eq!(Hermes.render(open), open);
}

// H10: a call quoted inside another call's `<bytes>` is that body's bytes.
#[test]
fn bytes_value_not_scanned() {
    let inner = "<call name=\"done\"></call>";
    let args = json!({"target_id": "notes", "payload": {"type": "write", "path": "a.md", "bytes": format!("see {inner}\n")}});
    let h = Hermes.render(&send(&args));
    assert_eq!(h.matches(CALL_OPEN).count(), 1, "{h}");
    assert!(h.contains(&format!("<bytes>see {inner}\n</bytes>")), "{h}");
    let outs = joined(run(&[h.as_str()]));
    assert_eq!(
        calls(&bytes(&outs)),
        vec![("send".to_string(), obj(&args).clone())]
    );
}

// ── the type's registration and resources ────────────────────────────────────

#[test]
fn registered_with_its_howto() {
    let e = entry(NAME).expect("hermes is registered");
    assert_eq!(e.renderer.name(), NAME);
    assert!(!e.renderer.identity());
    assert_eq!(e.renderer.rev(), 1);
    assert_eq!(e.howto_send, HOWTO_SEND);
    assert!(HOWTO_SEND.starts_with("## Reaching other agents\n"));
    // the `<tools>` block names the seven tools, one signature per line
    let tools: Vec<&str> = HOWTO_SEND
        .split_once("<tools>\n")
        .and_then(|(_, r)| r.split_once("\n</tools>"))
        .map(|(t, _)| t)
        .expect("a tools block")
        .lines()
        .collect();
    let names: Vec<String> = tools
        .iter()
        .map(|l| {
            let v: Value = serde_json::from_str(l).expect("one signature per line");
            v["function"]["name"].as_str().expect("a name").to_string()
        })
        .collect();
    assert_eq!(
        names,
        [
            "send",
            "done",
            "await",
            "inquiry_answer",
            "keep",
            "line_summary",
            "stop"
        ]
    );
    // the stop wording, and no internal envelope
    assert!(HOWTO_SEND.contains(
        "`{\"name\": \"stop\", \"arguments\": {\"reason\": \"WHY\"}}` (the work cannot be done: say why)"
    ));
    assert!(!HOWTO_SEND.contains("<call name="));
    // the how-to survives its own type's render
    assert_eq!(Hermes.render(HOWTO_SEND), HOWTO_SEND);
}
