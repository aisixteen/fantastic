//! The ONE call grammar: `<call name="…">` with bare child tags, args inline,
//! `bytes` verbatim, no JSON anywhere.
//!
//! A rule-for-rule port of python's
//! `bundled_agents/ai/ai_core/src/ai_core/tool_parse.py`, which is canonical.
//! The e2e harness parses THIS runtime's turns with THAT parser
//! (`e2e/helpers/watch.py` imports it whatever the runtime is), so the bar is
//! byte compatibility, not merely "a tag parser".
//!
//! ```text
//! <call name="send">
//! <target_id>notes</target_id>
//! <reply_to>self</reply_to>
//! <type>patch</type>
//! <edits><item><path>hello.py</path><hunks>2</hunks></item></edits>
//! <bytes>…verbatim: real newlines, real quotes, ends at the first </bytes>…
//! </bytes>
//! </call>
//! ```
//!
//! Why tags and not JSON: on shapes that carry a document, JSON with a verbatim
//! `bytes` cut fails often — escaped quotes, literal `\n`, a `\"}` inside content
//! ending the value early — while bare child tags need zero escapes. A `"`
//! boundary inside JSON fires the model's escaping reflex; a tag boundary carries
//! none.
//!
//! Nothing is repaired. A call that does not parse is reported with its fault
//! named and the minimal shape to send; the model fixes it.

use fantastic_bundle::{is_alnum, is_space, py_str, py_truthy};
use fantastic_kernel::{coerce_scalar, py_strip};
use regex::Regex;
use serde_json::{Map, Value};
use std::sync::OnceLock;

use super::shape::ToolCall;

/// The envelope opener. `<call` must be followed by whitespace and a `name`, so
/// `<caller>` is not one.
pub const OPEN: &str = "<call";
/// The envelope closer — unless it sits inside a `<bytes>` document.
pub const CLOSE: &str = "</call>";
/// Opens verbatim mode: everything to the FIRST [`BYTES_CLOSE`] is the document.
pub const BYTES_OPEN: &str = "<bytes>";
/// Ends verbatim mode. A document may not contain it.
pub const BYTES_CLOSE: &str = "</bytes>";

/// The tools. Any other `name` is refused with the shape — a verb is `<type>`,
/// never the tool name (a compression small models made under the JSON grammar).
pub const TOOLS: [&str; 4] = ["send", "done", "await", "inquiry_answer"];

/// Call-level fields of a `send`: where it goes, where the answer goes, what it
/// is for, and the kernel's per-item bounds. Everything else in the call is the
/// payload. The kernel item's caller-chosen execution name is `exec_id` HERE
/// (the router maps it onto the item's `id`): a bare `<id>` would collide with
/// the many verbs whose payload has an `id` (`delete_agent`, `update_agent`, …)
/// now that arguments are inline.
pub const ROUTING: [&str; 8] = [
    "target_id",
    "to",
    "re",
    "reply_to",
    "title",
    "timeout",
    "retries",
    "exec_id",
];

/// The block is a call the model MEANT and it does not parse; the string is the
/// fault, named for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed(pub String);

