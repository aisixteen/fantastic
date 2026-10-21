//! Streaming parts — a model's output split into PARTS as it is written. A port
//! of python `ai_core/stream_parts.py`, which is canonical.
//!
//! The provider streams raw text; this splitter reads it incrementally, with the
//! SAME grammar [`crate::tool_parse`] reads whole, and yields part lifecycle
//! events:
//!
//! ```text
//! Start(part)          a part opened — {stream_id, kind, name?, target?, to?}
//! Delta(part, text)    text of that part as it arrives
//! End(part, ok)        the part closed (`</think>`, `</call>`, or the pass ended on text);
//!                      `ok` is `None` for text/thinking — for a call, `Some(bool)`:
//!                      whether the block parsed (a malformed call is never dispatched)
//! Abort(part, why)     the part never closed: the pass ended, was cut, or was stopped
//! ```
//!
//! Kinds: `text` (prose), `thinking` (inside `<think>`), `call` (a `<call name="…">`
//! envelope, its args streaming as they are written). A call written inside a
//! thought is thinking text, never a call part — only the answer acts.
//!
//! It is ANIMATION, never action: nothing here dispatches. The call parser in
//! [`crate::tool_parse`] stays the one authority on what a call IS; this only
//! tells a watcher what is being written. A call part names its addressee once
//! `<target_id>` (or `<to>`) has closed, so its `Start` is held until then — a
//! watcher of that addressee sees the whole part.

use crate::think::{THINK_CLOSE, THINK_OPEN};
use crate::tool_parse::grammar::open_tags;
use crate::tool_parse::{parses, BYTES_CLOSE, BYTES_OPEN, CLOSE, OPEN};
use fantastic_bundle::{is_space, py_strip};
use serde_json::{json, Map, Value};

const NAME_HEAD: &str = "name=\"";

/// The reason [`PartSplitter::finish`] aborts with — python `finish`'s default.
pub const NO_CLOSE_TAG: &str = "no_close_tag";

/// What a part is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartKind {
    /// Prose.
    Text,
    /// Inside `<think>`.
    Thinking,
    /// A `<call name="…">` envelope.
    Call,
}

impl PartKind {
    /// The wire spelling — python's `kind` string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Thinking => "thinking",
            Self::Call => "call",
        }
    }
}

/// One part — python's part dict `{stream_id, kind, name?, target?, to?}`. A
/// call part carries `name`, and `target` / `to` once they are written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// `<source>:<8 hex>`.
    pub stream_id: String,
    /// text | thinking | call.
    pub kind: PartKind,
    /// The tool a call part names.
    pub name: Option<String>,
    /// A call part's `<target_id>`, once closed.
    pub target: Option<String>,
    /// A call part's `<to>` items, once closed.
    pub to: Option<Vec<String>>,
}

impl Part {
    /// The part minus its `stream_id`, in python's key order — what `part_start`
    /// carries after its `type`.
    #[must_use]
    pub fn head(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("kind".to_string(), json!(self.kind.as_str()));
        if let Some(n) = &self.name {
            m.insert("name".to_string(), json!(n));
        }
        if let Some(t) = &self.target {
            m.insert("target".to_string(), json!(t));
        }
        if let Some(to) = &self.to {
            m.insert("to".to_string(), json!(to));
        }
        m
    }
}

/// One part lifecycle event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartEvent {
    /// A part opened.
    Start(Part),
    /// Text of that part as it arrives.
    Delta(Part, String),
    /// The part closed. The info: `None` for text/thinking (python `{}`); for a
    /// call, `Some(ok)` — whether the block parsed (python `{"ok": bool}`).
    End(Part, Option<bool>),
    /// The part never closed; the string is why.
    Abort(Part, String),
}

impl PartEvent {
    /// The part this event is about.
    #[must_use]
    pub fn part(&self) -> &Part {
        match self {
            Self::Start(p) | Self::Delta(p, _) | Self::End(p, _) | Self::Abort(p, _) => p,
        }
    }
}

/// How many trailing bytes of `buf` could be the start of one of `tags` — held
/// back until the next chunk says whether they are markup or prose. The tags are
/// ASCII, so a matching tail is ASCII and always a char boundary.
fn held_tail(buf: &str, tags: &[&str]) -> usize {
    let b = buf.as_bytes();
    let mut best = 0;
    for tag in tags {
        let t = tag.as_bytes();
        for n in (1..=(t.len() - 1).min(b.len())).rev() {
            if t.starts_with(&b[b.len() - n..]) {
                best = best.max(n);
                break;
            }
        }
    }
    best
}

