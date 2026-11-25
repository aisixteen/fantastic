//! The hold-back scanner of a reader with a held call (#766, S1-S7), on a test opener
//! pair: a call opens with `<tool_call>` and its body ends at the first `</tool_call>`.
//! The type readers (W3 `default`, W4 `hermes`) bring their own halves of S1 and S4.

use super::stream::{Body, Piece, Scanner};
use super::{Out, Outbound};

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

/// The test pair's body: the call ends right after the first `</tool_call>`, which may
/// arrive split across deltas.
#[derive(Debug, Default)]
struct UntilClose {
    seen: String,
}

impl Body for UntilClose {
    fn feed(&mut self, more: &str) -> Option<usize> {
        let before = self.seen.len();
        self.seen.push_str(more);
        let from = before.saturating_sub(CLOSE.len() - 1);
        let from = (0..=from)
            .rev()
            .find(|&i| self.seen.is_char_boundary(i))
            .unwrap_or(0);
        self.seen[from..]
            .find(CLOSE)
            .map(|i| from + i + CLOSE.len() - before)
    }
}

/// A reader on the scanner: a held call comes out as one `Call`, a call cut at the end
/// as `Open` with nothing of its head read (the test pair has no grammar for it).
struct TestOutbound {
    scan: Scanner<UntilClose>,
}

impl TestOutbound {
    fn new() -> Self {
        Self {
            scan: Scanner::new(OPEN),
        }
    }

    fn map(pieces: Vec<Piece>) -> Vec<Out> {
        pieces
            .into_iter()
            .map(|p| match p {
                Piece::Text(t) => Out::Text(t),
                Piece::Call(text) => Out::Call { text },
                Piece::Open(_) => Out::Open {
                    name: None,
                    target: None,
                },
            })
            .collect()
    }
}

impl Outbound for TestOutbound {
    fn feed(&mut self, delta: &str) -> Vec<Out> {
        Self::map(self.scan.feed(delta))
    }

    fn finish(&mut self) -> Vec<Out> {
        Self::map(self.scan.finish())
    }
}

/// Every `Out` of `deltas` fed in order, then the end.
fn run(deltas: &[&str]) -> Vec<Out> {
    let mut o = TestOutbound::new();
    let mut out = Vec::new();
    for d in deltas {
        out.extend(o.feed(d));
    }
    out.extend(o.finish());
    out
}

/// `outs` with every run of adjacent `Text` joined: text may come in more pieces when it
/// arrives in more deltas, and nothing else may differ.
fn joined(outs: Vec<Out>) -> Vec<Out> {
    let mut v: Vec<Out> = Vec::new();
    for o in outs {
        match (v.last_mut(), o) {
            (Some(Out::Text(a)), Out::Text(b)) => a.push_str(&b),
            (_, o) => v.push(o),
        }
    }
    v
}

/// The bytes of every `Out` that carries any, in order.
fn bytes(outs: &[Out]) -> String {
    outs.iter()
        .map(|o| match o {
            Out::Text(t) | Out::Call { text: t } => t.as_str(),
            _ => "",
        })
        .collect()
}

fn call(body: &str) -> String {
    format!("{OPEN}{body}{CLOSE}")
}

/// S1: split at every byte offset (each char boundary) into two deltas, and fed one char
/// at a time, a stream gives the same `Out` sequence as fed whole.
#[test]
fn every_split_same_output() {
    let texts = [
        format!(
            "Привіт <think>план: {OPEN} ні</think>текст {} і далі <caller> <tool_calls_x {}",
            call("{\"name\":\"send\"}"),
            call("<think>x</think>")
        ),
        format!("<think>без кінця {OPEN}"),
        format!("так {OPEN}{{\"name\":\"send\",\"arguments\":{{"),
        format!("{}{}", call(""), call("</tool_cal")),
        "<<tool_call<tool_call>>".to_string(),
        "кінець <".to_string(),
    ];
    for text in &texts {
        let whole = joined(run(&[text.as_str()]));
        for (i, _) in text.char_indices().skip(1) {
            let (a, b) = text.split_at(i);
            assert_eq!(joined(run(&[a, b])), whole, "{text:?} split at {i}");
        }
        let chars: Vec<String> = text.chars().map(String::from).collect();
        let deltas: Vec<&str> = chars.iter().map(String::as_str).collect();
        assert_eq!(joined(run(&deltas)), whole, "{text:?} char by char");
    }
}