impl Malformed {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

// ── scanning ────────────────────────────────────────────────────────────────

/// Byte index of the `</call>` closing a call whose body starts at `start`,
/// skipping any `</call>` inside a `<bytes>` value; `None` if unterminated.
pub fn find_close(text: &str, start: usize) -> Option<usize> {
    let mut i = start;
    loop {
        let c = text[i..].find(CLOSE).map(|p| p + i);
        let b = text[i..].find(BYTES_OPEN).map(|p| p + i);
        match (c, b) {
            (_, None) => return c,
            (Some(c_at), Some(b_at)) if c_at < b_at => return Some(c_at),
            (_, Some(b_at)) => {
                let from = b_at + BYTES_OPEN.len();
                // An open document runs to the end: unterminated.
                let e = text[from..].find(BYTES_CLOSE).map(|p| p + from)?;
                i = e + BYTES_CLOSE.len();
            }
        }
    }
}

/// `(start, body_start, end)` for every `<call …>` envelope in document order.
/// `end` is `None` when the envelope is unterminated — the last thing in the
/// completion, so there is nothing after it to misread.
///
/// `<call` must be followed by whitespace AND a `name`, so `<caller>` is not an
/// envelope: a mention is stepped over by the OPENER ONLY, and never swallows
/// the real call that follows it.
pub fn envelopes(text: &str) -> Vec<(usize, usize, Option<usize>)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < text.len() {
        let Some(pos) = text[i..].find(OPEN).map(|p| p + i) else {
            return out;
        };
        let after = pos + OPEN.len();
        let rest = text.get(after..).unwrap_or("");
        if !rest.starts_with(is_ws) || !rest.trim_start_matches(is_ws).starts_with("name") {
            i = after;
            continue;
        }
        let end = find_close(text, after);
        out.push((pos, after, end));
        match end {
            None => return out,
            Some(e) => i = e + CLOSE.len(),
        }
    }
    out
}

/// A tag the model opened and has not closed yet: its byte span and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTag {
    /// Byte index of the `<`.
    pub start: usize,
    /// Byte index just past the `>`.
    pub end: usize,
    /// The tag's name, as written.
    pub name: String,
}

/// One `<call` envelope, with the tag it was written inside, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Byte index of the `<call` opener.
    pub start: usize,
    /// Byte index just past `<call` — the block starts here, at ` name="…">`.
    pub body: usize,
    /// Byte index of the `</call>` closer; `None` when unterminated.
    pub end: Option<usize>,
    /// The OUTERMOST tag still open when the envelope starts (`<tool_call>` in
    /// `<tool_call><call …`). `None` for a call at the top level.
    pub wrapper: Option<OpenTag>,
}

/// A tag: a name, then nothing, or attributes written `name="…"`, `name='…'` or
/// `name=value`. Prose such as `a<b and c>d` has bare words where an attribute
/// would be, so it is not a tag.
fn tag_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r#"<(/?)([A-Za-z_][\w.\-]*)((?:\s+[\w:.\-]+\s*=\s*(?:"[^"<>\n]*"|'[^'<>\n]*'|[^\s"'<>=`]+))*\s*)(/?)>"#,
        )
        .expect("tag")
    })
}

/// The answer with every code fence and code span blanked, for the tag walk only
/// (#188): `Vec<String>` in backticks, or a fenced `<div>`, is code, not markup, so
/// it opens no wrapper. Every byte keeps its offset (each char becomes spaces, a
/// newline stays), so positions in the masked text are positions in the answer. A
/// span needs its closing backticks of the same run length before a blank line; an
/// unmatched backtick is a plain character.
pub fn code_masked(text: &str) -> String {
    blank_spans(&blank_fences(text))
}

/// Blanks a string, keeping its newlines and its byte length.
fn blank(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c == '\n' {
                vec![c]
            } else {
                vec![' '; c.len_utf8()]
            }
        })
        .collect()
}

/// Blanks every fenced block: a line of three or more backticks or tildes opens it,
/// and a line of the same mark, at least as long, closes it. Unclosed, it runs to
/// the end of the text.
fn blank_fences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start_matches(' ');
        let mark = t.chars().next().filter(|c| *c == '`' || *c == '~');
        let run = mark.map_or(0, |m| t.chars().take_while(|c| *c == m).count());
        match fence {
            None if run >= 3 => {
                fence = mark.map(|m| (m, run));
                out.push_str(&blank(line));
            }
            None => out.push_str(line),
            Some((m, n)) => {
                out.push_str(&blank(line));
                if mark == Some(m) && run >= n && t[run..].trim().is_empty() {
                    fence = None;
                }
            }
        }
    }
    out
}

