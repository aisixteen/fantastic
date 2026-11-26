//! THE `hermes` TYPE (#766, 1.4): the call form the Qwen 2.5/3 and Hermes chat templates
//! train, one JSON object between `<tool_call>` and `</tool_call>`.
//!
//! - Inbound ([`Renderer`]): every parseable internal envelope becomes
//!   `<tool_call>\n{"name": N, "arguments": {..}}\n</tool_call>`; each reply block keeps
//!   its `[reply to ..]` tag line and its body goes, bytes unchanged, inside
//!   `<tool_response>\n..\n</tool_response>`; the default type's placeholder form and its
//!   two call-form notices are swapped by exact match (Q22); everything else is copied
//!   byte for byte. An envelope that does not parse and a stored cut marker stay as they
//!   are.
//! - Outbound ([`Reader`]): prose and reasoning stream live; a call is held from
//!   `<tool_call>` to its first `</tool_call>` outside a JSON string (a closer inside a
//!   string, or inside a raw block after the JSON, is value bytes: [`CallBody`]),
//!   then `serde_json` reads it and it comes out as one [`Out::Call`] in the internal
//!   form ([`render_call`]). One repair only: JSON that `serde_json` does not read and
//!   that `jsonrepair-rs` completes by adding closing brackets and braces at its end and
//!   nothing else ([`closed`]; the JSON alone when a `<bytes>` block follows it) is read so, and an [`Out::Repaired`] goes before its call
//!   (the turn's `repaired` trace event). Several calls in one block are [`Out::Bad`]
//!   with [`REPORT_MULTI`], each call written as its own block. A call that does not
//!   read, a JSON call with no tags, a closer with no opener and a call in the default
//!   form are each [`Out::Bad`] with this type's report (`res/hermes/report_*.txt`). A
//!   stream that ends inside a call is [`Out::Open`], after the internal head that could
//!   be read from it.
//!
//! The template the layout is written from, its commit and its licence are recorded in
//! `res/hermes/source.txt`. The JSON writer writes what `json.dumps(.., ensure_ascii=False)`
//! writes, as the template's `tojson` does: `", "` and `": "` between items, no trailing
//! space, non-ASCII as it is. Leaves are text: the internal form types nothing
//! (`tool_parse::typed` does, at the router), so a number, `true` or `false` a model
//! writes becomes that literal text (`arbitrary_precision` keeps a number's text); a
//! `null` becomes the empty tag, read as null where the schema allows null.
//!
//! The cut head needs the name and the target out of an incomplete JSON text. No
//! maintained partial-JSON crate says which strings were complete (each completes the
//! text into a whole value; `res/hermes/source.txt`), so [`cut_head`] is a prefix scan
//! limited to `name` and `arguments.routing.target_id`.

use serde_json::{Map, Value};

use super::stream::{Body, Piece, Scanner};
use super::{default, fill, Markers, Out, Outbound, Reader, Renderer, Section};
use crate::history_view::{parse_reply_tag, reply_blocks};
use crate::think::without_thinking;
use crate::tool_parse::grammar::{code_masked, envelopes, envelopes_in, parse_call, ROUTING};
use crate::tool_parse::{extract_tool_calls, render_call, BYTES_CLOSE, BYTES_OPEN, CLOSE, TOOLS};

// ── resources (res/hermes, #760) ─────────────────────────────────────────────