/// S2: prose before a call is forwarded before the call closes; the call is held and
/// comes out once, whole.
#[test]
fn prose_live_call_held() {
    let mut o = TestOutbound::new();
    assert_eq!(o.feed("Ось файл: "), vec![Out::Text("Ось файл: ".into())]);
    assert_eq!(o.feed("<tool"), vec![]);
    assert_eq!(o.feed("_call>{\"name\":"), vec![]);
    assert_eq!(o.feed("\"send\"}</tool_"), vec![]);
    assert_eq!(
        o.feed("call> далі"),
        vec![
            Out::Call {
                text: call("{\"name\":\"send\"}")
            },
            Out::Text(" далі".into())
        ]
    );
    assert_eq!(o.finish(), vec![]);
}

/// S3: an opener look-alike is prose, released as soon as it cannot be the opener.
#[test]
fn lookalike_is_prose() {
    let mut o = TestOutbound::new();
    assert_eq!(
        o.feed("a <caller> b"),
        vec![Out::Text("a <caller> b".into())]
    );
    assert_eq!(o.feed(" <tool_call"), vec![Out::Text(" ".into())]);
    assert_eq!(o.feed("s_x"), vec![Out::Text("<tool_calls_x".into())]);
    assert_eq!(o.feed(" <tool_call"), vec![Out::Text(" ".into())]);
    assert_eq!(o.finish(), vec![Out::Text("<tool_call".into())]);

    let text = "<caller>x</caller> <tool_calls_x> <tool_call\n{} <toolcall>{}</toolcall>";
    assert_eq!(joined(run(&[text])), vec![Out::Text(text.into())]);
}

/// S4: a call written inside reasoning is reasoning text: no `Call`.
#[test]
fn call_in_reasoning_not_run() {
    let text = format!(
        "<think>можна так: {} або ні</think>гаразд",
        call("{\"name\":\"send\"}")
    );
    let outs = run(&[text.as_str()]);
    assert!(outs.iter().all(|o| matches!(o, Out::Text(_))), "{outs:?}");
    assert_eq!(bytes(&outs), text);
}

/// S5: reasoning that never closes runs to the end as text: no `Call`, no `Bad`.
#[test]
fn open_reasoning_is_text() {
    let text = format!("<think>план {} і ще {OPEN}{{", call("{\"name\":\"send\"}"));
    let outs = run(&[text.as_str()]);
    assert!(outs.iter().all(|o| matches!(o, Out::Text(_))), "{outs:?}");
    assert_eq!(bytes(&outs), text);
    let mut o = TestOutbound::new();
    o.feed("<think>ще дума</thi");
    assert_eq!(o.finish(), vec![Out::Text("</thi".into())]);
}

/// S6: reasoning streams live, then the call comes out as one `Call`; inside the held
/// call the reasoning markers are value bytes.
#[test]
fn reasoning_then_call() {
    let mut o = TestOutbound::new();
    assert_eq!(
        o.feed("<think>я под"),
        vec![Out::Text("<think>я под".into())]
    );
    assert_eq!(
        o.feed("умаю</think>"),
        vec![Out::Text("умаю</think>".into())]
    );
    assert_eq!(o.feed("<tool_call>{\"a\":\"<think>\"}"), vec![]);
    assert_eq!(
        o.feed("</tool_call>"),
        vec![Out::Call {
            text: call("{\"a\":\"<think>\"}")
        }]
    );
    assert_eq!(o.finish(), vec![]);

    let held = call("{\"a\":\"</think> <tool_call>\"}");
    let text = format!("<think>x</think>{held}");
    assert_eq!(
        joined(run(&[text.as_str()])),
        vec![
            Out::Text("<think>x</think>".into()),
            Out::Call { text: held }
        ]
    );
}

/// S7: a call unterminated at the stream's end is `Open`, holding every byte from its
/// opener; nothing of it was forwarded as text.
#[test]
fn open_call_at_end() {
    let text = format!("гаразд {OPEN}{{\"name\":\"send\",\"arguments\":{{\"routing\"");
    assert_eq!(
        joined(run(&[text.as_str()])),
        vec![
            Out::Text("гаразд ".into()),
            Out::Open {
                name: None,
                target: None
            }
        ]
    );
    let mut s: Scanner<UntilClose> = Scanner::new(OPEN);
    assert_eq!(s.feed(&text), vec![Piece::Text("гаразд ".into())]);
    assert_eq!(
        s.finish(),
        vec![Piece::Open(text["гаразд ".len()..].to_string())]
    );
    // an opener that is the last thing written is a call cut at once
    assert_eq!(
        run(&["x ", OPEN]),
        vec![
            Out::Text("x ".into()),
            Out::Open {
                name: None,
                target: None
            }
        ]
    );
}

