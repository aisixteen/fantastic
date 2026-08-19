//! Thinking: PARSED, not PERSISTED.
//!
//! A reasoning model streams two things. ollama labels them `message.thinking` /
//! `message.content`; NIM labels them `reasoning_content` / `content`. We must
//! read BOTH — a turn spent entirely reasoning is otherwise an empty completion
//! that silently ends the chain (measured on gemma4:12b, 2026-08-02). But we
//! must not KEEP both: feeding a reasoning model its own prior deliberation back
//! is against every vendor's guidance, and it compounds — gemma's 3,800-char
//! turn-1 deliberation is re-sent in every later prompt, crowding out the task.
//!
//! So the two uses split. Providers wrap reasoning in these markers; the FULL
//! text is parsed (a tool call the model wrote mid-thought still dispatches) and
//! the STRIPPED text is persisted and shown as the answer. One convention, no
//! provider learns anything about the tool protocol.
//!
//! This lives in its own module because the provider crates (ollama, NIM) import
//! the markers — they are wire vocabulary, not loop internals.

/// Opening marker a provider wraps reasoning-channel text in.
pub const THINK_OPEN: &str = "<think>";

/// Closing marker of a reasoning span.
pub const THINK_CLOSE: &str = "</think>";

/// Drop every `<think>…</think>` span — what goes into durable history.
///
/// An UNTERMINATED span (the stream was cut mid-thought) drops its tail: a
/// half-finished deliberation is not an answer, and keeping it would poison
/// every later prompt.
pub fn without_thinking(text: &str) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    loop {
        let Some(rel_open) = text[i..].find(THINK_OPEN) else {
            out.push_str(&text[i..]);
            break;
        };
        let a = i + rel_open;
        out.push_str(&text[i..a]);
        let Some(rel_close) = text[a..].find(THINK_CLOSE) else {
            break; // unterminated (stream cut mid-thought) — drop the tail
        };
        i = a + rel_close + THINK_CLOSE.len();
    }
    out.trim().to_string()
}

/// Per-TURN reasoning wrapper state, threaded through a provider's chunk decoder.
///
/// The span opens on the FIRST reasoning chunk and closes when content starts or
/// the stream ends — **once per turn, never per chunk**. Per chunk the text would
/// be a confetti of tags and [`without_thinking`] would eat the answer along with
/// the deliberation.
#[derive(Debug, Default)]
pub struct ThinkWrap {
    open: bool,
}

impl ThinkWrap {
    /// A wrapper with no span open. One per provider pass.
    pub const fn new() -> Self {
        Self { open: false }
    }

    /// Reasoning-channel text arrived: `Some(THINK_OPEN)` on the turn's first
    /// reasoning chunk (emit it *before* the text), `None` while already open.
    pub fn open_marker(&mut self) -> Option<&'static str> {
        if self.open {
            return None;
        }
        self.open = true;
        Some(THINK_OPEN)
    }

    /// Content-channel text arrived, or the stream ended: `Some(THINK_CLOSE)`
    /// when a span is open (emit it *before* the content), else `None`.
    pub fn close_marker(&mut self) -> Option<&'static str> {
        if !self.open {
            return None;
        }
        self.open = false;
        Some(THINK_CLOSE)
    }

    /// Whether a reasoning span is currently open.
    pub const fn is_open(&self) -> bool {
        self.open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_a_span_and_keeps_the_surrounding_text() {
        assert_eq!(
            without_thinking("before <think>deliberation</think> after"),
            "before  after"
        );
        // Several spans, and a turn that is nothing but deliberation.
        assert_eq!(
            without_thinking("<think>a</think>one<think>b</think>two"),
            "onetwo"
        );
        assert_eq!(without_thinking("<think>all of it</think>"), "");
        // No markers at all — verbatim (modulo the surrounding trim).
        assert_eq!(without_thinking("  plain answer  "), "plain answer");
    }

    #[test]
    fn a_tool_call_written_inside_a_span_stays_visible_to_the_parser() {
        // The whole point of the split: the RAW text is parsed, the STRIPPED text
        // is persisted. A model that writes its call mid-thought still dispatches.
        let raw = concat!(
            "<think>I should ",
            r#"<tool_call>{"name":"send","arguments":{"payload":{"type":"recall"}}}</tool_call>"#,
            "</think>done"
        );
        let calls = crate::tool_parse::extract_tool_calls(raw);
        assert_eq!(calls.len(), 1, "the parser reads the RAW text: {calls:?}");
        assert_eq!(calls[0].arguments["payload"]["type"], "recall");
        assert_eq!(without_thinking(raw), "done");
    }

    #[test]
    fn an_unterminated_span_drops_its_tail() {
        assert_eq!(
            without_thinking("kept <think>cut off mid-thou"),
            "kept",
            "a stream cut mid-thought must not persist the fragment"
        );
        assert_eq!(without_thinking("<think>only a fragment"), "");
    }

    #[test]
    fn wrap_opens_and_closes_once_per_turn() {
        let mut w = ThinkWrap::new();
        assert_eq!(w.open_marker(), Some(THINK_OPEN));
        assert_eq!(w.open_marker(), None, "second reasoning chunk re-opens");
        assert!(w.is_open());
        assert_eq!(w.close_marker(), Some(THINK_CLOSE));
        assert_eq!(w.close_marker(), None, "closing twice emits a stray tag");
        assert!(!w.is_open());
        // A content-only turn never opens a span.
        let mut w2 = ThinkWrap::new();
        assert_eq!(w2.close_marker(), None);
    }
}