/// The type's name in the rule list.
pub(crate) const NAME: &str = "hermes";
/// How to reach another agent, in this type's form, with the `<tools>` block (Q39).
pub(crate) const HOWTO_SEND: &str = include_str!("res/hermes/howto_send.txt");
/// The working rules in this type's call form (`<tool_call>` blocks).
pub(crate) const HOWTO_WORKING: &str = include_str!("res/hermes/howto_working.txt");
/// What `reflect` is, in this type's form: `{detail}`, its declaration's detailed text.
pub(crate) const HOWTO_REFLECT: &str = include_str!("res/hermes/howto_reflect.txt");
/// The context window in this type's form: the default text's facts, no other.
pub(crate) const HOWTO_CONTEXT: &str = include_str!("res/hermes/howto_context.txt");
/// Titling calls in this type's form.
pub(crate) const HOWTO_TITLES: &str = include_str!("res/hermes/howto_titles.txt");
/// Replies as rows in this type's form.
pub(crate) const HOWTO_ASYNC: &str = include_str!("res/hermes/howto_async.txt");
/// The markers: a JSON line inside `<tool_response>` (the keep cue a `<tool_call>`), each
/// naming its row's id and the `recall` call.
pub(crate) const MARKERS: Markers = Markers {
    folded_reply: include_str!("res/hermes/marker_folded_reply.txt"),
    folded_reply_kept: include_str!("res/hermes/marker_folded_reply_kept.txt"),
    folded_recall: include_str!("res/hermes/marker_folded_recall.txt"),
    folded_recall_kept: include_str!("res/hermes/marker_folded_recall_kept.txt"),
    thought: include_str!("res/hermes/marker_thought.txt"),
    stub: include_str!("res/hermes/marker_stub.txt"),
    image_anchor: include_str!("res/hermes/marker_image_anchor.txt"),
    image_alt: include_str!("res/hermes/marker_image_alt.txt"),
    keep_cue: include_str!("res/hermes/marker_keep_cue.txt"),
    notice_head: super::NOTICE_HEAD,
    notice_summary: super::NOTICE_SUMMARY,
    notice_omitted: super::NOTICE_OMITTED,
    notice_stripped: super::NOTICE_STRIPPED,
    notice_tail: include_str!("res/hermes/notice_tail.txt"),
};
/// The call form, in one sentence: swapped for [`default::FORM`].
pub(crate) const FORM: &str = include_str!("res/hermes/form.txt");
/// The placeholder form of a call opener: swapped for [`default::FORM_OPEN`] (Q22).
pub(crate) const FORM_OPEN: &str = include_str!("res/hermes/form_open.txt");
/// The shape a parser refusal shows: swapped for [`default::HINT`].
pub(crate) const HINT: &str = include_str!("res/hermes/report_hint.txt");
/// The text inside the tags is not one JSON object: `{error}`.
pub(crate) const REPORT_JSON: &str = include_str!("res/hermes/report_json.txt");
/// The object is not `{"name", "arguments"}`.
pub(crate) const REPORT_SHAPE: &str = include_str!("res/hermes/report_shape.txt");
/// A name that is not a tool: `{name}`, `{tools}`.
pub(crate) const REPORT_NOT_A_TOOL: &str = include_str!("res/hermes/report_not_a_tool.txt");
/// A key beside `routing` and `payload` in a send: `{key}`.
pub(crate) const REPORT_SEND_ARGS: &str = include_str!("res/hermes/report_send_args.txt");
/// A send with no `routing` object.
pub(crate) const REPORT_ROUTING: &str = include_str!("res/hermes/report_routing.txt");
/// A routing key that is not a routing field: `{key}`, `{fields}`.
pub(crate) const REPORT_ROUTING_KEY: &str = include_str!("res/hermes/report_routing_key.txt");
/// A `payload` that is not an object.
pub(crate) const REPORT_PAYLOAD: &str = include_str!("res/hermes/report_payload.txt");
/// A send that names no recipient.
pub(crate) const REPORT_NO_TARGET: &str = include_str!("res/hermes/report_no_target.txt");
/// Anything between the JSON and the closer that is not one block of a send (S24).
pub(crate) const REPORT_BLOCK: &str = include_str!("res/hermes/report_block.txt");
/// Several calls inside one `<tool_call>`: `{calls}` the model's own calls, each in its
/// own block; `{careful}` empty, or a newline and the `careful:` line of each call whose
/// destructive command the form leaves out (#255).
pub(crate) const REPORT_MULTI: &str = include_str!("res/hermes/report_multi.txt");
/// A JSON call with no tags, or a closer with no opener (S21).
pub(crate) const REPORT_NO_OPENER: &str = include_str!("res/hermes/report_no_opener.txt");
/// A call written in the default form (S9).
pub(crate) const REPORT_FOREIGN: &str = include_str!("res/hermes/report_foreign.txt");
/// A `stop` with no non-empty `reason` (S25).
pub(crate) const REPORT_STOP_REASON: &str = include_str!("res/hermes/report_stop_reason.txt");
/// The call read, but its internal form would not: `{error}`.
pub(crate) const REPORT_OTHER: &str = include_str!("res/hermes/report_other.txt");
/// The reason of the `stop` the harness writes after the last malformed attempt: `{n}`,
/// the number of attempts (1.8). It holds no text of any attempt.
pub(crate) const STOP_REASON: &str = include_str!("res/hermes/stop_reason.txt");

/// The call opener and closer.
pub(crate) const CALL_OPEN: &str = "<tool_call>";
/// See [`CALL_OPEN`].
pub(crate) const CALL_CLOSE: &str = "</tool_call>";
/// A reply body is wrapped in these, each on its own line (Q13).
const RESPONSE_OPEN: &str = "<tool_response>";
const RESPONSE_CLOSE: &str = "</tool_response>";

/// The default type's texts this type swaps by exact match, each for its own: the
/// parser's hint, the form sentence, the placeholder opener. At one position the
/// longer one wins.
fn swaps() -> [(&'static str, &'static str); 3] {
    [
        (default::HINT, HINT),
        (default::FORM, FORM),
        (default::FORM_OPEN, FORM_OPEN),
    ]
}

/// This type's text of a section.
pub(crate) fn texts(s: Section) -> &'static str {
    match s {
        Section::Send => HOWTO_SEND,
        Section::Reflect => HOWTO_REFLECT,
        Section::Context => HOWTO_CONTEXT,
        Section::Titles => HOWTO_TITLES,
        Section::Async => HOWTO_ASYNC,
        Section::Working => HOWTO_WORKING,
        Section::StopReason => STOP_REASON,
        shared => super::shared_text(shared).expect("a shared section"),
    }
}

/// A value as one JSON string, as the call form writes it.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_str(s, &mut out);
    out
}

// ── the pair ─────────────────────────────────────────────────────────────────

/// The `hermes` type: both halves, one value.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hermes;

impl Renderer for Hermes {
    fn name(&self) -> &'static str {
        NAME
    }

    fn rev(&self) -> u32 {
        1
    }

    fn render(&self, text: &str) -> String {
        render_text(text)
    }
}

impl Reader for Hermes {
    fn outbound(&self) -> Box<dyn Outbound> {
        Box::new(Stream::new(self.tools()))
    }

    fn tools(&self) -> &'static [&'static str] {
        &TOOLS
    }
}

// ── inbound ──────────────────────────────────────────────────────────────────

/// One replacement: the bytes `start..stop` of the text and what goes in their place.
struct Sub {
    start: usize,
    stop: usize,
    with: String,
    /// A call (an envelope), not a swapped default text.
    call: bool,
}

