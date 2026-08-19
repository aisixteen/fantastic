//! The scanner: every envelope in the completion, in DOCUMENT ORDER, whether or
//! not its body parsed. Ported from python `tool_parse.py:450-500`.

use super::shape::ToolCall;
use super::ENVELOPES;

/// One recognized text envelope: its delimiters, its body parser, the shape test
/// that separates a real attempt from a MENTION, and the advice a malformed one
/// earns.
///
/// ONE table entry per envelope, so the scanner has no per-envelope branches to
/// keep in step and a fourth envelope is one entry. Python keeps the parsers, the
/// hints and the shape tests in three parallel maps (`_ENVELOPES`, `_HINTS`,
/// `_looks_like_an_attempt`); folding them onto the single entry is the same
/// fact, stated once.
pub struct Envelope {
    /// Opening delimiter.
    pub open: &'static str,
    /// Closing delimiter.
    pub close: &'static str,
    /// Body parser for this grammar.
    pub parse: fn(&str) -> Option<ToolCall>,
    /// Shape test over the TRIMMED body — see [`Envelope::looks_like_an_attempt`].
    pub is_attempt: fn(&str) -> bool,
    /// Advice appended to this grammar's malformed-envelope error.
    pub hint: &'static str,
}

impl Envelope {
    /// Did the model MEAN this opener as a call, or is the tag just a word in a
    /// sentence? Only the body's SHAPE can say (python `_looks_like_an_attempt`,
    /// `tool_parse.py:565-573`) — never whether a closing tag was found; see
    /// [`crate::tool_parse::malformed_calls`] for the loop that mistake closed.
    pub fn looks_like_an_attempt(&self, block: &str) -> bool {
        (self.is_attempt)(block.trim())
    }
}

/// One envelope the scanner found: `(envelope, block, call|None, terminated)`.
pub struct Found<'a> {
    /// The table entry that matched.
    pub envelope: &'static Envelope,
    /// The raw body between the delimiters — to end-of-text when unterminated.
    pub block: &'a str,
    /// The parsed call, or `None` when the envelope OPENED and its body did not
    /// parse.
    pub call: Option<ToolCall>,
    /// Whether the closing delimiter was present.
    pub terminated: bool,
}

/// Every envelope found, in document order.
///
/// A `None` call means an envelope WAS opened and its body did not parse. The
/// scanner keeps those instead of dropping them so a caller can tell the two
/// outcomes apart — see [`crate::tool_parse::malformed_calls`]. `terminated` says
/// whether the closing tag was present, which is what separates a real attempt
/// from a model merely MENTIONING the tag in prose. Splitting the scan out of
/// [`crate::tool_parse::extract_tool_calls`] keeps ONE implementation of the
/// envelope precedence rules; two copies would drift the moment a fourth envelope
/// arrives.
pub fn scan(text: &str) -> Vec<Found<'_>> {
    let mut out: Vec<Found<'_>> = Vec::new();
    let mut i = 0usize;
    while i < text.len() {
        // Whichever envelope comes first, in document order. Channel before
        // invoke before taught on a tie, preserving the original precedence:
        // `min_by_key` keeps the FIRST minimum, so the table's own order IS the
        // tie-break, exactly as python's `min` over the same sequence.
        let Some((pos, env)) = ENVELOPES
            .iter()
            .filter_map(|e| text[i..].find(e.open).map(|p| (i + p, e)))
            .min_by_key(|(p, _)| *p)
        else {
            break;
        };
        let body_start = pos + env.open.len();
        let end = text[body_start..].find(env.close).map(|r| body_start + r);
        // UNTERMINATED (`end == None`) is not a truncation guess: the envelope is
        // the LAST thing in the completion, so there is nothing after it to
        // misread. Observed live from nvidia/nemotron-3-ultra-550b, which emits a
        // complete, valid call object and simply stops without the closing tag.
        // Dropping it loses an unambiguous instruction.
        let block = &text[body_start..end.unwrap_or(text.len())];
        let call = (env.parse)(block);
        if call.is_none() && !env.looks_like_an_attempt(block) {
            // A MENTION, not a call — the model wrote the tag inside a sentence.
            // Step over the OPENER ONLY. Consuming through `close` would swallow
            // whatever follows, and what follows is often the real call: a stray
            // opener in prose pairs with the closing tag of the genuine call
            // further down, and the genuine call is then never seen. That silently
            // cost real instructions long before malformed reporting existed.
            i = body_start;
            continue;
        }
        out.push(Found {
            envelope: env,
            block,
            call,
            terminated: end.is_some(),
        });
        let Some(e) = end else { break };
        i = e + env.close.len();
    }
    out
}
