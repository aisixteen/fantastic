//! Tool-call parsing — the ONE text shape a model uses to name a call.
//!
//! Ported rule for rule from python `ai_core/tool_parse.py`, which is canonical.
//! TWO consumers, here as there:
//!
//!  - [`crate::route::route_reply`] — every completion is scanned and each call
//!    found is dispatched, with its reply delivered to its own `reply_to`. This
//!    is the path a chain continues on. A shape this module does not recognize is
//!    a turn that silently does nothing, which is why [`malformed_calls`] exists.
//!  - [`crate::projection::reaction_from`] — scans the DURABLE transcript for
//!    whether the model reacted to a compaction notice (e.g. by emitting a
//!    `recall`/persist call).
//!
//! So this module is LOAD-BEARING on the routing path, not just a read-model.
//! Every failure recorded in it was MEASURED against a live model.
//!
//! There is NO agentic loop here and NO native provider tool API: the provider
//! streams plain text, this module reads it. There is also NO JSON anywhere on
//! this path, in either direction. ONE envelope, args inline:
//!
//! ```text
//! <call name="send">
//! <target_id>notes</target_id>
//! <type>patch</type>
//! <edits><item><path>hello.py</path><hunks>2</hunks></item></edits>
//! <bytes>…verbatim, to the first </bytes>…
//! </bytes>
//! </call>
//! ```
//!
//! There is no other envelope — no `<tool_call>{…}</tool_call>`, no Claude
//! `<invoke>`, no special-token channel — and no `bytes` cut: on shapes that
//! carry a document, bare child tags beat a model's trained JSON format.

pub mod grammar;
pub mod render;
pub mod shape;

#[cfg(test)]
mod tests_grammar;
#[cfg(test)]
mod tests_malformed;
#[cfg(test)]
mod tests_render;

use grammar::{envelopes_in, parse_call, Malformed};

pub use grammar::{BYTES_CLOSE, BYTES_OPEN, CLOSE, OPEN, TOOLS};
pub use render::{render_call, render_reply};
pub use shape::{mint_call_id, ToolCall};

/// The shape appended to every refusal: literal, minimal, copyable. Advice, not
/// a repair — the block is never re-parsed leniently.
pub const HINT: &str = concat!(
    "The shape: <call name=\"send\">\n<target_id>agent id</target_id>\n",
    "<type>verb</type>\n<path>a.md</path>\n<bytes>document, verbatim, to the first ",
    "</bytes>\n</bytes>\n</call> — one child tag per argument, a list is one tag ",
    "whose children are all <item>, nothing is escaped."
);

/// One envelope the scanner found: `(block, call, fault, terminated)`.
///
/// A `None` call with a fault means an envelope WAS opened as a call and its
/// body did not parse — kept, so [`malformed_calls`] can say so. A `None` call
/// without a fault is the model MENTIONING `<call` in prose: stepped over, so a
/// stray mention never swallows the real call after it.
struct Found<'a> {
    block: &'a str,
    call: Option<ToolCall>,
    fault: Option<String>,
    /// Closed by its `</call>`.
    terminated: bool,
}

fn scan(text: &str) -> Vec<Found<'_>> {
    let mut out = Vec::new();
    for env in envelopes_in(text) {
        // A call INSIDE a wrapper tag is not dispatched and is not malformed here
        // either: it is refused and reported by `call_check`, which owns that
        // report, so it is not reported twice.
        if env.wrapper.is_some() {
            continue;
        }
        // UNTERMINATED (`end == None`) is not a truncation guess: the envelope is
        // the LAST thing in the completion, so there is nothing after it to
        // misread.
        let block = &text[env.body..env.end.unwrap_or(text.len())];
        let (call, fault) = match parse_call(block) {
            Ok(call) => (call, None),
            Err(Malformed(fault)) => (None, Some(fault)),
        };
        if call.is_none() && fault.is_none() {
            continue;
        }
        out.push(Found {
            block,
            call,
            fault,
            terminated: env.end.is_some(),
        });
    }
    out
}