/// Every exact occurrence of a swapped default text ([`swaps`]), left to right, the
/// longer one first at one position: `(start, stop, replacement)`.
fn placeholders(text: &str) -> Vec<(usize, usize, &'static str)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        let next = swaps()
            .into_iter()
            .filter_map(|(from, to)| text[at..].find(from).map(|i| (at + i, from, to)))
            .min_by(|a, b| a.0.cmp(&b.0).then(b.1.len().cmp(&a.1.len())));
        let Some((i, from, to)) = next else {
            return out;
        };
        out.push((i, i + from.len(), to));
        at = i + from.len();
    }
}

/// What the render replaces, in text order: every terminated envelope that parses, as
/// its hermes call text, and every swapped default text outside them. The envelopes are
/// found with the swapped texts masked: the placeholder `<call name="…">` opens an
/// envelope to the scanner, which would swallow the real call after it. An envelope
/// inside another one's value is part of that value ([`envelopes`] pairs it), so it is
/// never rendered on its own (H10).
fn subs(text: &str) -> Vec<Sub> {
    let held = placeholders(text);
    let mut masked = text.as_bytes().to_vec();
    for &(a, b, _) in &held {
        for c in &mut masked[a..b] {
            if *c == b'<' {
                *c = b' ';
            }
        }
    }
    // only ASCII `<` was replaced by an ASCII space: still UTF-8, same offsets
    let masked = String::from_utf8(masked).expect("a masked text is UTF-8");
    let mut out: Vec<Sub> = envelopes_in(&masked)
        .into_iter()
        .filter_map(|env| {
            let end = env.end?;
            let call = parse_call(&text[env.body..end]).ok().flatten()?;
            Some(Sub {
                start: env.start,
                stop: end + CLOSE.len(),
                with: write_call(&call.name, &call.arguments),
                call: true,
            })
        })
        .collect();
    for (a, b, to) in held {
        if !out.iter().any(|s| s.call && s.start <= a && a < s.stop) {
            out.push(Sub {
                start: a,
                stop: b,
                with: to.to_string(),
                call: false,
            });
        }
    }
    out.sort_by_key(|s| s.start);
    out
}

/// The message text in the hermes form. Pure: the output is a function of `text` alone.
fn render_text(text: &str) -> String {
    let subs = subs(text);
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for (i, (from, to)) in blocks(text, &subs).into_iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        let block = &text[from..to];
        let tag_end = block.find('\n').unwrap_or(block.len());
        let tag_free = !subs
            .iter()
            .any(|s| s.call && s.start >= from && s.start < from + tag_end);
        if parse_reply_tag(block).is_some() && tag_free {
            // the tag line is kept; the body, bytes unchanged (its envelopes rendered),
            // goes inside the response tags
            segment(text, from, from + tag_end, &subs, &mut out);
            out.push('\n');
            out.push_str(RESPONSE_OPEN);
            out.push('\n');
            let body = (from + tag_end + 1).min(to);
            segment(text, body, to, &subs, &mut out);
            out.push('\n');
            out.push_str(RESPONSE_CLOSE);
        } else {
            segment(text, from, to, &subs, &mut out);
        }
    }
    out
}

/// The text's reply blocks as byte ranges ([`reply_blocks`]: a blank line before a
/// `[reply to ..]` tag starts one), a split inside a replacement undone.
fn blocks(text: &str, subs: &[Sub]) -> Vec<(usize, usize)> {
    let base = text.as_ptr() as usize;
    let mut out: Vec<(usize, usize)> = Vec::new();
    for b in reply_blocks(text) {
        let start = b.as_ptr() as usize - base;
        let end = start + b.len();
        match out.last_mut() {
            Some(last) if subs.iter().any(|s| s.start < last.1 && last.1 < s.stop) => {
                last.1 = end;
            }
            _ => out.push((start, end)),
        }
    }
    out
}

/// `text[from..to]` with its replacements made.
fn segment(text: &str, from: usize, to: usize, subs: &[Sub], out: &mut String) {
    let mut at = from;
    for s in subs.iter().filter(|s| s.start >= from && s.stop <= to) {
        out.push_str(&text[at..s.start]);
        out.push_str(&s.with);
        at = s.stop;
    }
    out.push_str(&text[at..to]);
}

/// One call in the hermes form. A send's routing keys go in [`ROUTING`] order, its
/// payload keys in stored order; a non-send call's fields go flat in `arguments`.
fn write_call(name: &str, arguments: &Map<String, Value>) -> String {
    let mut args = Map::new();
    let mut body: Option<String> = None;
    if name == "send" {
        let mut routing = Map::new();
        for key in ROUTING {
            if let Some(v) = arguments.get(key) {
                routing.insert(key.to_string(), v.clone());
            }
        }
        let mut payload = match arguments.get("payload") {
            Some(Value::Object(p)) => p.clone(),
            _ => Map::new(),
        };
        // EXPERIMENTAL (Q12): the body leaves the JSON. This departs from the trained
        // Hermes format, where a body is an inline JSON string: models mis-escape long
        // code inside JSON strings (quotes, backslashes, newlines), so render writes the
        // body after the JSON line inside the call as a raw block, `<bytes>BODY</bytes>`,
        // byte for byte, nothing escaped; it ends at the first `</bytes>`, as the internal
        // `<bytes>` does. The reader takes both forms: this raw block, and the trained
        // inline `payload.bytes` string. Render writes only the raw block. To be measured
        // against the inline form. Pinned by R15.
        if let Some(Value::String(b)) = payload.get("bytes") {
            body = Some(b.clone());
            payload.shift_remove("bytes");
        }
        args.insert("routing".to_string(), Value::Object(routing));
        args.insert("payload".to_string(), Value::Object(payload));
    } else {
        args = arguments.clone();
    }
    let mut call = Map::new();
    call.insert("name".to_string(), Value::String(name.to_string()));
    call.insert("arguments".to_string(), Value::Object(args));
    let mut out = String::new();
    out.push_str(CALL_OPEN);
    out.push('\n');
    write_json(&Value::Object(call), &mut out);
    out.push('\n');
    if let Some(b) = body {
        out.push_str(BYTES_OPEN);
        out.push_str(&b);
        out.push_str(BYTES_CLOSE);
        out.push('\n');
    }
    out.push_str(CALL_CLOSE);
    out
}