/// Blanks every code span: a run of backticks, through the next run of the SAME
/// length, with no blank line between.
fn blank_spans(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut i = 0usize;
    while let Some(off) = text[i..].find('`') {
        let start = i + off;
        let run = backticks_at(text, start);
        match closer(text, start + run, run) {
            Some(end) => {
                out.push_str(&text[last..start]);
                out.push_str(&blank(&text[start..end]));
                last = end;
                i = end;
            }
            None => i = start + run,
        }
    }
    out.push_str(&text[last..]);
    out
}

/// The number of backticks in the run at `at`.
fn backticks_at(text: &str, at: usize) -> usize {
    text[at..].bytes().take_while(|b| *b == b'`').count()
}

/// The byte index just past the closing run of `len` backticks, searching from
/// `from`; `None` when a blank line comes first or the text ends.
fn closer(text: &str, from: usize, len: usize) -> Option<usize> {
    let mut i = from;
    while let Some(off) = text[i..].find('`') {
        let p = i + off;
        if text[i..p].contains("\n\n") {
            return None;
        }
        let run = backticks_at(text, p);
        if run == len {
            return Some(p + run);
        }
        i = p + run;
    }
    None
}

/// Walks the tags of `text[from..to]`: an open tag is pushed; a close pops the most
/// recent open tag of its name, and whatever opened after it. A close with no open
/// tag of its name is stray and does nothing. A self-closing tag is not opened.
fn track(text: &str, from: usize, to: usize, open: &mut Vec<OpenTag>) {
    for c in tag_re().captures_iter(&text[from..to]) {
        let whole = c.get(0).expect("match");
        let name = c.get(2).map_or("", |g| g.as_str());
        if !c.get(1).map_or("", |g| g.as_str()).is_empty() {
            if let Some(i) = open.iter().rposition(|t| t.name == name) {
                open.truncate(i);
            }
        } else if c.get(4).map_or("", |g| g.as_str()).is_empty() {
            open.push(OpenTag {
                start: from + whole.start(),
                end: from + whole.end(),
                name: name.to_string(),
            });
        }
    }
}

/// Every envelope in document order, each with the tag it sits inside.
///
/// A call inside a wrapper is NOT dispatched. A wrapper is markup the model put
/// around the call (`<tool_call>`, `<function_calls>`, any name); the call is refused
/// and reported by [`crate::call_check`] until it is written at the top level. A
/// tag closed before the call (`<b>x</b> <call …>`) wraps nothing.
pub fn envelopes_in(text: &str) -> Vec<Envelope> {
    let masked = code_masked(text);
    let mut out = Vec::new();
    let mut open: Vec<OpenTag> = Vec::new();
    let mut at = 0usize;
    for (start, body, end) in envelopes(text) {
        track(&masked, at, start, &mut open);
        out.push(Envelope {
            start,
            body,
            end,
            wrapper: open.first().cloned(),
        });
        at = end.map_or(text.len(), |e| e + CLOSE.len());
    }
    out
}

/// The tags still open at the end of `text`, outermost first, code excluded (see
/// [`code_masked`]). Meant for a text with no envelope in it: an envelope's own tags
/// are not walked by [`envelopes_in`], and this does not skip them.
pub fn open_tags(text: &str) -> Vec<OpenTag> {
    let masked = code_masked(text);
    let mut open = Vec::new();
    track(&masked, 0, masked.len(), &mut open);
    open
}

/// The next `<name>` open tag at or after `from`: `(tag_start, body_start, name)`.
///
/// A tag name is a KEY, not an XML name: grant maps are keyed by agent ids and
/// the relative tokens (`$self`, `$parent`, `$subtree`), and the model writes
/// those as tags (`<$self>*</$self>`) — so `$`, `-` and `.` are letters here.
/// Mirrors python's
/// `<([A-Za-z_$][\w$.\-]*)>`.
fn next_tag(body: &str, from: usize) -> Option<(usize, usize, String)> {
    let mut search = from;
    while let Some(lt) = body[search..].find('<').map(|p| p + search) {
        let mut chars = body[lt + 1..].char_indices();
        let mut name = String::new();
        let mut closed_at: Option<usize> = None;
        if let Some((_, first)) = chars.next() {
            if first.is_ascii_alphabetic() || first == '_' || first == '$' {
                name.push(first);
                for (off, c) in chars {
                    if c == '>' {
                        closed_at = Some(lt + 1 + off);
                        break;
                    }
                    if is_alnum(c) || c == '_' || c == '$' || c == '.' || c == '-' {
                        name.push(c);
                    } else {
                        break;
                    }
                }
            }
        }
        if let Some(gt) = closed_at {
            return Some((lt, gt + 1, name));
        }
        search = lt + 1;
    }
    None
}

