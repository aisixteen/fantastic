//! The CALL CHECKER — what a model wrote as a call that is not one (owner,
//! 2026-10-07). Python `ai_core/call_check.py`, report for report.
//!
//! A model may invent any call syntax — a `<tool_call>` wrapper, `<function=call
//! name="send">`, `<send>` as a tag, a JSON object, `<parameter=x>` arguments — and
//! none of it opens a `<call` envelope, so the scanner finds nothing and the turn
//! reads as call-less (measured 2026-10-07, E59 `calc` on qwen3.8-27b: 18 of 25
//! replies were `<tool_call><function=call name="send">…</call>`, each answered with
//! the call-less reminder, the same shape repeated until the chain gave up).
//!
//! The foreign syntaxes cannot be listed, so this checks the other way round: OUR
//! grammar and OUR vocabulary are closed — `<call name="N">`, one child tag per
//! argument, `</call>`; the call names ([`TOOLS`]) and the routing tags are known —
//! and whatever markup carries that vocabulary OUTSIDE every `<call` envelope is a
//! call written wrong. Each is reported like a compiler error: the line, a caret
//! under the first token that departs from the grammar, and the form to write
//! instead, built from the name and arguments the model used.
//!
//! Nothing here runs a call: one grammar, one parser. Positions are BYTE offsets
//! into the answer (the mask keeps every byte where it was); lines and columns are
//! counted in characters, as python counts them.

use crate::tool_parse::grammar::{envelopes, ROUTING};
use crate::tool_parse::{defang, CLOSE, TOOLS};
use fantastic_bundle::py_str;
use regex::Regex;
use serde_json::{Map, Value};
use std::sync::OnceLock;

/// At most this many malformed calls are reported per answer, then a count.
pub const MAX_REPORTS: usize = 3;
/// Chars of a quoted line.
const SHOWN: usize = 160;

/// The argument tags a call carries that are ours by name: the routing fields and
/// the two every payload has.
fn is_arg_tag(name: &str) -> bool {
    ROUTING.contains(&name) || name == "type" || name == "bytes"
}

fn tag_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"<(/?)([^\s<>/=]*)([^<>\n]{0,300}?)(/?)>").expect("tag"))
}

fn named_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"\bname\s*=\s*["']?([A-Za-z_]\w*)"#).expect("named"))
}

fn eq_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"^=\s*["']?([A-Za-z_]\w*)"#).expect("eq"))
}

fn json_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(&format!(
            r#"\{{\s*"(?:name|tool|tool_name|function)"\s*:\s*"({})""#,
            TOOLS.join("|")
        ))
        .expect("json")
    })
}

fn close_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"</[^<>\n]{0,40}>").expect("close"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Opener,
    Close,
    Arg,
    Param,
    Other,
    Json,
}

#[derive(Debug, Clone)]
struct Tok {
    kind: Kind,
    text: String,
    start: usize,
    end: usize,
    name: Option<String>,
    arg: Option<String>,
    closing: bool,
}

#[derive(Debug, Default)]
struct Call {
    toks: Vec<Tok>,
    wrappers: Vec<Tok>,
}

/// The answer with every `<call` envelope blanked — those are the scanner's
/// (dispatched, or reported as unparsable). Every byte keeps its offset (a char is
/// replaced by as many spaces as it has bytes); newlines are kept so lines count.
fn mask(text: &str) -> String {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (pos, _after, end) in envelopes(text) {
        spans.push((pos, end.map_or(text.len(), |e| e + CLOSE.len())));
    }
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.char_indices() {
        if c != '\n' && spans.iter().any(|(a, b)| i >= *a && i < *b) {
            out.extend(std::iter::repeat_n(' ', c.len_utf8()));
        } else {
            out.push(c);
        }
    }
    out
}