/// The one JSON writer: `json.dumps(v, ensure_ascii=False)` byte for byte, keys in the
/// map's order (an ordered map, `preserve_order`).
pub(crate) fn write_json(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_str(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_json(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_str(k, out);
                out.push_str(": ");
                write_json(x, out);
            }
            out.push('}');
        }
    }
}

/// A JSON string as python writes it with `ensure_ascii=False`.
fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ── outbound ─────────────────────────────────────────────────────────────────

/// JSON whitespace.
fn is_json_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// The first `needle` in `hay` at or after `from`.
fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || hay.len() - from < needle.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| from + i)
}

/// Where the reader of a held call stands.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Looking for the first closer outside a JSON string.
    #[default]
    Close,
    /// Inside the raw block that the text before the first closer opened: to its first
    /// `</bytes>`.
    Block,
    /// After the block: to the next closer.
    AfterBlock,
}

/// The end of one held hermes call after `<tool_call>`: its first `</tool_call>` that is
/// not inside a JSON string ([`in_string`]: a closer there is value bytes, S20), so a
/// call whose JSON lacks its closing braces still ends there and is read or reported.
/// One case reads on: when the text before that closer is one JSON value (read by
/// `serde_json`) and then a `<bytes>` block not yet closed, the closer is the block's
/// bytes; the call ends at the first closer after the block's first `</bytes>` (R3,
/// S20). The end depends on the text alone, never on how it was split.
#[derive(Debug, Default)]
pub(crate) struct CallBody {
    seen: String,
    pos: usize,
    phase: Phase,
}

impl Body for CallBody {
    fn feed(&mut self, more: &str) -> Option<usize> {
        let before = self.seen.len();
        self.seen.push_str(more);
        self.step().map(|end| end - before)
    }
}

impl CallBody {
    /// The byte index in `seen` just past the call's closer, once it is known.
    fn step(&mut self) -> Option<usize> {
        let b = self.seen.as_bytes();
        let close = CALL_CLOSE.as_bytes();
        loop {
            let needle = match self.phase {
                Phase::Block => BYTES_CLOSE.as_bytes(),
                Phase::Close | Phase::AfterBlock => close,
            };
            let Some(i) = find_bytes(b, needle, self.pos) else {
                // a needle split at the end of `seen` is found on the next feed
                self.pos = self.pos.max(b.len().saturating_sub(needle.len() - 1));
                return None;
            };
            match self.phase {
                Phase::Close if in_string(&self.seen[..i]) => self.pos = i + close.len(),
                Phase::Close => match open_block(&self.seen[..i]) {
                    Some(at) => {
                        self.pos = at;
                        self.phase = Phase::Block;
                    }
                    None => return Some(i + close.len()),
                },
                Phase::Block => {
                    self.pos = i + needle.len();
                    self.phase = Phase::AfterBlock;
                }
                Phase::AfterBlock => return Some(i + close.len()),
            }
        }
    }
}

/// Whether `head` (a held call's text before a closer) ends inside a JSON string, as
/// `serde_json` reads it: its text, from its first byte that is not whitespace, stops
/// at its end while a string is being read. A closer there is the string's bytes (S20).
fn in_string(head: &str) -> bool {
    let lead = head.trim_start_matches(|c: char| c.is_ascii() && is_json_ws(c as u8));
    match serde_json::from_str::<serde::de::IgnoredAny>(lead) {
        Err(e) => e.is_eof() && e.to_string().starts_with("EOF while parsing a string"),
        Ok(_) => false,
    }
}

/// When `head` (a held call's text before its first closer) is one JSON value, then
/// whitespace, then `<bytes>` with no `</bytes>` after it: the index just past that
/// `<bytes>`. The JSON is read by `serde_json`.
fn open_block(head: &str) -> Option<usize> {
    let mut it = serde_json::Deserializer::from_str(head).into_iter::<serde::de::IgnoredAny>();
    it.next()?.ok()?;
    let end = it.byte_offset();
    let rest = &head[end..];
    let lead = rest.len()
        - rest
            .trim_start_matches(|c: char| c.is_ascii() && is_json_ws(c as u8))
            .len();
    let at = end + lead;
    let body = head[at..].strip_prefix(BYTES_OPEN)?;
    (!body.contains(BYTES_CLOSE)).then_some(at + BYTES_OPEN.len())
}

/// The streaming state of one `hermes` answer.
struct Stream {
    scan: Scanner<CallBody>,
    /// The text forwarded so far, outside every call (a newline where a call stood).
    prose: String,
    /// A `Bad` or an `Open` was given: nothing more of this answer is forwarded.
    over: bool,
    tools: &'static [&'static str],
}