/// Whether one envelope body (` name="…">…`, the text after `<call`) parses as a
/// call. It does NOT look at the wrapper: a call inside an open tag parses here and is
/// still refused by [`extract_tool_calls`] (#188). The streaming splitter asks this at
/// `</call>` and ANDs in its own wrapper test (`stream_parts`), so a wrapped call
/// streams as not ok.
#[must_use]
pub fn parses(block: &str) -> bool {
    matches!(parse_call(block), Ok(Some(_)))
}

/// Every dispatchable call with the BODY it was written as (the text after its
/// `name="…">`), in document order — what links a call to the stream part it was
/// previewed as.
#[must_use]
pub fn terminated_calls(text: &str) -> Vec<(ToolCall, String)> {
    scan(text)
        .into_iter()
        .filter(|f| f.terminated)
        .filter_map(|f| {
            let body = f.block.find('>').map_or("", |i| &f.block[i + 1..]);
            f.call.map(|call| (call, body.to_string()))
        })
        .collect()
}

/// Non-streaming: pull every finalized `<call>` out of a complete string. A call
/// with no `</call>` is NOT a call: the model stopped generating before it closed,
/// so nothing is dispatched — the turn's cut marker names it as not sent
/// ([`crate::stream_parts::cut_marker`]). A call written inside a wrapper tag is not
/// one either (#188): the router refuses it, so folds and projection, which read the
/// same calls, do not count it.
#[must_use]
pub fn extract_tool_calls(text: &str) -> Vec<ToolCall> {
    scan(text)
        .into_iter()
        .filter(|f| f.terminated)
        .filter_map(|f| f.call)
        .collect()
}

/// An envelope that opened as a call and did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedCall {
    /// The whole reply body, composed by the parser so the wording has one owner.
    pub error: String,
    /// The envelope opener the model used — always `<call`.
    pub envelope: &'static str,
    /// The block, WHOLE and defanged.
    pub your_block: String,
}

impl MalformedCall {
    /// What the model is told, as PLAIN TEXT — the model↔harness path carries no
    /// JSON in either direction.
    #[must_use]
    pub fn body(&self) -> String {
        format!("{}\n\nyour block:\n{}", self.error, self.your_block)
    }
}

/// Envelopes that opened as a call and did not parse.
///
/// Exists because a call that cannot be read is the one failure a model cannot
/// debug: [`extract_tool_calls`] returns `[]` for it, which is byte-identical to
/// a turn that emitted no call at all — and that means "done": nothing parsed,
/// nothing dispatched, no error came back, and the run ends looking finished
/// with the task untouched. Models self-correct from an error out loud, so the
/// fix is not a better parser, it is giving this failure the same voice the
/// others have.
#[must_use]
pub fn malformed_calls(text: &str) -> Vec<MalformedCall> {
    scan(text)
        .into_iter()
        // An UNTERMINATED block is not malformed — it was cut; the cut marker
        // says so.
        .filter(|f| f.call.is_none() && f.terminated)
        .map(|f| MalformedCall {
            error: format!(
                "call NOT dispatched: {}. Nothing ran. {HINT}",
                f.fault.unwrap_or_default()
            ),
            envelope: OPEN,
            // BOTH ENDS, never just the head — the fault in a long block is
            // almost never in the first 400 chars. Shown WHOLE, DEFANGED so
            // quoting it cannot arm the scanner again.
            your_block: defang(fantastic_kernel::py_strip(f.block)),
        })
        .collect()
}

/// Neutralize envelope delimiters so quoted text cannot re-arm the scanner.
///
/// Anything echoed back becomes conversation the model reads, quotes and reasons
/// about — and a quoted `<call` is indistinguishable from an emitted one.
/// Echoing a block verbatim makes the error self-amplifying, each report seeding
/// the next. Breaking the opener is enough.
#[must_use]
pub fn defang(text: &str) -> String {
    let mut out = text.to_string();
    for tag in [OPEN, CLOSE, BYTES_OPEN, BYTES_CLOSE] {
        out = out.replace(tag, &tag.replace('<', "\u{2039}").replace('>', "\u{203a}"));
    }
    out
}