/// Feed it chunks; it returns part events. One per model pass.
#[derive(Debug)]
pub struct PartSplitter {
    source: String,
    /// Unconsumed text.
    buf: String,
    in_think: bool,
    /// The open text/thinking part (an index into `parts`).
    part: Option<usize>,
    /// The open call part (an index into `parts`).
    call: Option<usize>,
    /// The call's body so far (for target detection).
    call_body: String,
    /// Its `Start` was emitted.
    call_started: bool,
    /// The call was written inside an open tag (`<tool_call>`): the router refuses
    /// it (#188), so it ends not ok and is never matched to a dispatch.
    call_wrapped: bool,
    /// The answer's text outside thinking and outside calls, as the router reads it:
    /// the tags a call written now would sit inside ([`open_tags`]).
    outside: String,
    /// Deltas held until the addressee is known.
    call_pending: String,
    in_bytes: bool,
    /// Every part opened, in order.
    pub parts: Vec<Part>,
    /// Every CLOSED call part, in order: `(stream_id, body)` — what a dispatched
    /// call is matched to by its exact body, so the message it becomes carries the
    /// stream it was previewed as.
    pub closed_calls: Vec<(String, String)>,
    /// STOPPED is terminal: the stop runs while the pass's own task is still
    /// alive, so a chunk the provider already handed over may still be fed — it
    /// opens no part after the aborts (python `PartSplitter._stopped`).
    stopped: bool,
}

impl PartSplitter {
    /// A splitter for one pass of agent `source`.
    #[must_use]
    pub fn new(source: &str) -> Self {
        Self {
            source: source.to_string(),
            buf: String::new(),
            in_think: false,
            part: None,
            call: None,
            call_body: String::new(),
            call_started: false,
            call_wrapped: false,
            outside: String::new(),
            call_pending: String::new(),
            in_bytes: false,
            parts: Vec::new(),
            closed_calls: Vec::new(),
            stopped: false,
        }
    }

    // ── public ──

    /// The next chunk of the stream.
    pub fn feed(&mut self, chunk: &str) -> Vec<PartEvent> {
        if self.stopped {
            return Vec::new();
        }
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while self.step(&mut out) {}
        out
    }

    /// The pass ended: flush prose, END an open text part, ABORT an open thought
    /// or call (a call that never closed was never a call).
    pub fn finish(&mut self) -> Vec<PartEvent> {
        self.finish_because(NO_CLOSE_TAG)
    }