impl Stream {
    fn new(tools: &'static [&'static str]) -> Self {
        Self {
            scan: Scanner::new(CALL_OPEN),
            prose: String::new(),
            over: false,
            tools,
        }
    }

    /// The scanner's pieces as `Out` values: text forwarded, a held call read.
    fn map(&mut self, pieces: Vec<Piece>) -> Vec<Out> {
        let mut out = Vec::new();
        for p in pieces {
            match p {
                Piece::Text(t) => {
                    self.prose.push_str(&t);
                    out.push(Out::Text(t));
                }
                Piece::Call(raw) => {
                    let read =
                        prose_fault(&self.prose).map_or_else(|| read_call(&raw, self.tools), Err);
                    match read {
                        Ok(Read { text, added }) => {
                            self.prose.push('\n');
                            if let Some(added) = added {
                                out.push(Out::Repaired { added });
                            }
                            out.push(Out::Call { text });
                        }
                        Err(report) => {
                            self.over = true;
                            out.push(Out::Bad { report });
                            return out;
                        }
                    }
                }
                Piece::Open(raw) => {
                    self.over = true;
                    let (head, name, target) = cut_head(&raw[CALL_OPEN.len()..]);
                    if !head.is_empty() {
                        out.push(Out::Text(head));
                    }
                    out.push(Out::Open { name, target });
                    return out;
                }
            }
        }
        out
    }
}

impl Outbound for Stream {
    fn feed(&mut self, delta: &str) -> Vec<Out> {
        if self.over {
            return Vec::new();
        }
        let pieces = self.scan.feed(delta);
        self.map(pieces)
    }

    fn finish(&mut self) -> Vec<Out> {
        if self.over {
            return Vec::new();
        }
        let pieces = self.scan.finish();
        let mut out = self.map(pieces);
        if !self.over {
            if let Some(report) = prose_fault(&self.prose) {
                self.over = true;
                out.push(Out::Bad { report });
            }
        }
        out
    }
}

/// A call the prose outside the calls holds, reasoning and code left out: one in the
/// default form (S9), a JSON call with no tags, or a closer with no opener (S21).
fn prose_fault(prose: &str) -> Option<String> {
    let masked = code_masked(&without_thinking(prose));
    if !envelopes(&masked).is_empty() {
        return Some(REPORT_FOREIGN.to_string());
    }
    if masked.contains(CALL_CLOSE) || json_call_re().is_match(&masked) {
        return Some(REPORT_NO_OPENER.to_string());
    }
    None
}

fn json_call_re() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r#"\{\s*"name"\s*:\s*"[^"]*"\s*,\s*"arguments"\s*:"#).expect("json call")
    })
}

/// A held call read: its internal call text, and the closers a repair added to its
/// JSON ([`closed`]), if it was repaired.
struct Read {
    text: String,
    added: Option<String>,
}

/// One held call, opener and closer included, as internal call text; `Err` is the
/// report. When `serde_json` does not read the text inside the tags, it is read once
/// more as [`closed`] repairs it, if it does.
fn read_call(raw: &str, tools: &[&str]) -> Result<Read, String> {
    let inner = &raw[CALL_OPEN.len()..raw.len() - CALL_CLOSE.len()];
    let lead = inner.trim_start_matches(|c: char| c.is_ascii() && is_json_ws(c as u8));
    if lead.starts_with(BYTES_OPEN) {
        return Err(REPORT_BLOCK.to_string());
    }
    let reads = matches!(
        serde_json::Deserializer::from_str(lead)
            .into_iter::<serde::de::IgnoredAny>()
            .next(),
        Some(Ok(_))
    );
    if !reads {
        // the JSON alone is repaired: a `<bytes>` block after it is the body, kept as
        // written
        let json = json_part(lead);
        if let Some((fixed, added)) = closed(json) {
            let have = json.trim_end_matches(|c: char| c.is_ascii() && is_json_ws(c as u8));
            let whole = format!("{fixed}{}", &lead[have.len()..]);
            return read_lead(&whole, tools).map(|text| Read {
                text,
                added: Some(added),
            });
        }
    }
    read_lead(lead, tools).map(|text| Read { text, added: None })
}

/// The JSON of `lead`: the text before its first `<bytes>` outside a JSON string, or
/// all of it when no block follows.
pub(super) fn json_part(lead: &str) -> &str {
    let b = lead.as_bytes();
    let (mut in_string, mut escaped) = (false, false);
    for (i, &c) in b.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
        } else if c == b'"' {
            in_string = true;
        } else if b[i..].starts_with(BYTES_OPEN.as_bytes()) {
            return &lead[..i];
        }
    }
    lead
}

/// `lead` (the text inside the tags, from its first byte that is not whitespace) as
/// `jsonrepair-rs` repairs it, when the repair only added closing brackets and braces
/// at its end (whitespace after them is dropped) and the result reads as JSON:
/// `(repaired text, the closers added)`. Any other repair is refused: a quote, a comma,
/// a value or a key the model did not write is never put in its call.
fn closed(lead: &str) -> Option<(String, String)> {
    let ws = |c: char| c.is_ascii() && is_json_ws(c as u8);
    let have = lead.trim_end_matches(ws);
    let fixed = jsonrepair_rs::jsonrepair(lead).ok()?;
    let added = fixed.strip_prefix(have)?.trim_end_matches(ws);
    if added.is_empty() || !added.bytes().all(|c| c == b'}' || c == b']') {
        return None;
    }
    let text = format!("{have}{added}");
    serde_json::from_str::<serde::de::IgnoredAny>(&text).ok()?;
    Some((text, added.to_string()))
}