/// The closer of `<name>` opened just before `start`, honouring nesting of the
/// SAME name and skipping `<bytes>` values inside.
fn matching_close(body: &str, name: &str, start: usize) -> Option<usize> {
    let opener = format!("<{name}>");
    let closer = format!("</{name}>");
    let mut depth = 1usize;
    let mut i = start;
    while depth > 0 {
        let c = body[i..].find(&closer).map(|p| p + i)?;
        let o = body[i..].find(&opener).map(|p| p + i);
        let b = body[i..].find(BYTES_OPEN).map(|p| p + i);
        let next = [Some(c), o, b].into_iter().flatten().min()?;
        if Some(next) == b {
            let from = next + BYTES_OPEN.len();
            let e = body[from..].find(BYTES_CLOSE).map(|p| p + from)?;
            i = e + BYTES_CLOSE.len();
        } else if Some(next) == o {
            depth += 1;
            i = next + opener.len();
        } else {
            depth -= 1;
            if depth == 0 {
                return Some(c);
            }
            i = c + closer.len();
        }
    }
    None
}

/// `(name, raw)` for every child element of `body`, in order. Text between
/// elements must be whitespace. `bytes` runs verbatim to its FIRST closer.
pub fn elements(body: &str) -> Result<Vec<(String, &str)>, Malformed> {
    let mut out: Vec<(String, &str)> = Vec::new();
    let mut pos = 0usize;
    let tail: &str;
    loop {
        let Some((tag_start, body_start, name)) = next_tag(body, pos) else {
            tail = &body[pos..];
            break;
        };
        let between = &body[pos..tag_start];
        if !py_strip(between).is_empty() {
            return Err(Malformed::new(format!(
                "text outside a tag (`{}`) — every argument is its own \
                 `<name>…</name>` element",
                head40(py_strip(between))
            )));
        }
        let closer = format!("</{name}>");
        if name == "bytes" {
            let Some(end) = body[body_start..].find(&closer).map(|p| p + body_start) else {
                return Err(Malformed::new(
                    "`<bytes>` opened and never closed — close the document with \
                     `</bytes>`, then the call with `</call>`",
                ));
            };
            out.push((name, &body[body_start..end]));
            pos = end + closer.len();
            continue;
        }
        let Some(end) = matching_close(body, &name, body_start) else {
            return Err(Malformed::new(format!(
                "`<{name}>` opened and never closed with `{closer}`"
            )));
        };
        out.push((name, &body[body_start..end]));
        pos = end + closer.len();
    }
    if !py_strip(tail).is_empty() {
        return Err(Malformed::new(format!(
            "text after the last tag (`{}`) — every argument is its own \
             `<name>…</name>` element",
            head40(py_strip(tail))
        )));
    }
    Ok(out)
}

/// The first 40 CHARACTERS of a fault excerpt — python slices `[:40]` on a str,
/// which counts characters, and a byte slice would panic mid-codepoint.
fn head40(s: &str) -> String {
    s.chars().take(40).collect()
}

/// True when `raw` holds a tag at all — python's `_TAG.search(raw) is None` test.
fn has_tag(raw: &str) -> bool {
    next_tag(raw, 0).is_some()
}

