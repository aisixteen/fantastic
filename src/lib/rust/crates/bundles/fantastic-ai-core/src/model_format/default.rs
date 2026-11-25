//! THE `default` TYPE (#766, 1.4): today's tag grammar, a type like the others.
//!
//! - Inbound it is the identity: the internal form is what a `default` model reads, so
//!   the render cache stores nothing for it.
//! - Outbound every delta is forwarded live as text, as on `4fd5bb4d`. The attempt fails
//!   ([`Out::Bad`]) at the first foreign call tag, as soon as a delta completes it
//!   ([`crate::call_check::foreign_cut`]), and at the stream's end for a terminated
//!   envelope that does not parse ([`crate::tool_parse::malformed_calls`]) or a call
//!   written wrong ([`crate::call_check::check`] with nothing ran), or a `stop` with no
//!   reason ([`REPORT_STOP_REASON`], S25). A stream that ends
//!   inside a call is [`Out::Open`]: the cut path, no retry.
//! - Its model-visible words are the resource files of `res/default/` (#760): the `send`
//!   how-to, the call form, the placeholder form, the parser's hint and the words of the
//!   checker's report.
//!
//! So a good `default` pass streams, routes and stores the same bytes as before (R10,
//! R11); the report of a bad one is, byte for byte, what the router wrote for the same
//! text (S10).

use regex::Regex;
use std::sync::OnceLock;

use super::{Out, Outbound, Reader, Renderer, DEFAULT};
use crate::think::{THINK_CLOSE, THINK_OPEN};
use crate::tool_parse::grammar::{envelopes, envelopes_in};
use crate::tool_parse::{extract_tool_calls, malformed_calls, CLOSE, TOOLS};

// ── resources (res/default, #760) ────────────────────────────────────────────

/// How to reach another agent: the envelope the router dispatches
/// ([`crate::route::route_reply`]), in the system block's how-to.
///
/// It lands in the default system block only because routing exists: a prompt that
/// teaches an envelope nothing dispatches is strictly worse than no prompt at all, so
/// this text and the router are one change. It states facts and a few short working
/// rules: how to address an agent, what `reflect` returns, where `reply_to` sends the
/// answer, how to recognise a reply, and when work counts as done. It never names a
/// loop; stopping is `done` or `await`.
pub(crate) const HOWTO_SEND: &str = include_str!("res/default/howto_send.txt");
/// The last line of every checker report: the form, in one sentence.
pub(crate) const FORM: &str = include_str!("res/default/form.txt");
/// The placeholder form of a call opener, as notices write it. Another type swaps it by
/// exact match (Q22).
pub(crate) const FORM_OPEN: &str = include_str!("res/default/form_open.txt");
/// The shape appended to every parser refusal: literal, minimal, copyable. Advice, not a
/// repair: the block is never re-parsed leniently.
pub(crate) const HINT: &str = include_str!("res/default/report_hint.txt");
/// A checker report's first line: `{line}`, `{col}`.
pub(crate) const REPORT_HEAD: &str = include_str!("res/default/report_head.txt");
/// Why the first token departs from the grammar: a call opener that lost its `<`.
pub(crate) const REPORT_BARE: &str = include_str!("res/default/report_bare.txt");
/// ... a JSON call object.
pub(crate) const REPORT_JSON: &str = include_str!("res/default/report_json.txt");
/// ... an argument tag with no call open.
pub(crate) const REPORT_ARG: &str = include_str!("res/default/report_arg.txt");
/// ... a close with no call open.
pub(crate) const REPORT_CLOSE: &str = include_str!("res/default/report_close.txt");
/// ... any other call-like tag.
pub(crate) const REPORT_OTHER: &str = include_str!("res/default/report_other.txt");
/// Appended to the caret line of a lone stray close.
pub(crate) const REPORT_DELETE: &str = include_str!("res/default/report_delete.txt");
/// A note on a `<parameter=x>` argument: `{tag}` as written, `{arg}` its name.
pub(crate) const REPORT_PARAM: &str = include_str!("res/default/report_param.txt");
/// A note on a close that is not `</call>`: `{tag}` as written.
pub(crate) const REPORT_CLOSER: &str = include_str!("res/default/report_closer.txt");
/// A note on a wrapper tag around a call: `{line}`, `{tag}` as written.
pub(crate) const REPORT_WRAPPER: &str = include_str!("res/default/report_wrapper.txt");
/// The notes line: `{notes}`, the notes joined.
pub(crate) const REPORT_ALSO: &str = include_str!("res/default/report_also.txt");
/// The reason of the `stop` the harness writes after the last malformed attempt: `{n}`,
/// the number of attempts (1.8). It holds no text of any attempt.
pub(crate) const STOP_REASON: &str = include_str!("res/default/stop_reason.txt");
/// The report on a `stop` written with no reason, or an empty one (S25).
pub(crate) const REPORT_STOP_REASON: &str = include_str!("res/default/report_stop_reason.txt");

// ── the pair ─────────────────────────────────────────────────────────────────

/// The `default` type: both halves, one value.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tags;

impl Renderer for Tags {
    fn name(&self) -> &'static str {
        DEFAULT
    }

    fn rev(&self) -> u32 {
        1
    }

    fn identity(&self) -> bool {
        true
    }

    fn render(&self, text: &str) -> String {
        text.to_string()
    }
}