fn classify(m: &regex::Captures<'_>) -> Tok {
    let whole = m.get(0).expect("match");
    let slash = !m.get(1).map_or("", |g| g.as_str()).is_empty();
    let head = m.get(2).map_or("", |g| g.as_str());
    let rest = m.get(3).map_or("", |g| g.as_str());
    let mut t = Tok {
        kind: Kind::Other,
        text: whole.as_str().to_string(),
        start: whole.start(),
        end: whole.end(),
        name: None,
        arg: None,
        closing: slash,
    };
    let named = named_re()
        .captures(rest)
        .and_then(|c| c.get(1))
        .map(|g| g.as_str().to_string());
    let eq = eq_re()
        .captures(rest)
        .and_then(|c| c.get(1))
        .map(|g| g.as_str().to_string());
    let tool = |n: &Option<String>| n.as_deref().is_some_and(|n| TOOLS.contains(&n));
    if head == "call" {
        t.kind = if slash { Kind::Close } else { Kind::Opener };
        t.name = named;
    } else if TOOLS.contains(&head) {
        t.kind = if slash { Kind::Close } else { Kind::Opener };
        t.name = Some(head.to_string());
    } else if !slash && tool(&named) {
        t.kind = Kind::Opener;
        t.name = named;
    } else if !slash && tool(&eq) {
        t.kind = Kind::Opener;
        t.name = eq;
    } else if !slash && eq.as_deref().is_some_and(is_arg_tag) {
        t.kind = Kind::Param;
        t.arg = eq;
    } else if is_arg_tag(head) {
        t.kind = Kind::Arg;
        t.arg = Some(head.to_string());
    }
    t
}

fn tokens(masked: &str) -> Vec<Tok> {
    let mut toks: Vec<Tok> = tag_re()
        .captures_iter(masked)
        .map(|c| classify(&c))
        .collect();
    for c in json_re().captures_iter(masked) {
        let whole = c.get(0).expect("match");
        toks.push(Tok {
            kind: Kind::Json,
            text: whole.as_str().to_string(),
            start: whole.start(),
            end: whole.end(),
            name: c.get(1).map(|g| g.as_str().to_string()),
            arg: None,
            closing: false,
        });
    }
    toks.sort_by_key(|t| t.start);
    toks
}

fn newlines(s: &str, upto: usize) -> usize {
    s[..upto].matches('\n').count()
}

/// The call-like runs: each starts at a call-like token and ends at a closing
/// `</call>` / `</<call name>>` (a JSON object is one run). Foreign tags right before
/// a run's first token — on its line or the one above — are its wrappers.
fn calls(masked: &str, toks: Vec<Tok>) -> Vec<Call> {
    let mut out: Vec<Call> = Vec::new();
    let mut cur: Option<Call> = None;
    let mut pending: Vec<Tok> = Vec::new();
    for t in toks {
        if t.kind == Kind::Other {
            match cur.as_mut() {
                Some(c) => c.toks.push(t),
                None => pending.push(t),
            }
            continue;
        }
        if cur.is_none() {
            let first_line = newlines(masked, t.start);
            let wrappers = std::mem::take(&mut pending)
                .into_iter()
                .filter(|w| !w.closing && first_line - newlines(masked, w.start) <= 1)
                .collect();
            cur = Some(Call {
                toks: Vec::new(),
                wrappers,
            });
        }
        let ends = matches!(t.kind, Kind::Json | Kind::Close);
        if let Some(c) = cur.as_mut() {
            c.toks.push(t);
        }
        if ends {
            out.extend(cur.take());
        }
    }
    out.extend(cur);
    out.into_iter()
        .filter(|c| c.toks.iter().any(|t| t.kind != Kind::Other))
        .collect()
}

/// Python `s.strip()` then `[:n] + "…"` past `n` chars.
fn clip(v: &str, n: usize) -> String {
    if v.chars().count() <= n {
        v.to_string()
    } else {
        v.chars().take(n).collect::<String>() + "…"
    }
}

/// The text between an argument's open tag and its close, on the masked answer.
fn value(masked: &str, tok: &Tok, closer: &str) -> Option<String> {
    let end = masked[tok.end..].find(closer)? + tok.end;
    Some(clip(masked[tok.end..end].trim(), 200))
}

