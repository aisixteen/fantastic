//! The `default` type (#766, W3): R1-R10, S9-S12, S14-S16, H9; with the `stop` call
//! (W13): R8 `stop`, S25, and R9 and S16 with its six tool names.
//!
//! The goldens compare the adapter with what `4fd5bb4d` did for the same text: the
//! stream checker's cut (`agent_loop::stream_cut`, restated here as [`today_cut`]), then
//! the router's reports (`malformed_calls` bodies, then `check(_, 0)`), byte for byte.
//! The corpus is every string literal of the four test files R9 names
//! (`res/test/default_corpus.json`, extracted once from them).

use serde_json::{json, Map, Value};

use super::default::{answer_of, Tags, FORM, FORM_OPEN, HOWTO_SEND, REPORT_STOP_REASON};
use super::{declared, fill, Out, Reader, Renderer, DEFAULT};
use crate::think::without_thinking;
use crate::tool_parse::{extract_tool_calls, malformed_calls, render_call, TOOLS};

const CORPUS: &str = include_str!("res/test/default_corpus.json");
const HISTORY: &str = include_str!("res/test/history.json");

fn corpus() -> Vec<String> {
    serde_json::from_str(CORPUS).expect("the corpus is a JSON array of strings")
}

/// Every `Out` of `deltas` fed in order, then the end.
fn run(deltas: &[&str]) -> Vec<Out> {
    let mut o = Tags.outbound();
    let mut out = Vec::new();
    for d in deltas {
        out.extend(o.feed(d));
    }
    out.extend(o.finish());
    out
}

/// Adjacent texts joined, empty texts dropped: text may come in more pieces when it
/// arrives in more deltas.
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