/// [`read_call`] from the text inside the tags, leading whitespace gone. The format is
/// strict, one call per block: several calls in one are [`REPORT_MULTI`], with each
/// call that reads written as its own block; the first that does not read is reported
/// instead, with its own report.
fn read_lead(lead: &str, tools: &[&str]) -> Result<String, String> {
    let Some(parts) = calls_in(lead) else {
        return read_one(lead, tools).map(|(_, _, text)| text);
    };
    let mut careful = String::new();
    let mut blocks = Vec::with_capacity(parts.len());
    for part in parts {
        let (name, mut internal, _) = read_one(part, tools)?;
        if let Some(Value::Object(payload)) = internal.get("payload") {
            let pairs: Vec<(String, Value)> = payload
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if let Some((line, shown)) = crate::call_check::left_out(&pairs) {
                careful.push('\n');
                careful.push_str(&line);
                internal.insert(
                    "payload".to_string(),
                    Value::Object(shown.into_iter().collect()),
                );
            }
        }
        blocks.push(write_call(&name, &internal));
    }
    Err(fill(
        REPORT_MULTI,
        &[
            ("careful", careful.as_str()),
            ("calls", blocks.join("\n").as_str()),
        ],
    ))
}

/// The calls of `lead` (the text inside one pair of tags, leading whitespace gone) when
/// it holds more than one: each JSON value with the `<bytes>` block after it, if one
/// follows, as written. A part that does not read as JSON runs to the end. `None` for
/// one call, or when something other than a further JSON object follows a call (that
/// one call's own report says what).
fn calls_in(lead: &str) -> Option<Vec<&str>> {
    let ws = |c: char| c.is_ascii() && is_json_ws(c as u8);
    let mut parts = Vec::new();
    let mut at = 0usize;
    loop {
        let mut it =
            serde_json::Deserializer::from_str(&lead[at..]).into_iter::<serde::de::IgnoredAny>();
        let Some(Ok(_)) = it.next() else {
            parts.push(&lead[at..]);
            break;
        };
        let mut end = at + it.byte_offset();
        let after = lead[end..].trim_start_matches(ws);
        if let Some(body) = after.strip_prefix(BYTES_OPEN) {
            if let Some(stop) = body.find(BYTES_CLOSE) {
                end = lead.len() - body.len() + stop + BYTES_CLOSE.len();
            }
        }
        parts.push(&lead[at..end]);
        let next = lead[end..].trim_start_matches(ws);
        if !next.starts_with('{') {
            if !next.is_empty() {
                return None;
            }
            break;
        }
        at = lead.len() - next.len();
    }
    (parts.len() > 1).then_some(parts)
}

/// One call's text inside the tags, leading whitespace gone: its name, its internal
/// arguments and its internal call text.
fn read_one(lead: &str, tools: &[&str]) -> Result<(String, Map<String, Value>, String), String> {
    let mut it = serde_json::Deserializer::from_str(lead).into_iter::<Value>();
    let value = match it.next() {
        Some(Ok(v)) => v,
        Some(Err(e)) => return Err(fill(REPORT_JSON, &[("error", e.to_string().as_str())])),
        None => {
            let e = serde_json::from_str::<Value>(lead)
                .err()
                .map(|e| e.to_string());
            return Err(fill(
                REPORT_JSON,
                &[("error", e.unwrap_or_default().as_str())],
            ));
        }
    };
    let end = it.byte_offset();
    let rest = &lead[end..];
    let body = block_of(rest)?;
    let Some(call) = value.as_object() else {
        return Err(REPORT_SHAPE.to_string());
    };
    let (Some(Value::String(name)), Some(Value::Object(_))) =
        (call.get("name"), call.get("arguments"))
    else {
        return Err(REPORT_SHAPE.to_string());
    };
    // the arguments again, each number as the text the model wrote: the parser's own
    // number text is normalised (`1e3` reads as `1e+3`)
    let literal: Value = serde_json::from_str(&numbers_as_text(&lead[..end]))
        .map_err(|e| fill(REPORT_JSON, &[("error", e.to_string().as_str())]))?;
    let Some(Value::Object(args)) = literal.get("arguments") else {
        return Err(REPORT_SHAPE.to_string());
    };
    if call.len() != 2 {
        return Err(REPORT_SHAPE.to_string());
    }
    if !tools.contains(&name.as_str()) {
        return Err(fill(
            REPORT_NOT_A_TOOL,
            &[
                ("name", name.as_str()),
                ("tools", tools.join(", ").as_str()),
            ],
        ));
    }
    let internal = if name == "send" {
        // an inline body must be a JSON string: `numbers_as_text` would make a number one
        let typed_bytes = call
            .get("arguments")
            .and_then(|a| a.get("payload"))
            .and_then(|p| p.get("bytes"));
        if typed_bytes.is_some_and(|b| !b.is_string()) {
            return Err(REPORT_PAYLOAD.to_string());
        }
        send_args(args, body)?
    } else {
        if body.is_some() {
            return Err(REPORT_BLOCK.to_string());
        }
        let reason = args.get("reason").map(text_of);
        let has_reason = reason
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if name == "stop" && !has_reason {
            return Err(REPORT_STOP_REASON.to_string());
        }
        args.iter().map(|(k, v)| (k.clone(), text_of(v))).collect()
    };
    let text = render_call(name, &internal);
    // the internal form must read back as this one call: a value holding the form's own
    // tags (`</call>` outside a body) would not
    let back = extract_tool_calls(&text);
    if back.len() == 1 && back[0].name == *name && envelopes(&text).len() == 1 {
        return Ok((name.clone(), internal, text));
    }
    let block = &text[crate::tool_parse::OPEN.len()..text.len() - CLOSE.len()];
    let error = match parse_call(block) {
        Err(m) => m.0,
        // it parses alone but not inside the text: a value closes the call early
        Ok(_) => crate::tool_parse::defang(&text),
    };
    Err(fill(REPORT_OTHER, &[("error", error.as_str())]))
}