impl Reader for Tags {
    fn outbound(&self) -> Box<dyn Outbound> {
        Box::new(Stream::default())
    }

    fn tools(&self) -> &'static [&'static str] {
        &TOOLS
    }
}

/// The streaming state of one `default` answer.
#[derive(Debug, Default)]
struct Stream {
    /// Every byte fed so far, reasoning included.
    raw: String,
    /// A `Bad` was given: nothing more of this answer is forwarded or read.
    failed: bool,
}

impl Outbound for Stream {
    fn feed(&mut self, delta: &str) -> Vec<Out> {
        if self.failed {
            return Vec::new();
        }
        let before = self.raw.len();
        self.raw.push_str(delta);
        // Checked only when this delta can complete a tag or a JSON call head, as the
        // stream checker of `4fd5bb4d` did (#499).
        if delta.contains('>') || delta.contains('"') {
            if let Some(keep) = foreign_keep(&self.raw) {
                // The router read the answer as cut there; the whole text so far is
                // read only if the cut one holds no report (it always holds the tag).
                if let Some(report) = report_of(&self.raw[..keep]).or_else(|| report_of(&self.raw))
                {
                    self.failed = true;
                    let mut out = Vec::new();
                    if keep > before {
                        out.push(Out::Text(self.raw[before..keep].to_string()));
                    }
                    out.push(Out::Bad { report });
                    return out;
                }
            }
        }
        if delta.is_empty() {
            Vec::new()
        } else {
            vec![Out::Text(delta.to_string())]
        }
    }

    fn finish(&mut self) -> Vec<Out> {
        if self.failed {
            return Vec::new();
        }
        if let Some(open) = open_call(&answer_of(&self.raw)) {
            return vec![open];
        }
        match report_of(&self.raw) {
            Some(report) => {
                self.failed = true;
                vec![Out::Bad { report }]
            }
            None => Vec::new(),
        }
    }
}

/// The byte length of the raw answer to keep, just past its first foreign call tag, or
/// `None` while it holds none. The check reads the answer without its reasoning; the cut
/// is mapped back to the raw text by its tail, which the last delta wrote.
fn foreign_keep(raw: &str) -> Option<usize> {
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

/// What the router wrote back for a finished answer `raw` with nothing ran, in its
/// order: one block per envelope that did not parse, then one per call written wrong;
/// then one per `stop` with no non-empty reason ([`REPORT_STOP_REASON`], 1.4); the
/// blocks a blank line apart. `None` when there is none.
fn report_of(raw: &str) -> Option<String> {
    let answer = crate::call_check::repair(&answer_of(raw));
    let mut blocks: Vec<String> = malformed_calls(&answer).iter().map(|m| m.body()).collect();
    blocks.extend(crate::call_check::check(&answer, 0));
    blocks.extend(
        extract_tool_calls(&answer)
            .into_iter()
            .filter(|c| c.name == "stop" && !has_reason(&c.arguments))
            .map(|_| REPORT_STOP_REASON.to_string()),
    );
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

/// A `stop`'s arguments hold a `reason` that is text with more than spaces in it.
fn has_reason(args: &serde_json::Map<String, serde_json::Value>) -> bool {
    args.get("reason")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|r| !r.trim().is_empty())
}

/// The answer without its reasoning, trimmed, as [`crate::think::without_thinking`] reads
/// it, except that a reasoning marker inside a call envelope is value bytes (a `<bytes>`
/// document may hold `<think>`, S15): a span opens only outside every envelope, and one
/// that never closes runs to the end.
pub(crate) fn answer_of(raw: &str) -> String {
    let mut out = String::new();
    let mut pos = 0usize;
    while pos < raw.len() {
        let rest = &raw[pos..];
        let think = rest.find(THINK_OPEN);
        let env = envelopes(rest).into_iter().next();
        match (think, env) {
            (Some(t), e) if e.is_none_or(|(start, _, _)| t < start) => {
                out.push_str(&rest[..t]);
                match rest[t..].find(THINK_CLOSE) {
                    Some(c) => pos += t + c + THINK_CLOSE.len(),
                    None => break,
                }
            }
            (_, Some((_, _, end))) => {
                let stop = end.map_or(rest.len(), |e| e + CLOSE.len());
                out.push_str(&rest[..stop]);
                pos += stop;
            }
            _ => {
                out.push_str(rest);
                break;
            }
        }
    }
    out.trim().to_string()
}

fn head_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"^\s+name\s*=\s*"([^"]*)"\s*>"#).expect("head"))
}

fn target_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"<target_id>([^<]*)</target_id>").expect("target"))
}

/// The unterminated call the answer ends in, with the parts of its head that were
/// written whole: the name once its closing quote and `>` are in, the target once its
/// `</target_id>` is.
fn open_call(answer: &str) -> Option<Out> {
    let env = envelopes_in(answer).into_iter().last()?;
    if env.end.is_some() {
        return None;
    }
    let block = &answer[env.body..];
    let head = head_re().captures(block);
    let name = head.as_ref().map(|c| c[1].to_string());
    let target = head.and_then(|c| {
        let rest = &block[c.get(0).map_or(0, |m| m.end())..];
        target_re().captures(rest).map(|t| t[1].trim().to_string())
    });
    Some(Out::Open { name, target })
}