/// One element's value: verbatim bytes, a nested dict/list, or a typed scalar.
///
/// `bytes` is the one value python delivers as real bytes. JSON has no such
/// variant, so it rides as the string it was written as — byte-identical, since
/// python's own value is that text encoded utf-8 — and the ROUTER lifts it out
/// of the JSON header into the item's binary channel.
pub fn value(name: &str, raw: &str) -> Result<Value, Malformed> {
    if name == "bytes" {
        return Ok(Value::String(raw.to_string()));
    }
    if !has_tag(raw) || !py_strip(raw).starts_with('<') {
        return Ok(coerce_scalar(py_strip(raw)));
    }
    let kids = elements(raw)?;
    if kids.is_empty() {
        return Ok(coerce_scalar(py_strip(raw)));
    }
    // A LIST is a tag whose children are all `<item>` — one element each, any
    // count, one element included. The same tag the reply notation renders, so
    // what the model reads is what it writes: a reply shows
    // `<allowed><item>…</item></allowed>`, the model writes it back, and a lone
    // child must not be read as a one-field object.
    if kids.iter().all(|(k, _)| k == "item") {
        let mut items = Vec::with_capacity(kids.len());
        for (k, v) in &kids {
            items.push(value(k, v)?);
        }
        return Ok(Value::Array(items));
    }
    // `<item>` beside OTHER tags is an ordinary field named item
    // (`<payload><type>buy</type><item>rocket</item></payload>` is not a list
    // error). Only a tag
    // whose children are ALL `<item>` is a list; a one-field object named `item`
    // is the single shape the notation cannot say.
    let mut out = Map::new();
    for (k, v) in &kids {
        if out.contains_key(k) {
            return Err(duplicate(k, name));
        }
        out.insert(k.clone(), value(k, v)?);
    }
    Ok(Value::Object(out))
}

fn duplicate(key: &str, outer: &str) -> Malformed {
    Malformed::new(format!(
        "duplicate `<{key}>` — a list is one tag whose children are all `<item>`: \
         `<{outer}><item>…</item><item>…</item></{outer}>`"
    ))
}

/// The call head: `name="<tool>"` and nothing else before `>`. Returns the tool
/// name and the byte index just past the `>`. Mirrors python's
/// `^\s*name\s*=\s*"([A-Za-z_][\w]*)"\s*>`.
fn head(block: &str) -> Option<(String, usize)> {
    let bytes = block.as_bytes();
    let mut i = skip_ws(block, 0);
    if !block[i..].starts_with("name") {
        return None;
    }
    i = skip_ws(block, i + 4);
    if bytes.get(i) != Some(&b'=') {
        return None;
    }
    i = skip_ws(block, i + 1);
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    let start = i;
    let mut chars = block[i..].char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return None,
    }
    let mut end = i + 1;
    for (off, c) in chars {
        // python `\w`: `_` plus `str.isalnum()` (Lu/Ll/Lt/Lm/Lo + the numeric
        // categories). `char::is_alphanumeric` also admits Other_Alphabetic
        // (U+093E, U+0345, U+24B6), so `<k_नाम>`/`<xⒶ>` dispatched here and were
        // `text after the last tag` to python.
        if is_alnum(c) || c == '_' {
            end = i + off + c.len_utf8();
        } else {
            break;
        }
    }
    if bytes.get(end) != Some(&b'"') {
        return None;
    }
    let after = skip_ws(block, end + 1);
    if bytes.get(after) != Some(&b'>') {
        return None;
    }
    Some((block[start..end].to_string(), after + 1))
}

/// PYTHON'S WHITESPACE, not rust's: `\s` in a python `str` pattern is
/// `str.isspace()`, which includes the information separators `\x1c`–`\x1f`. So
/// `<call\x1cname="send">` IS an envelope to python and was not to rust.
fn is_ws(c: char) -> bool {
    is_space(c)
}

fn skip_ws(s: &str, from: usize) -> usize {
    let mut i = from;
    for c in s[from.min(s.len())..].chars() {
        if is_ws(c) {
            i += c.len_utf8();
        } else {
            break;
        }
    }
    i
}