/// `json` (one valid JSON value) with every number outside a string written as a JSON
/// string of its own text, so a number keeps the literal the model wrote.
fn numbers_as_text(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + 8);
    let mut chars = json.chars().peekable();
    let (mut in_string, mut escaped) = (false, false);
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == '-' || c.is_ascii_digit() {
            out.push('"');
            out.push(c);
            while let Some(&n) = chars.peek() {
                if !(n.is_ascii_digit() || matches!(n, '+' | '-' | '.' | 'e' | 'E')) {
                    break;
                }
                out.push(n);
                chars.next();
            }
            out.push('"');
        } else {
            out.push(c);
        }
    }
    out
}

/// What follows the JSON inside the call: nothing, or exactly one `<bytes>` block with
/// whitespace around it. `Ok(Some(body))` for a block.
fn block_of(rest: &str) -> Result<Option<String>, String> {
    let ws = |c: char| c.is_ascii() && is_json_ws(c as u8);
    let rest = rest.trim_matches(ws);
    if rest.is_empty() {
        return Ok(None);
    }
    // EXPERIMENTAL (Q12): the body of a send is read from the raw block after the JSON
    // line. This departs from the trained Hermes format, where a body is an inline JSON
    // string: models mis-escape long code inside JSON strings, so the block's body is
    // taken verbatim, nothing unescaped, to the first `</bytes>`. The trained inline
    // form is read too: a `payload.bytes` string with no block is the body, as JSON
    // decoded it (`send_args`). A body in both places is refused. Render writes only
    // the raw block. To be measured against the inline form.
    let Some(after) = rest.strip_prefix(BYTES_OPEN) else {
        return Err(REPORT_BLOCK.to_string());
    };
    let Some(end) = after.find(BYTES_CLOSE) else {
        return Err(REPORT_BLOCK.to_string());
    };
    if !after[end + BYTES_CLOSE.len()..].trim_matches(ws).is_empty() {
        return Err(REPORT_BLOCK.to_string());
    }
    Ok(Some(after[..end].to_string()))
}

/// A send's JSON arguments as the internal send arguments: routing flat in [`ROUTING`]
/// order, then `payload`, the block's body as its `bytes`.
fn send_args(
    args: &Map<String, Value>,
    body: Option<String>,
) -> Result<Map<String, Value>, String> {
    if let Some(k) = args.keys().find(|k| *k != "routing" && *k != "payload") {
        return Err(fill(REPORT_SEND_ARGS, &[("key", k.as_str())]));
    }
    let Some(Value::Object(routing)) = args.get("routing") else {
        return Err(REPORT_ROUTING.to_string());
    };
    if let Some(k) = routing.keys().find(|k| !ROUTING.contains(&k.as_str())) {
        return Err(fill(
            REPORT_ROUTING_KEY,
            &[("key", k.as_str()), ("fields", ROUTING.join(", ").as_str())],
        ));
    }
    let mut payload = match args.get("payload") {
        None => Map::new(),
        Some(Value::Object(p)) => p.clone(),
        Some(_) => return Err(REPORT_PAYLOAD.to_string()),
    };
    // EXPERIMENTAL (Q12): the trained inline body, a `payload.bytes` string, is the body
    // when no block follows, as JSON decoded it, nothing trimmed.
    let body = match (payload.shift_remove("bytes"), body) {
        (None, body) => body,
        (Some(_), Some(_)) => return Err(REPORT_BLOCK.to_string()),
        (Some(Value::String(b)), None) => Some(b),
        (Some(_), None) => return Err(REPORT_PAYLOAD.to_string()),
    };
    let named = |v: Option<&Value>| match v.map(text_of) {
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(items)) => items
            .iter()
            .any(|x| x.as_str().is_some_and(|s| !s.trim().is_empty())),
        _ => false,
    };
    if !named(routing.get("target_id")) && !named(routing.get("to")) {
        return Err(REPORT_NO_TARGET.to_string());
    }
    let mut out = Map::new();
    for key in ROUTING {
        if let Some(v) = routing.get(key) {
            out.insert(key.to_string(), text_of(v));
        }
    }
    let mut p: Map<String, Value> = payload
        .iter()
        .map(|(k, v)| (k.clone(), text_of(v)))
        .collect();
    if let Some(b) = body {
        p.insert("bytes".to_string(), Value::String(b));
    }
    out.insert("payload".to_string(), Value::Object(p));
    Ok(out)
}

/// A JSON value with every leaf as text: a number as its literal, `true` and `false` as
/// those words. `null` stays null: the internal form writes it as an empty tag, the
/// notation's null, which the router reads as null where the schema allows it. The
/// internal form types nothing else.
fn text_of(v: &Value) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::Bool(b) => Value::String(if *b { "true" } else { "false" }.to_string()),
        Value::Number(n) => Value::String(n.to_string()),
        Value::String(s) => Value::String(s.clone()),
        Value::Array(items) => Value::Array(items.iter().map(text_of).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), text_of(x))).collect()),
    }
}

// ── the cut call ─────────────────────────────────────────────────────────────

/// What a prefix scan of a cut call's JSON found.
#[derive(Debug, Default)]
struct Cut {
    /// `name`: whole, or the part written before the cut.
    name: Option<(String, bool)>,
    /// `arguments.routing` was opened.
    routing: bool,
    /// `arguments.routing.target_id`: whole, or the part written before the cut.
    target: Option<(String, bool)>,
}