    /// [`Self::finish`] with the abort reason named.
    pub fn finish_because(&mut self, why: &str) -> Vec<PartEvent> {
        if self.stopped {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.call.is_some() {
            let rest = std::mem::take(&mut self.buf);
            self.call_body.push_str(&rest);
            out.extend(self.abort_call(why));
        } else {
            if !self.buf.is_empty() {
                let rest = std::mem::take(&mut self.buf);
                self.text(&mut out, &rest);
            }
            if let Some(pi) = self.part.take() {
                let p = self.parts[pi].clone();
                if p.kind == PartKind::Thinking {
                    out.push(PartEvent::Abort(p, why.to_string()));
                } else {
                    out.push(PartEvent::End(p, None));
                }
            }
        }
        out
    }

    /// Stopped mid-stream: every open part ABORTS (nothing is half-ended), and
    /// nothing fed after opens another.
    pub fn stop(&mut self, why: &str) -> Vec<PartEvent> {
        self.stopped = true;
        let mut out = Vec::new();
        if self.call.is_some() {
            out.extend(self.abort_call(why));
        }
        if let Some(pi) = self.part.take() {
            out.push(PartEvent::Abort(self.parts[pi].clone(), why.to_string()));
        }
        self.buf.clear();
        out
    }

    /// The call part still open, if any.
    #[must_use]
    pub fn open_call(&self) -> Option<&Part> {
        self.call.map(|ci| &self.parts[ci])
    }

    /// Inside an unclosed `<think>`.
    #[must_use]
    pub fn in_think(&self) -> bool {
        self.in_think
    }

    // ── internals ──

    fn new_part(&mut self, kind: PartKind, name: Option<String>) -> usize {
        let hex: String = crate::helpers::os_random(4)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.parts.push(Part {
            stream_id: format!("{}:{hex}", self.source),
            kind,
            name,
            target: None,
            to: None,
        });
        self.parts.len() - 1
    }

    /// Prose or thought, outside any call.
    fn text(&mut self, out: &mut Vec<PartEvent>, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.in_think {
            self.outside.push_str(text);
        }
        let kind = if self.in_think {
            PartKind::Thinking
        } else {
            PartKind::Text
        };
        let pi = match self.part {
            Some(pi) => pi,
            None => {
                if kind == PartKind::Text && py_strip(text).is_empty() {
                    return; // whitespace between parts is not a part
                }
                let pi = self.new_part(kind, None);
                self.part = Some(pi);
                out.push(PartEvent::Start(self.parts[pi].clone()));
                pi
            }
        };
        out.push(PartEvent::Delta(self.parts[pi].clone(), text.to_string()));
    }

    fn close_text_part(&mut self, out: &mut Vec<PartEvent>) {
        if let Some(pi) = self.part.take() {
            out.push(PartEvent::End(self.parts[pi].clone(), None));
        }
    }

    /// The earliest of `tags` in the buffer, `(index, tag)`.
    fn first_hit<'t>(&self, tags: &[&'t str]) -> Option<(usize, &'t str)> {
        tags.iter()
            .filter_map(|t| self.buf.find(t).map(|i| (i, *t)))
            .min()
    }

    fn step(&mut self, out: &mut Vec<PartEvent>) -> bool {
        if self.call.is_some() {
            return self.step_call(out);
        }
        // Outside a call: look for the next markup that changes the part. INSIDE
        // thinking only `</think>` does — a `<call>` there is thinking text, never a
        // call part (only the answer acts).
        let opens: &[&str] = if self.in_think {
            &[THINK_CLOSE]
        } else {
            &[OPEN, THINK_OPEN]
        };
        let Some((i, tag)) = self.first_hit(opens) else {
            let cut = self.buf.len() - held_tail(&self.buf, opens);
            let prose = self.buf[..cut].to_string();
            self.text(out, &prose);
            self.buf.drain(..cut);
            return false;
        };
        let before = self.buf[..i].to_string();
        self.text(out, &before);
        if tag == OPEN {
            let rest = self.buf[i + OPEN.len()..].to_string();
            // `<call` + whitespace + `name="X">` — anything else is prose (`<caller>`).
            if rest.is_empty() {
                self.buf.drain(..i);
                return false;
            }
            if !rest.starts_with(is_space) {
                self.text(out, OPEN);
                self.buf = rest;
                return true;
            }
            let head = rest.trim_start_matches(is_space);
            if !head.starts_with(NAME_HEAD) {
                if NAME_HEAD.starts_with(head) {
                    self.buf.drain(..i); // could still become a call
                    return false;
                }
                self.text(out, OPEN);
                self.buf = rest;
                return true;
            }
            let q = head[NAME_HEAD.len()..]
                .find('"')
                .map(|p| p + NAME_HEAD.len());
            let gt = q.and_then(|q| head[q + 1..].find('>').map(|p| p + q + 1));
            let (Some(q), Some(gt)) = (q, gt) else {
                self.buf.drain(..i);
                return false;
            };
            self.close_text_part(out);
            let name = head[NAME_HEAD.len()..q].to_string();
            let ci = self.new_part(PartKind::Call, Some(name));
            self.call = Some(ci);
            // The same test the scanner runs (`envelopes_in`): a tag still open
            // before the call wraps it. Code is not markup there either.
            self.call_wrapped = !open_tags(&self.outside).is_empty();
            self.call_body.clear();
            self.call_started = false;
            self.call_pending.clear();
            self.in_bytes = false;
            self.buf = head[gt + 1..].to_string();
            return true;
        }
        // a thinking boundary
        self.close_text_part(out);
        self.in_think = tag == THINK_OPEN;
        self.buf.drain(..i + tag.len());
        true
    }

    fn step_call(&mut self, out: &mut Vec<PartEvent>) -> bool {
        let stop_tags: &[&str] = if self.in_bytes {
            &[BYTES_CLOSE]
        } else {
            &[BYTES_OPEN, CLOSE]
        };
        let Some((i, tag)) = self.first_hit(stop_tags) else {
            let cut = self.buf.len() - held_tail(&self.buf, stop_tags);
            let body = self.buf[..cut].to_string();
            self.call_text(out, &body);
            self.buf.drain(..cut);
            return false;
        };
        if tag == CLOSE {
            let body = self.buf[..i].to_string();
            self.call_text(out, &body);
            self.buf.drain(..i + CLOSE.len());
            self.end_call(out);
            return true;
        }
        let body = self.buf[..i + tag.len()].to_string();
        self.call_text(out, &body);
        self.buf.drain(..i + tag.len());
        self.in_bytes = tag == BYTES_OPEN;
        true
    }

    fn call_text(&mut self, out: &mut Vec<PartEvent>, text: &str) {
        if text.is_empty() {
            return;
        }
        let ci = self.call.expect("call_text runs inside a call");
        self.call_body.push_str(text);
        if self.call_started {
            out.push(PartEvent::Delta(self.parts[ci].clone(), text.to_string()));
            return;
        }
        self.call_pending.push_str(text);
        self.learn_addressee(ci);
        if self.addressee_known(ci) {
            self.call_started = true;
            out.push(PartEvent::Start(self.parts[ci].clone()));
            out.push(PartEvent::Delta(
                self.parts[ci].clone(),
                std::mem::take(&mut self.call_pending),
            ));
        }
    }

    fn learn_addressee(&mut self, ci: usize) {
        for tag in ["target_id", "to"] {
            let body = &self.call_body;
            let Some(a) = body.find(&format!("<{tag}>")) else {
                continue;
            };
            let Some(b) = body[a + 1..].find(&format!("</{tag}>")).map(|p| p + a + 1) else {
                continue;
            };
            let inner = &body[a + tag.len() + 2..b];
            if tag == "target_id" {
                self.parts[ci].target = Some(py_strip(inner).to_string());
            } else {
                let items: Vec<String> = inner
                    .split("<item>")
                    .skip(1)
                    .map(|x| py_strip(x.split("</item>").next().unwrap_or("")).to_string())
                    .filter(|x| !x.is_empty())
                    .collect();
                self.parts[ci].to = Some(items);
            }
        }
    }

    fn addressee_known(&self, ci: usize) -> bool {
        let call = &self.parts[ci];
        if call.target.is_some() || call.to.is_some() {
            return true;
        }
        // Only `send` names an addressee; every other tool (done, await,
        // inquiry_answer) speaks to the caller from its first byte.
        call.name.as_deref() != Some("send")
    }

    fn end_call(&mut self, out: &mut Vec<PartEvent>) {
        let ci = self.call.take().expect("end_call runs inside a call");
        if !self.call_started {
            self.call_started = true;
            out.push(PartEvent::Start(self.parts[ci].clone()));
            if !self.call_pending.is_empty() {
                out.push(PartEvent::Delta(
                    self.parts[ci].clone(),
                    self.call_pending.clone(),
                ));
            }
        }
        self.call_pending.clear();
        let call = &self.parts[ci];
        // A wrapped call is not dispatched, so it is not ok, and it is not pushed to
        // `closed_calls`: the router matches a dispatch to a stream by its exact body,
        // and a refused copy with the same body must not take the dispatched call's
        // stream id.
        let ok = !self.call_wrapped
            && parses(&format!(
                " name=\"{}\">{}",
                call.name.as_deref().unwrap_or(""),
                self.call_body
            ));
        if !self.call_wrapped {
            self.closed_calls
                .push((call.stream_id.clone(), self.call_body.clone()));
        }
        out.push(PartEvent::End(call.clone(), Some(ok)));
    }

    fn abort_call(&mut self, why: &str) -> Vec<PartEvent> {
        let mut out = Vec::new();
        let ci = self.call.take().expect("abort_call runs inside a call");
        if self.call_started {
            out.push(PartEvent::Abort(self.parts[ci].clone(), why.to_string()));
        }
        // An unannounced call (its addressee never written) aborts silently:
        // nobody was shown its start.
        self.call_pending.clear();
        out
    }
}