// ── the `default` reader's halves of S1 and S4 ───────────────────────────────

/// Every `Out` of `deltas` through the `default` reader, then the end.
fn run_default(deltas: &[&str]) -> Vec<Out> {
    use super::Reader;
    let mut o = super::default::Tags.outbound();
    let mut out = Vec::new();
    for d in deltas {
        out.extend(o.feed(d));
    }
    out.extend(o.finish());
    joined(
        out.into_iter()
            .filter(|o| *o != Out::Text(String::new()))
            .collect(),
    )
}

/// S1 (D): split at every char boundary, and fed one char at a time, the `default`
/// reader gives the same `Out` sequence as fed whole: good calls, reasoning, a cut call,
/// a call written wrong.
#[test]
fn default_every_split_same_output() {
    let texts = [
        "Привіт <think>план <call name=\"done\"></call></think>текст \
         <call name=\"send\"><routing><target_id>notes</target_id></routing>\
         <payload><type>read</type></payload></call> і далі"
            .to_string(),
        "<think>без кінця <call name=\"done\">".to_string(),
        "так <call name=\"send\"><routing><target_id>a</target_id>".to_string(),
        "<call name=\"send\"><payload><type>x</type></payload></call>".to_string(),
        "<call name=\"grep\"></call>".to_string(),
        "кінець <".to_string(),
    ];
    for text in &texts {
        let whole = run_default(&[text.as_str()]);
        for (i, _) in text.char_indices().skip(1) {
            assert_eq!(
                run_default(&[&text[..i], &text[i..]]),
                whole,
                "{text} at {i}"
            );
        }
        let chars: Vec<String> = text.chars().map(String::from).collect();
        let deltas: Vec<&str> = chars.iter().map(String::as_str).collect();
        assert_eq!(run_default(&deltas), whole, "{text} char by char");
    }
}

/// S4 (D): a call, and a call written wrong, inside reasoning are text: nothing is run
/// and nothing is reported.
#[test]
fn default_call_in_reasoning_not_run() {
    let text = "<think>maybe <call name=\"done\"></call> or <tool_call>{\"name\": \"send\"}\
                </tool_call></think>No call this time.";
    assert_eq!(run_default(&[text]), vec![Out::Text(text.to_string())]);
    assert!(crate::tool_parse::extract_tool_calls(&super::default::answer_of(text)).is_empty());
}

// ── the `hermes` reader (W4): its halves of S1 and S4, and S18-S20, S23 ───────

/// Every `Out` of `deltas` through the `hermes` reader, then the end, texts joined.
fn run_hermes(deltas: &[&str]) -> Vec<Out> {
    use super::Reader;
    let mut o = super::hermes::Hermes.outbound();
    let mut out = Vec::new();
    for d in deltas {
        out.extend(o.feed(d));
    }
    out.extend(o.finish());
    joined(
        out.into_iter()
            .filter(|o| *o != Out::Text(String::new()))
            .collect(),
    )
}

/// `text` through the `hermes` reader split in two at every char boundary, and fed one
/// char at a time: each gives what the whole text gives.
fn hermes_same_every_split(text: &str) -> Vec<Out> {
    let whole = run_hermes(&[text]);
    for (i, _) in text.char_indices().skip(1) {
        assert_eq!(
            run_hermes(&[&text[..i], &text[i..]]),
            whole,
            "{text:?} at {i}"
        );
    }
    let chars: Vec<String> = text.chars().map(String::from).collect();
    let deltas: Vec<&str> = chars.iter().map(String::as_str).collect();
    assert_eq!(run_hermes(&deltas), whole, "{text:?} char by char");
    whole
}

/// A hermes send to `notes` titled `title`, its body (when given) in the raw block.
fn h_send(title: &str, body: Option<&str>) -> String {
    let block = body.map_or(String::new(), |b| format!("<bytes>{b}</bytes>\n"));
    format!(
        "<tool_call>\n{{\"name\": \"send\", \"arguments\": {{\"routing\": {{\"target_id\": \"notes\", \
         \"title\": \"{title}\"}}, \"payload\": {{\"type\": \"write\", \"path\": \"a.md\"}}}}}}\n{block}</tool_call>"
    )
}