/// The arguments of a JSON call object, flattened one level (a `payload` /
/// `arguments` / `parameters` object's keys become tags of the call).
fn json_args(masked: &str, tok: &Tok) -> Vec<(String, Value)> {
    let mut de = serde_json::Deserializer::from_str(&masked[tok.start..]).into_iter::<Value>();
    let Some(Ok(Value::Object(obj))) = de.next() else {
        return Vec::new();
    };
    let mut args: Map<String, Value> = Map::new();
    for (k, v) in obj {
        if ["name", "tool", "tool_name", "function"].contains(&k.as_str()) {
            continue;
        }
        if let Value::Object(inner) = v {
            for (k2, v2) in inner {
                if let Value::Object(deep) = v2 {
                    args.extend(deep);
                } else {
                    args.insert(k2, v2);
                }
            }
        } else {
            args.insert(k, v);
        }
    }
    args.into_iter().collect()
}

fn expected(name: &str, args: &[(String, Value)]) -> String {
    if args.is_empty() {
        return format!("<call name=\"{name}\"></call>");
    }
    let mut lines = vec![format!("<call name=\"{name}\">")];
    for (k, v) in args {
        let inner = match v {
            Value::Array(xs) => xs
                .iter()
                .map(|x| format!("<item>{}</item>", py_str(x)))
                .collect(),
            other => py_str(other),
        };
        lines.push(format!("<{k}>{inner}</{k}>"));
    }
    lines.push("</call>".to_string());
    lines.join("\n")
}