/// Parse the text between `<call` and `</call>` into a [`ToolCall`].
///
/// `Err` for a call-shaped block that does not parse, with the fault named.
/// `Ok(None)` ONLY for a block that is not a call at all (no `name="…">` head) —
/// the model mentioning `<call` in prose.
pub fn parse_call(block: &str) -> Result<Option<ToolCall>, Malformed> {
    let Some((name, body_at)) = head(block) else {
        let head_text = py_strip(block.split_once('>').map_or(block, |(h, _)| h));
        if !head_text.starts_with("name=") {
            return Ok(None);
        }
        return Err(Malformed::new(
            "the call head must be exactly `name=\"<tool>\"` — nothing else before `>`",
        ));
    };
    if !TOOLS.contains(&name.as_str()) {
        return Err(Malformed::new(format!(
            "`{name}` is not a tool — the tools are {}. A VERB goes in `<type>`: \
             `<call name=\"send\"><target_id>…</target_id><type>{name}</type></call>`",
            TOOLS.join(", ")
        )));
    }
    let mut fields = Map::new();
    for (k, raw) in elements(&block[body_at..])? {
        if fields.contains_key(&k) {
            return Err(duplicate(&k, &k));
        }
        let v = value(&k, raw)?;
        fields.insert(k, v);
    }
    if name != "send" {
        return Ok(Some(ToolCall::new(name, fields)));
    }
    // python `fields.pop(...)` keeps the SURVIVORS in insertion order. serde_json
    // `Map::remove` is `swap_remove` under `preserve_order` — it drags the LAST key
    // into the hole, so `{target_id, type, pid, offset}` became `{offset, type, pid}`.
    let target = fields.shift_remove("target_id");
    let to = normalize_to(fields.shift_remove("to"));
    let target = target
        .as_ref()
        .and_then(Value::as_str)
        .map(py_strip)
        .filter(|t| !t.is_empty());
    if target.is_none() && !to.as_ref().is_some_and(py_truthy) {
        return Err(Malformed::new(
            "a send names its target: `<target_id>agent id</target_id>`, or several \
             recipients as `<to><item>id</item><item>id</item></to>`",
        ));
    }
    let mut args = Map::new();
    if let Some(t) = target {
        args.insert("target_id".to_string(), Value::String(t.to_string()));
    }
    // python `if to:` — inserted whenever TRUTHY, whatever shape it has.
    if let Some(list) = to.filter(py_truthy) {
        args.insert("to".to_string(), list);
    }
    for key in &ROUTING[2..] {
        if let Some(v) = fields.shift_remove(*key) {
            args.insert((*key).to_string(), v);
        }
    }
    args.insert("payload".to_string(), Value::Object(fields));
    Ok(Some(ToolCall::new("send", args)))
}

/// `to` as python leaves it — and python leaves MOST of it alone.
///
/// A non-blank string becomes a one-element list; a list keeps its non-blank
/// `str(t).strip()` members; EVERY OTHER VALUE IS KEPT RAW (`<to>42</to>` sends
/// `{to: 42}` on to routing, which answers with its own no-target reply the
/// model can read). Only a python-FALSY `to` with no target is the malformed
/// refusal — `<to>0</to>`, `<to></to>`.
fn normalize_to(raw: Option<Value>) -> Option<Value> {
    match raw {
        Some(Value::String(s)) => {
            let t = py_strip(&s);
            if t.is_empty() {
                // python leaves the blank string as itself; it is falsy either way.
                Some(Value::String(s.clone()))
            } else {
                Some(Value::Array(vec![Value::String(t.to_string())]))
            }
        }
        Some(Value::Array(items)) => Some(Value::Array(
            items
                .iter()
                .map(scalar_text)
                .filter(|s| !s.is_empty())
                .map(Value::String)
                .collect(),
        )),
        other => other,
    }
}

/// One recipient as text — python's `str(t).strip()` over whatever the notation
/// produced: `True`, `1e+20`, `{'a': 'x'}`, `['y']`. `Value::to_string`'s JSON
/// (`true`, `1e20`, `{"a":"x"}`) was a second dialect for the same list.
fn scalar_text(v: &Value) -> String {
    py_strip(&py_str(v)).to_string()
}