/// The internal text of [`h_send`].
fn i_send(title: &str, body: Option<&str>) -> String {
    let mut payload = serde_json::json!({"type": "write", "path": "a.md"});
    if let Some(b) = body {
        payload["bytes"] = serde_json::json!(b);
    }
    let args = serde_json::json!({"target_id": "notes", "title": title, "payload": payload});
    crate::tool_parse::render_call("send", args.as_object().expect("an object"))
}

/// S1 (H): split at every char boundary, and fed one char at a time, the `hermes` reader
/// gives the same `Out` sequence as fed whole: prose, reasoning, calls, a cut call, a bad
/// call.
#[test]
fn hermes_every_split_same_output() {
    let texts = [
        format!(
            "Привіт <think>план {}</think>текст {} і далі",
            h_send("x", None),
            h_send("Огляд", Some("a\n</tool_call>\n"))
        ),
        format!("<think>без кінця {}", h_send("x", None)),
        "так <tool_call>\n{\"name\": \"send\", \"arguments\": {\"routing\": {\"target_id\": \"no"
            .to_string(),
        "<tool_call>\n{\"name\": \"grep\", \"arguments\": {}}\n</tool_call> далі".to_string(),
        "<tool_call\n{\"name\": \"done\", \"arguments\": {}}".to_string(),
        "кінець <".to_string(),
    ];
    for text in &texts {
        hermes_same_every_split(text);
    }
}

/// S4 (H): a call, and a call written wrong, inside reasoning are text: nothing is run
/// and nothing is reported.
#[test]
fn hermes_call_in_reasoning_not_run() {
    let text = format!(
        "<think>maybe {} or <call name=\"done\"></call> or {{\"name\": \"done\", \"arguments\": {{}}}} \
         </tool_call></think>No call this time.",
        h_send("x", None)
    );
    assert_eq!(run_hermes(&[text.as_str()]), vec![Out::Text(text.clone())]);
}

/// S18: a complete call, then a second call cut by the token limit: one `Call`, then the
/// cut call's internal head and `Open`.
#[test]
fn call_then_cut_call() {
    let cut = "<tool_call>\n{\"name\": \"send\", \"arguments\": {\"routing\": {\"target_id\": \"files\"}, \"payload\": {\"ty";
    let text = format!("{}\n{cut}", h_send("x", None));
    let got = hermes_same_every_split(&text);
    assert_eq!(
        got,
        vec![
            Out::Call {
                text: i_send("x", None)
            },
            Out::Text("\n<call name=\"send\"><routing><target_id>files</target_id>".into()),
            Out::Open {
                name: Some("send".into()),
                target: Some("files".into())
            },
        ]
    );
}

/// S19: prose after the last call is forwarded; the call is still one `Call`.
#[test]
fn prose_after_call() {
    let text = format!("Пишу. {} Готово, чекаю.", h_send("x", Some("рядок\n")));
    assert_eq!(
        hermes_same_every_split(&text),
        vec![
            Out::Text("Пишу. ".into()),
            Out::Call {
                text: i_send("x", Some("рядок\n"))
            },
            Out::Text(" Готово, чекаю.".into()),
        ]
    );
}

/// S20: `</tool_call>` inside a JSON string is value bytes, however the stream is split.
#[test]
fn closer_in_string() {
    let text = h_send("a </tool_call> b", None);
    assert_eq!(
        hermes_same_every_split(&text),
        vec![Out::Call {
            text: i_send("a </tool_call> b", None)
        }]
    );
}

/// S20: `</tool_call>` inside the raw block is value bytes, however the stream is split.
#[test]
fn closer_in_block() {
    let body = "x </tool_call> y\n</tool_call>\n";
    let text = h_send("t", Some(body));
    assert_eq!(
        hermes_same_every_split(&text),
        vec![Out::Call {
            text: i_send("t", Some(body))
        }]
    );
}

/// S23: the token limit right after `</tool_call>` leaves a whole call: `Call`, no
/// `Open`.
#[test]
fn limit_after_closer() {
    let text = h_send("x", Some("b"));
    let mut o = {
        use super::Reader;
        super::hermes::Hermes.outbound()
    };
    assert_eq!(
        o.feed(&text),
        vec![Out::Call {
            text: i_send("x", Some("b"))
        }]
    );
    assert_eq!(o.finish(), vec![]);
}