/// The bytes forwarded as text.
fn text_of(outs: &[Out]) -> String {
    outs.iter()
        .filter_map(|o| match o {
            Out::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// What the router of `4fd5bb4d` wrote back for a finished answer, nothing ran.
fn router_report(raw: &str) -> Option<String> {
    let answer = crate::call_check::repair(&answer_of(raw));
    let mut blocks: Vec<String> = malformed_calls(&answer).iter().map(|m| m.body()).collect();
    blocks.extend(crate::call_check::check(&answer, 0));
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

/// `4fd5bb4d`'s stream cut of a whole answer that arrived as one delta.
fn today_cut(raw: &str) -> Option<usize> {
    if !(raw.contains('>') || raw.contains('"')) {
        return None;
    }
    let stripped = answer_of(raw);
    let end = crate::call_check::foreign_cut(&stripped)?;
    let tail = &stripped[end..];
    let body = raw.trim_end();
    Some(if tail.is_empty() {
        body.len()
    } else if body.ends_with(tail) {
        body.len() - tail.len()
    } else {
        raw.len()
    })
}

/// The `Out` values `4fd5bb4d` stands for, for one delta: the text it streamed, then the
/// report its router wrote, or the cut it stored (an `Open`, its head compared apart in
/// [`open_call_names_its_head`]).
fn today(raw: &str) -> Vec<Out> {
    if let Some(keep) = today_cut(raw) {
        if let Some(report) = router_report(&raw[..keep]).or_else(|| router_report(raw)) {
            return joined(vec![
                Out::Text(raw[..keep].to_string()),
                Out::Bad { report },
            ]);
        }
    }
    let mut out = joined(vec![Out::Text(raw.to_string())]);
    let open = crate::tool_parse::grammar::envelopes_in(&answer_of(raw))
        .last()
        .is_some_and(|e| e.end.is_none());
    if open {
        out.push(Out::Open {
            name: None,
            target: None,
        });
    } else if let Some(report) = router_report(raw) {
        out.push(Out::Bad { report });
    }
    out
}

/// `outs` with the head of an `Open` left out.
fn headless(outs: Vec<Out>) -> Vec<Out> {
    outs.into_iter()
        .map(|o| match o {
            Out::Open { .. } => Out::Open {
                name: None,
                target: None,
            },
            o => o,
        })
        .collect()
}

/// The calls of a text as name and arguments (the minted ids differ per parse).
fn calls(text: &str) -> Vec<(String, Map<String, Value>)> {
    extract_tool_calls(text)
        .into_iter()
        .map(|c| (c.name, c.arguments))
        .collect()
}

/// R1-R8: internal text, rendered (the identity), read back as it streams: the text
/// comes out whole as text, in one delta and split anywhere, and parses to `want`.
fn round_trip(text: &str, want: &[(&str, Value)]) {
    assert_eq!(Tags.render(text), text);
    let one = joined(run(&[text]));
    assert_eq!(one, vec![Out::Text(text.to_string())], "{text}");
    for split in every_split(text) {
        assert_eq!(split, one, "{text}");
    }
    let got = calls(&text_of(&one));
    let want: Vec<(String, Map<String, Value>)> = want
        .iter()
        .map(|(n, a)| ((*n).to_string(), a.as_object().cloned().unwrap_or_default()))
        .collect();
    assert_eq!(got, want, "{text}");
}

fn send(args: &Value) -> String {
    render_call("send", args.as_object().expect("an object"))
}

// ── R1-R8: round trips ───────────────────────────────────────────────────────

// R1
#[test]
fn roundtrip_routing() {
    let args = json!({
        "target_id": "notes", "to": ["x", "y"], "re": "y", "reply_to": "self",
        "title": "write the report", "timeout": "30", "retries": "2", "id": "c1",
        "payload": {"type": "read", "path": "a.md"},
    });
    round_trip(&send(&args), &[("send", args.clone())]);
}

// R2
#[test]
fn roundtrip_reply_to() {
    for reply_to in [Some("null"), None, Some("lead-7")] {
        let mut args = json!({"target_id": "notes", "payload": {"type": "read"}});
        if let Some(r) = reply_to {
            args["reply_to"] = json!(r);
        }
        let want = args.clone();
        round_trip(&send(&args), &[("send", want)]);
    }
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
        let got = calls(&text);
        assert_eq!(got[0].1["payload"]["bytes"], json!(body), "{text}");
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

// R6: a `default` model writes numbers and booleans as text; the router types them.
#[test]
fn roundtrip_scalars() {
    let args = json!({
        "target_id": "notes",
        "payload": {"type": "read", "offset": "12", "length": "1.5", "all": "true", "none": "null"},
    });
    round_trip(&send(&args), &[("send", args.clone())]);
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

// R8: the five non-send tools of `4fd5bb4d` (`stop` is [`roundtrip_stop`])
#[test]
fn roundtrip_control_calls() {
    for (name, args) in [
        ("done", json!({})),
        ("inquiry_answer", json!({"answer": "yes, \"both\""})),
        ("keep", json!({"why": "the plan", "ids": ["4", "5"]})),
        ("line_summary", json!({"line": "read a.md: 3 sections"})),
    ] {
        let text = render_call(name, args.as_object().expect("an object"));
        round_trip(&text, &[(name, args.clone())]);
    }
}

// R8: `stop` and its reason, Ukrainian and the form's own characters included.
#[test]
fn roundtrip_stop() {
    for reason in [
        "the file it names does not exist",
        "файл, який він називає, не існує",
        "a \"quoted\" reason with \\ and & in it",
    ] {
        let args = json!({"reason": reason});
        let text = render_call("stop", args.as_object().expect("an object"));
        round_trip(&text, &[("stop", args.clone())]);
    }
}

// ── R9, R10: goldens ─────────────────────────────────────────────────────────

// R9: every fixture of the four test files, one delta, against `4fd5bb4d`'s path.
#[test]
fn golden_corpus() {
    let corpus = corpus();
    assert!(corpus.len() > 300, "{}", corpus.len());
    for t in &corpus {
        assert_eq!(Tags.render(t), *t, "render is the identity");
        assert_eq!(headless(joined(run(&[t.as_str()]))), today(t), "{t:?}");
    }
}

// R9: the not-a-tool refusal lists the tools as `TOOLS` has them.
#[test]
fn golden_tool_list() {
    assert_eq!(Tags.tools(), TOOLS.as_slice());
    let r = router_report("<call name=\"grep\"></call>").expect("a refusal");
    assert!(
        r.contains(&format!("the tools are {}.", TOOLS.join(", "))),
        "{r}"
    );
    assert_eq!(TOOLS.len(), 6);
    assert_eq!(TOOLS.last(), Some(&"stop"));
    assert!(
        r.contains("the tools are send, done, inquiry_answer, keep, line_summary, stop."),
        "{r}"
    );
}

// R9: `render` is the identity on a recorded history; the type's resources.
#[test]
fn golden_render_identity_on_history() {
    let rows: Vec<Value> = serde_json::from_str(HISTORY).expect("history.json");
    let mut n = 0;
    for row in &rows {
        if let Some(c) = row.get("content").and_then(Value::as_str) {
            assert_eq!(Tags.render(c), c);
            n += 1;
        }
    }
    assert!(n > 0);
    assert_eq!(Tags.name(), DEFAULT);
    let e = declared(DEFAULT).expect("default is registered");
    assert_eq!(e.calls.name(), DEFAULT);
    assert_eq!((e.texts)(super::Section::Send), HOWTO_SEND);
    assert!(HOWTO_SEND.starts_with("## Reaching other agents\n"));
    assert!(HOWTO_SEND.ends_with("— nothing else counts as your answer.\n"));
    assert_eq!(FORM_OPEN, "<call name=\"…\">");
    assert_eq!(
        FORM,
        format!("A call is {FORM_OPEN}, one child tag per argument, closed by </call>.")
    );
}

// R10: a text with no `Bad` streams through byte for byte, however it is split.
#[test]
fn stream_is_passthrough() {
    let mut checked = 0;
    for t in corpus() {
        let one = joined(run(&[t.as_str()]));
        if one.iter().any(|o| matches!(o, Out::Bad { .. })) {
            continue;
        }
        assert_eq!(text_of(&one), t);
        if t.len() <= 400 {
            for split in every_split(&t) {
                assert_eq!(text_of(&split), t, "{t:?}");
                assert_eq!(split, one, "{t:?}");
            }
        }
        checked += 1;
    }
    assert!(checked > 100, "{checked}");
}

// ── S9-S16 ───────────────────────────────────────────────────────────────────

fn bad(outs: &[Out]) -> &str {
    outs.iter()
        .find_map(|o| match o {
            Out::Bad { report } => Some(report.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Bad in {outs:?}"))
}

// S9: a `default` model that writes the hermes form wrote a foreign call.
#[test]
fn hermes_form_is_foreign() {
    let text = "I will read it.\n<tool_call>\n{\"name\": \"send\", \"arguments\": {\"routing\": \
                {\"target_id\": \"notes\"}, \"payload\": {\"type\": \"read\"}}}\n</tool_call>";
    let one = joined(run(&[text]));
    assert_eq!(one, today(text));
    let r = bad(&one);
    assert!(
        r.starts_with("[malformed call — nothing ran] line 3, col 1:"),
        "{r}"
    );
    assert!(r.contains("calls are tags, not JSON"), "{r}");
    assert!(
        r.contains("also: line 2 `<tool_call>` — drop the wrapper."),
        "{r}"
    );
    assert!(r.ends_with(FORM), "{r}");
    // nothing after the call's JSON head was forwarded
    assert_eq!(
        text_of(&one),
        "I will read it.\n<tool_call>\n{\"name\": \"send\""
    );
}

// S10: an envelope that does not parse, a wrapped call, a JSON call.
#[test]
fn bad_golden() {
    for text in [
        "<call name=\"send\"><payload><type>read</type></payload></call>",
        "<tool_call><call name=\"done\"></call></tool_call>",
        "{\"name\": \"send\", \"arguments\": {\"target_id\": \"files\", \"payload\": {\"type\": \"list\"}}}",
    ] {
        let one = joined(run(&[text]));
        assert_eq!(one, today(text), "{text}");
        let keep = today_cut(text).unwrap_or(text.len());
        assert_eq!(Some(bad(&one).to_string()), router_report(&text[..keep]), "{text}");
    }
    let r = joined(run(&[
        "<call name=\"send\"><payload><type>read</type></payload></call>",
    ]));
    assert!(bad(&r).starts_with("call NOT dispatched: "), "{r:?}");
}

// S11: an opener that lost its `<` after `</think>` is repaired at the store and the
// route, as before: no `Bad`.
#[test]
fn repair_golden() {
    let text = "<think>plan</think>call name=\"done\"></call>";
    let one = joined(run(&[text]));
    assert_eq!(one, vec![Out::Text(text.to_string())]);
    assert_eq!(one, today(text));
    let fixed = crate::call_check::repair(&without_thinking(text));
    assert_eq!(fixed, "<call name=\"done\"></call>");
}

// S12: the risky-command guard keeps its report. The call follows an open backtick, so
// the stream is not cut at its first tag and the report reads the whole call.
#[test]
fn risky_golden() {
    let text = "Running `it now:\n<send><target_id>sh</target_id><type>run</type>\
                <command>git reset --hard origin/main</command></send>";
    assert_eq!(today_cut(text), None);
    let one = joined(run(&[text]));
    assert_eq!(one, today(text));
    let r = bad(&one);
    assert!(
        r.contains("careful: this command runs `git reset --hard`"),
        "{r}"
    );
    assert!(r.contains("LEFT OUT — write the command yourself"), "{r}");
}

// S14: an opener in a fence, in inline code, in a quoted example.
#[test]
fn opener_in_code_golden() {
    for text in [
        "Like this:\n```\n<call name=\"done\"></call>\n```\nthat is all.",
        "Write `<call name=\"done\"></call>` to end.",
        "> <call name=\"done\"></call>\nas quoted.",
        "```xml\n<tool_call>{}</tool_call>\n```",
        "Use `<tool_call>` never.",
    ] {
        let one = joined(run(&[text]));
        assert_eq!(one, today(text), "{text}");
        for split in every_split(text) {
            assert_eq!(text_of(&split), text_of(&one), "{text}");
        }
    }
}

// S15: reasoning markers inside `<bytes>` are value bytes.
#[test]
fn marker_in_bytes() {
    for body in [
        "a <think>b</think> c",
        "only </think> here",
        "open <think> never closed\n",
    ] {
        let args = json!({
            "target_id": "notes",
            "payload": {"type": "write", "path": "a.md", "bytes": body},
        });
        let text = format!("<think>plan</think>{}", send(&args));
        let one = joined(run(&[text.as_str()]));
        assert_eq!(one, vec![Out::Text(text.clone())], "{body}");
        let got = calls(&answer_of(&text));
        assert_eq!(got.len(), 1, "{body}");
        assert_eq!(got[0].1["payload"]["bytes"], json!(body));
    }
}

// S16: a name that is not a tool: the report lists `TOOLS`, otherwise today's bytes.
#[test]
fn not_a_tool() {
    let text = "<call name=\"grep\"><query>x</query></call>";
    let one = joined(run(&[text]));
    assert_eq!(one, today(text));
    let r = bad(&one);
    assert_eq!(Some(r.to_string()), router_report(text));
    assert!(
        r.contains(&format!(
            "`grep` is not a tool — the tools are {}.",
            TOOLS.join(", ")
        )),
        "{r}"
    );
    assert!(r.contains("keep, line_summary, stop."), "{r}");
}

// S25: a `stop` with no reason, an empty one, one of spaces only: `Bad`, with the
// type's `report_stop_reason.txt`; one with a reason streams through.
#[test]
fn stop_needs_reason() {
    for text in [
        "<call name=\"stop\"></call>",
        "<call name=\"stop\"><reason></reason></call>",
        "<call name=\"stop\"><reason>   </reason></call>",
        "I cannot.\n<call name=\"stop\"><reason> \n </reason></call>",
    ] {
        let one = joined(run(&[text]));
        assert_eq!(bad(&one), REPORT_STOP_REASON, "{text}");
        for split in every_split(text) {
            assert_eq!(bad(&split), REPORT_STOP_REASON, "{text}");
        }
    }
    assert!(
        REPORT_STOP_REASON.starts_with("[malformed call — nothing ran] "),
        "{REPORT_STOP_REASON}"
    );
    assert!(
        REPORT_STOP_REASON.contains("<call name=\"stop\"><reason>"),
        "{REPORT_STOP_REASON}"
    );
    let good = "I cannot.\n<call name=\"stop\"><reason>no such file</reason></call>";
    assert_eq!(joined(run(&[good])), vec![Out::Text(good.to_string())]);
}

// The how-to teaches `stop` in its section on ending an answer, next to `done` (1.8).
#[test]
fn howto_teaches_stop() {
    let turns = HOWTO_SEND
        .split("## Ending an answer")
        .nth(1)
        .and_then(|s| s.split("\n## ").next())
        .expect("an Ending an answer section");
    let flat = turns.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(
            "When the work cannot be done, end it with \
             <call name=\"stop\"><reason>WHY</reason></call> and say why."
        ),
        "{flat}"
    );
    assert!(
        flat.find("name=\"done\"") < flat.find("name=\"stop\""),
        "{flat}"
    );
}

// ── H9 ───────────────────────────────────────────────────────────────────────

// H9: an envelope that does not parse in old text, and a stored cut marker, verbatim.
#[test]
fn verbatim_texts() {
    let broken =
        "[reply to - · - -]\nyour block:\n‹call name=\"send\"›<payload></payload>‹/call›\n\
                  and <call name=\"send\"><payload><type>x</type></payload></call> quoted";
    assert_eq!(Tags.render(broken), broken);
    let marker = crate::stream_parts::cut_marker(
        "<call name=\"send\"><routing><target_id>notes</target_id>",
        "generation stopped",
        &[],
    );
    assert_eq!(Tags.render(&marker), marker);
}

// S7 (the default half): a call cut at the stream's end names what was complete.
#[test]
fn open_call_names_its_head() {
    let open = |name: Option<&str>, target: Option<&str>| Out::Open {
        name: name.map(str::to_string),
        target: target.map(str::to_string),
    };
    for (text, want) in [
        ("<call name=\"se", open(None, None)),
        ("<call name=\"send\"", open(None, None)),
        ("<call name=\"send\">", open(Some("send"), None)),
        ("<call name=\"send\"><routing><target_id>no", open(Some("send"), None)),
        (
            "<call name=\"send\"><routing><target_id>notes</target_id></routing><payload>",
            open(Some("send"), Some("notes")),
        ),
        ("done.\n<call name=\"done\">", open(Some("done"), None)),
        (
            "<call name=\"send\"><routing><target_id>a</target_id></routing><payload><type>write</type><bytes>x</call>",
            open(Some("send"), Some("a")),
        ),
    ] {
        assert_eq!(joined(run(&[text])), vec![Out::Text(text.to_string()), want.clone()], "{text}");
        for split in every_split(text) {
            assert_eq!(split.last(), Some(&want), "{text}");
        }
    }
    // a call inside reasoning that never closed is reasoning, not a cut call
    let r = "<think>maybe <call name=\"done\">";
    assert_eq!(joined(run(&[r])), vec![Out::Text(r.to_string())]);
}

// ── the resources' filler ────────────────────────────────────────────────────

#[test]
fn fill_is_one_pass() {
    assert_eq!(
        fill("line {line}, col {col}:", &[("line", "3"), ("col", "1")]),
        "line 3, col 1:"
    );
    // a value is never scanned again; an unknown key stays as written
    assert_eq!(fill("{a} {b} {c", &[("a", "{b}"), ("b", "x")]), "{b} x {c");
    assert_eq!(
        fill("<{arg}>…</{arg}>", &[("arg", "type")]),
        "<type>…</type>"
    );
}