/// The assistant row a CUT pass leaves — a marker on top of
/// the context, never a half answer. `why` is `"stopped by <receiver>"` (a
/// receiver stopped the stream) or `"model stopped generating"` (the pass ended
/// with a block still open). Kept: the VISIBLE text streamed so far, thinking
/// stripped; a cut inside an open thought says so; every call that did not go
/// out is NAMED as not sent, never kept as markup.
#[must_use]
pub fn cut_marker(text_so_far: &str, why: &str, unsent: &[Part]) -> String {
    let mut splitter = PartSplitter::new("marker");
    let events = splitter.feed(text_so_far);
    let visible: String = events
        .iter()
        .filter_map(|ev| match ev {
            PartEvent::Delta(p, t) if p.kind == PartKind::Text => Some(t.as_str()),
            _ => None,
        })
        .collect();
    let visible = py_strip(&visible);
    let mut head = format!("[{why}");
    if splitter.in_think() && visible.is_empty() {
        head.push_str(" while thinking");
    }
    let mut lines = vec![head + "]"];
    if !visible.is_empty() {
        lines.push(visible.to_string());
    }
    for call in unsent {
        let to = match (&call.target, &call.to) {
            (Some(t), _) if !t.is_empty() => t.clone(),
            (_, Some(to)) if !to.join(", ").is_empty() => to.join(", "),
            _ => "the caller".to_string(),
        };
        lines.push(format!(
            "[a {} call to {to} was not sent]",
            call.name.as_deref().unwrap_or("call")
        ));
    }
    lines.join("\n")
}