/// (line, col, the quoted line, its caret) — 1-based, the line windowed to
/// [`SHOWN`] chars around the token, defanged so the quote cannot arm the scanner
/// (`defang` swaps one char for one, so the caret still lines up).
fn place(text: &str, tok: &Tok) -> (usize, usize, String, String) {
    let line_start = text[..tok.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[tok.start..]
        .find('\n')
        .map_or(text.len(), |i| i + tok.start);
    let line: Vec<char> = text[line_start..line_end].chars().collect();
    let col = text[line_start..tok.start].chars().count();
    let lo = col.saturating_sub(40);
    let shown: String = line.iter().skip(lo).take(SHOWN).collect();
    let shown_len = shown.chars().count();
    let lead = if lo > 0 { "…" } else { "" };
    let width = tok.text.chars().count().min(shown_len - (col - lo)).max(1);
    let caret = format!(
        "{}^{}",
        " ".repeat(lead.chars().count() + col - lo),
        "~".repeat(width - 1)
    );
    (
        newlines(text, tok.start) + 1,
        col + 1,
        format!("{lead}{}", defang(&shown)),
        caret,
    )
}

/// The name of a bare opening tag (`<query>`, no attributes) that is none of
/// ours — inside a call run, an argument of the verb. Python `_plain`.
fn plain(t: &Tok) -> Option<String> {
    if t.kind != Kind::Other {
        return None;
    }
    let inner = t.text.strip_prefix('<')?.strip_suffix('>')?;
    let mut chars = inner.chars();
    let first = chars.next()?;
    let ok = (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_' || c == '-');
    (ok && inner != "item").then(|| inner.to_string())
}

fn report(text: &str, masked: &str, call: &Call) -> String {
    let first = call
        .toks
        .iter()
        .find(|t| t.kind != Kind::Other)
        .expect("a call-like token");
    let name = call
        .toks
        .iter()
        .find_map(|t| t.name.clone())
        .unwrap_or_else(|| "send".to_string());
    let mut args: Vec<(String, Value)> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let why = match first.kind {
        Kind::Json => {
            args = json_args(masked, first);
            "calls are tags, not JSON"
        }
        Kind::Arg | Kind::Param => "an argument outside a call — open the call above it",
        Kind::Close => "a close with no call open",
        _ => "not a call tag",
    };
    // EVERY ARGUMENT THE MODEL WROTE carries into the form to write — the verb's
    // own (`<query>`, `<ids>`) as much as ours by name. Measured 2026-10-07
    // (misleading_fold:rust): a grep scoped by `<ids>` lost its `<` and was
    // reported with only target/title/type; the model rewrote it unscoped. A tag
    // nested in an argument (an `<item>` of `<ids>`) is part of that value.
    let mut after = 0usize;
    for t in &call.toks {
        if t.start < after {
            continue;
        }
        let arg = t.arg.clone().unwrap_or_default();
        let plain_name = plain(t);
        match t.kind {
            Kind::Arg if !t.closing => {
                let closer = format!("</{arg}>");
                if let Some(v) = value(masked, t, &closer) {
                    after = masked[t.end..]
                        .find(&closer)
                        .map_or(0, |i| t.end + i + closer.len());
                    args.push((arg, Value::String(v)));
                }
            }
            Kind::Other if plain_name.is_some() => {
                let name = plain_name.unwrap_or_default();
                let closer = format!("</{name}>");
                if let Some(v) = value(masked, t, &closer) {
                    after = masked[t.end..]
                        .find(&closer)
                        .map_or(0, |i| t.end + i + closer.len());
                    args.push((name, Value::String(v)));
                }
            }
            Kind::Param => {
                if let Some(close) = close_re().find_at(masked, t.end) {
                    // python cuts a parameter at 200 chars with no ellipsis
                    let v: String = masked[t.end..close.start()]
                        .trim()
                        .chars()
                        .take(200)
                        .collect();
                    args.push((arg.clone(), Value::String(v)));
                    notes.push(format!(
                        "`{}` — one child tag per argument: <{arg}>…</{arg}>",
                        defang(&t.text)
                    ));
                }
            }
            Kind::Close if t.text != CLOSE => {
                notes.push(format!("`{}` — close with </call>", defang(&t.text)));
            }
            _ => {}
        }
    }
    let wrapped: Vec<String> = call
        .wrappers
        .iter()
        .map(|w| {
            format!(
                "line {} `{}` — drop the wrapper",
                newlines(text, w.start) + 1,
                defang(&w.text)
            )
        })
        .collect();
    notes.splice(0..0, wrapped);
    let (line, col, quoted, caret) = place(text, first);
    let mut out = vec![
        format!("[malformed call — nothing ran] line {line}, col {col}:"),
        format!("  {quoted}"),
        format!("  {caret} {why}"),
    ];
    let callish = call.toks.iter().filter(|t| t.kind != Kind::Other).count();
    if first.kind == Kind::Close && callish == 1 {
        if let Some(last) = out.last_mut() {
            last.push_str(" — delete it");
        }
    } else {
        out.push("write it as:".to_string());
        out.push(expected(&name, &args));
    }
    if !notes.is_empty() {
        out.push(format!("also: {}.", notes.join("; ")));
    }
    out.push(
        "A call is <call name=\"…\">, one child tag per argument, closed by </call>.".to_string(),
    );
    out.join("\n")
}

/// One report per call the answer wrote outside every `<call` envelope, in document
/// order (at most [`MAX_REPORTS`], then a count); empty when there is none. `ran` is
/// how many of the answer's calls were dispatched — said in each report, so the
/// model knows its well-formed calls are not to be written again.
#[must_use]
pub fn check(answer: &str, ran: usize) -> Vec<String> {
    let masked = mask(answer);
    let found = calls(&masked, tokens(&masked));
    let mut reports: Vec<String> = found
        .iter()
        .take(MAX_REPORTS)
        .map(|c| report(answer, &masked, c))
        .collect();
    if found.len() > MAX_REPORTS {
        if let Some(last) = reports.last_mut() {
            last.push_str(&format!(
                "\n({} more malformed calls not shown.)",
                found.len() - MAX_REPORTS
            ));
        }
    }
    if ran > 0 {
        let tail = if ran == 1 {
            "The other call of this answer ran — its reply is in this batch; do not write it \
             again."
                .to_string()
        } else {
            format!(
                "The other {ran} calls of this answer ran — their replies are in this batch; do \
                 not write them again."
            )
        };
        for r in &mut reports {
            r.push('\n');
            r.push_str(&tail);
        }
    }
    reports
}

#[cfg(test)]
#[path = "tests_call_check.rs"]
mod tests;
