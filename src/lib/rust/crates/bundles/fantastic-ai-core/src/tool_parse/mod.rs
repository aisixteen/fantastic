//! Tool-call parsing — the text shapes a model may use to name a `send`.
//!
//! Ported from python `ai_core/tool_parse.py`, which is canonical. TWO
//! consumers, here as there:
//!
//!  - [`crate::route::route_reply`] — every completion is scanned and each call
//!    found is dispatched, with its reply delivered to its own `reply_to`. This
//!    is the path a chain continues on. A shape this module does not recognize is
//!    a turn that silently does nothing, which is why
//!    [`malformed_calls`] exists.
//!  - [`crate::projection::reaction_from`] — scans the DURABLE transcript for
//!    whether the model reacted to a compaction notice (e.g. by emitting a
//!    `recall`/persist call).
//!
//! So this module is LOAD-BEARING on the routing path, not just a read-model.
//! (This header said otherwise until 2026-08-06, when routing landed and the
//! second consumer stopped being the only one — one stale copy of a fact is
//! worse than none.) Every failure recorded in it was MEASURED against a live
//! model.
//!
//! There is NO agentic loop here and NO native provider tool API: the provider
//! streams plain text, this module reads it. Three envelopes are recognized,
//! because a model's trained format beats our instruction and refusing a shape
//! costs a whole run (each is documented in its own module):
//!
//! ```text
//! <tool_call>{"name":"send","arguments":{…}}</tool_call>     taught (Hermes-style)
//! <invoke name="send"><parameter …>…</parameter></invoke>    Claude's own
//! <|tool_call>call:<name>{…}<tool_call|>                     special-token channel
//! ```

pub mod channel;
pub mod hints;
pub mod invoke_envelope;
pub mod json_envelope;
pub mod scan;
pub mod shape;

#[cfg(test)]
mod tests_channel;
#[cfg(test)]
mod tests_invoke;
#[cfg(test)]
mod tests_json;
#[cfg(test)]
mod tests_malformed;
#[cfg(test)]
mod tests_scan;
#[cfg(test)]
mod tests_send_binary;

use hints::{defang, CHANNEL_HINT, JSON_HINT};
use scan::{scan, Envelope};
use serde_json::Value;

pub use channel::{CHANNEL_CLOSE, CHANNEL_OPEN};
pub use invoke_envelope::{INVOKE_CLOSE, INVOKE_OPEN};
pub use json_envelope::{parse_one, CLOSE, OPEN};
pub use shape::{mint_call_id, ToolCall};

/// The envelope table: opener → (closer, body parser, shape test, hint). One
/// table, so the scanner has no per-envelope branches to keep in step and a
/// fourth envelope is one entry.
///
/// ORDER IS LOAD-BEARING: [`scan::scan`] breaks a positional tie by table order,
/// so channel comes before invoke comes before taught, as in python.
pub static ENVELOPES: [Envelope; 3] = [
    Envelope {
        open: CHANNEL_OPEN,
        close: CHANNEL_CLOSE,
        parse: channel::parse_channel_call,
        is_attempt: channel::looks_like_an_attempt,
        hint: CHANNEL_HINT,
    },
    Envelope {
        open: INVOKE_OPEN,
        close: INVOKE_CLOSE,
        parse: invoke_envelope::parse_invoke,
        is_attempt: invoke_envelope::looks_like_an_attempt,
        hint: JSON_HINT,
    },
    Envelope {
        open: OPEN,
        close: CLOSE,
        parse: json_envelope::parse_one,
        is_attempt: json_envelope::looks_like_an_attempt,
        hint: JSON_HINT,
    },
];

/// Non-streaming: pull every finalized call out of a complete string. Used by the
/// durable-history reader ([`crate::projection::reaction_from`]).
pub fn extract_tool_calls(text: &str) -> Vec<ToolCall> {
    scan(text).into_iter().filter_map(|f| f.call).collect()
}

/// An envelope that OPENED and did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedCall {
    /// The whole reply body, composed at the parser so the wording has ONE owner:
    /// the router delivers this verbatim and does not assemble a second version of
    /// the same sentence.
    pub error: String,
    /// Which envelope opened (its opening delimiter).
    pub envelope: &'static str,
    /// Enough of the model's own block to locate the break, bounded so a huge
    /// program body cannot itself blow the context it is being explained in, and
    /// DEFANGED so quoting this error cannot arm the scanner again.
    pub your_block: String,
}