/// The internal head of a call cut at the stream's end, and the parts complete at the
/// cut: `after` is everything after `<tool_call>`. The head is the internal prefix that
/// matches what was written: `<call name="` and the name so far; then `">`; for a send,
/// `<routing>` once it opened, `<target_id>` and the target so far, `</target_id>` once
/// it is whole. Nothing after the target is read: the call is not sent.
fn cut_head(after: &str) -> (String, Option<String>, Option<String>) {
    let mut cut = Cut::default();
    let mut scan = Prefix {
        s: after.as_bytes(),
        text: after,
        i: 0,
    };
    let mut path: Vec<String> = Vec::new();
    scan.ws();
    if scan.peek() == Some(b'{') {
        let _ = scan.value(&mut path, &mut cut);
    }
    let mut head = String::new();
    let mut name = None;
    let mut target = None;
    if let Some((n, whole)) = &cut.name {
        head.push_str("<call name=\"");
        head.push_str(n);
        if *whole {
            head.push_str("\">");
            name = Some(n.clone());
            if n == "send" && cut.routing {
                head.push_str("<routing>");
                if let Some((t, whole)) = &cut.target {
                    head.push_str("<target_id>");
                    head.push_str(t);
                    if *whole {
                        head.push_str("</target_id>");
                        target = Some(t.trim().to_string()).filter(|t| !t.is_empty());
                    }
                }
            }
        }
    } else {
        head.push_str("<call name=\"");
    }
    (head, name, target)
}

/// A scan of a JSON text that may stop anywhere: `None` from a step means the text
/// ended (or stopped reading as JSON) there.
struct Prefix<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
}

impl Prefix<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && is_json_ws(self.s[self.i]) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// One value at `path`, noting the parts [`Cut`] keeps.
    fn value(&mut self, path: &mut Vec<String>, cut: &mut Cut) -> Option<()> {
        self.ws();
        match self.peek()? {
            b'{' => {
                self.i += 1;
                if path.len() == 2 && path[0] == "arguments" && path[1] == "routing" {
                    cut.routing = true;
                }
                loop {
                    self.ws();
                    match self.peek()? {
                        b'}' => {
                            self.i += 1;
                            return Some(());
                        }
                        b'"' => {
                            let (key, whole) = self.string()?;
                            if !whole {
                                return None;
                            }
                            self.ws();
                            if self.peek()? != b':' {
                                return None;
                            }
                            self.i += 1;
                            path.push(key);
                            let r = self.value(path, cut);
                            path.pop();
                            r?;
                            self.ws();
                            match self.peek()? {
                                b',' => self.i += 1,
                                b'}' => {
                                    self.i += 1;
                                    return Some(());
                                }
                                _ => return None,
                            }
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.i += 1;
                loop {
                    self.ws();
                    if self.peek()? == b']' {
                        self.i += 1;
                        return Some(());
                    }
                    path.push(String::new());
                    let r = self.value(path, cut);
                    path.pop();
                    r?;
                    self.ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Some(());
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => {
                let (s, whole) = self.string()?;
                let at = |p: &[&str]| {
                    path.len() == p.len() && path.iter().zip(p).all(|(a, b)| a.as_str() == *b)
                };
                if at(&["name"]) {
                    cut.name = Some((s, whole));
                } else if at(&["arguments", "routing", "target_id"]) {
                    cut.target = Some((s, whole));
                }
                whole.then_some(())
            }
            _ => {
                // a scalar: it is whole only once a delimiter follows it
                let start = self.i;
                while self.i < self.s.len()
                    && !matches!(self.s[self.i], b',' | b'}' | b']')
                    && !is_json_ws(self.s[self.i])
                {
                    self.i += 1;
                }
                (self.i < self.s.len() && self.i > start).then_some(())
            }
        }
    }

    /// A string at `i` (its opening quote): `(text, whole)`. A string cut before its
    /// closing quote gives the part before the cut, an escape cut in the middle left
    /// out. `None` only when a whole string does not decode.
    fn string(&mut self) -> Option<(String, bool)> {
        let start = self.i;
        let mut j = start + 1;
        let mut esc = false;
        while j < self.s.len() {
            let c = self.s[j];
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                self.i = j + 1;
                let s: String = serde_json::from_str(&self.text[start..=j]).ok()?;
                return Some((s, true));
            }
            j += 1;
        }
        self.i = self.s.len();
        Some((decode_prefix(&self.text[start + 1..]), false))
    }
}

/// The decoded text of a JSON string body cut before its closing quote, up to the
/// first escape that is not whole.
fn decode_prefix(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(e) = chars.next() else {
            break;
        };
        let simple = match e {
            '"' => Some('"'),
            '\\' => Some('\\'),
            '/' => Some('/'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'b' => Some('\u{8}'),
            'f' => Some('\u{c}'),
            _ => None,
        };
        if let Some(s) = simple {
            out.push(s);
            continue;
        }
        if e != 'u' {
            break;
        }
        let hex: String = chars.by_ref().take(4).collect();
        let Some(hi) = (hex.len() == 4)
            .then(|| u32::from_str_radix(&hex, 16).ok())
            .flatten()
        else {
            break;
        };
        if (0xD800..0xDC00).contains(&hi) {
            let rest: String = chars.by_ref().take(6).collect();
            let lo = rest
                .strip_prefix("\\u")
                .filter(|h| h.len() == 4)
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .filter(|lo| (0xDC00..0xE000).contains(lo));
            let Some(lo) = lo else {
                break;
            };
            match char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)) {
                Some(ch) => out.push(ch),
                None => break,
            }
        } else {
            match char::from_u32(hi) {
                Some(ch) => out.push(ch),
                None => break,
            }
        }
    }
    out
}