impl MalformedCall {
    /// The report as the model receives it — the body
    /// [`crate::route::route_reply`] delivers verbatim. Shaped HERE because the
    /// wording is this module's (see [`MalformedCall::error`]); the router
    /// serializes it and does not compose a second version of the same sentence.
    pub fn report(&self) -> Value {
        serde_json::json!({
            "error": self.error,
            "envelope": self.envelope,
            "your_block": self.your_block,
        })
    }
}

/// Envelopes that opened and did not parse.
///
/// Exists because a call that cannot be read is the one failure a model cannot
/// debug: [`extract_tool_calls`] returns `[]` for it, which is byte-identical to a
/// turn that emitted no call at all — and that means "done". Measured on
/// gemma-4-12B (2026-08-06): its last pass left the `code` string unclosed, nothing
/// parsed, nothing dispatched, no error came back, and the run ended looking
/// finished with the task untouched. Five earlier `no target_id` errors had TAUGHT
/// it — it self-corrected out loud — so the fix is not a better parser, it is
/// giving this failure the same voice the others already have.
pub fn malformed_calls(text: &str) -> Vec<MalformedCall> {
    let mut out = Vec::new();
    for found in scan(text) {
        if found.call.is_some() {
            continue;
        }
        if !found.envelope.looks_like_an_attempt(found.block) {
            // An opener whose body is not call-shaped is the model TALKING about
            // the envelope, not using it. Reporting those cost gemma-4-12B a turn
            // on 2026-08-06: it wrote the tag inside a sentence and got an error
            // about JSON.
            //
            // `terminated` is NOT the test, though the first version of this guard
            // used it. A stray opener in prose plus the closing tag of a REAL call
            // further down the completion makes a "terminated" block out of a
            // sentence, so requiring the closer let prose through — and then the
            // loop closed: this error echoes `your_block`, the model quotes the
            // error while reasoning about it, its quotation contains the tag, and
            // the next scan finds a fresh opener. Three of run 4's errors were that
            // loop feeding itself, not the model. Shape decides, always.
            continue;
        }
        let body = found.block.trim();
        let mut reason = "the body did not parse".to_string();
        if found.envelope.open == OPEN {
            // Diagnose the grammar the model ACTUALLY used — the strict-JSON
            // error for a raw/fenced attempt sends it debugging the wrong thing
            // (measured 2026-09-01, python: a raw-form call one brace short was
            // told "Invalid control character" and reasoned about newlines for
            // four turns while the write never landed).
            if body.starts_with("*** Begin Bytes") || body.contains("\n*** Begin Bytes") {
                let meta = body.split("*** Begin Bytes").next().unwrap_or("");
                reason = match serde_json::from_str::<Value>(meta.trim()) {
                    Ok(_) => "the fenced call did not resolve".to_string(),
                    Err(exc) => format!(
                        "the metadata JSON above `*** Begin Bytes` did not parse — {exc}. \
                         It must be ONE complete, small JSON call object (every brace \
                         closed) with \"@body\" where the program goes; the program \
                         itself is fine, it rides verbatim between the markers"
                    ),
                };
            } else if body.contains("\"send_binary\"") && body.contains("\"bytes\"") {
                reason = "the send_binary raw lift failed — the JSON metadata around \
                          `bytes` must be valid and after the program's closing quote \
                          nothing may follow but the braces that close that metadata \
                          EXACTLY (count them)"
                    .to_string();
            } else if let Err(exc) = serde_json::from_str::<Value>(body) {
                reason = format!("invalid JSON — {exc}");
            }
        }
        out.push(MalformedCall {
            error: format!(
                "tool_call NOT dispatched: {reason}. Nothing ran. {}",
                found.envelope.hint
            ),
            envelope: found.envelope.open,
            // 400 CHARACTERS, not bytes — a truncated multi-byte sequence would be
            // unreadable to the model that has to fix it. Defanging is char-for-char,
            // so the cap survives it.
            your_block: defang(&body.chars().take(400).collect::<String>()),
        });
    }
    out
}
