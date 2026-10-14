//! Thinking: neither DISPATCHED nor PERSISTED — only the answer acts.
//!
//! A reasoning model streams two things. ollama labels them `message.thinking` /
//! `message.content`; vLLM labels them `reasoning_content` / `content`. We must
//! read BOTH — a turn spent entirely reasoning is otherwise an empty completion
//! that silently ends the chain. But we
//! must not KEEP both: feeding a reasoning model its own prior deliberation back
//! is against every vendor's guidance, and it compounds — a long early
//! deliberation is re-sent in every later prompt, crowding out the task.
//!
//! Providers wrap reasoning in these markers, and everything downstream reads the
//! ANSWER — the text with every span dropped whole: dispatch parses it and history
//! persists it. A `<call>` written inside thinking is thinking: a call drafted in
//! thought either copies an answer call (the action would run twice) or is a
//! draft the model never wrote in its answer. One convention, no provider learns
//! anything about the tool protocol.
//!
//! This lives in its own module because the provider crates (ollama, vllm) import
//! the markers — they are wire vocabulary, not loop internals.

/// Opening marker a provider wraps reasoning-channel text in.
pub const THINK_OPEN: &str = "<think>";

/// Closing marker of a reasoning span.
pub const THINK_CLOSE: &str = "</think>";

/// Characters of a generation's thinking kept on its assistant row
/// (`thinking`); what is past them is counted in `thinking_cut`. Python
/// `THINKING_CAP`.
pub const THINKING_CAP: usize = 32000;

/// A generation's THINKING, for its assistant row (a chat shows it collapsed
/// under the answer, after a reload too): every
/// `<think>…</think>` span of `text` (an unclosed one runs to the end), joined by
/// a blank line — `{"thinking": …}`, the first [`THINKING_CAP`] characters, plus
/// `{"thinking_cut": n}` when n more were dropped; empty when there was none.
/// NEVER the model's view (it reads role + content only) and never dispatched.
/// Python `thinking_fields`.
#[must_use]
pub fn thinking_fields(text: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut spans: Vec<&str> = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = text[i..].find(THINK_OPEN) {
        let a = i + rel + THINK_OPEN.len();
        match text[a..].find(THINK_CLOSE) {
            Some(relb) => {
                spans.push(fantastic_bundle::py_strip(&text[a..a + relb]));
                i = a + relb + THINK_CLOSE.len();
            }
            None => {
                spans.push(fantastic_bundle::py_strip(&text[a..]));
                break;
            }
        }
    }
    let thought = spans
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut out = serde_json::Map::new();
    if thought.is_empty() {
        return out;
    }
    let total = thought.chars().count();
    out.insert(
        "thinking".into(),
        serde_json::Value::String(thought.chars().take(THINKING_CAP).collect()),
    );
    if total > THINKING_CAP {
        out.insert(
            "thinking_cut".into(),
            serde_json::json!(total - THINKING_CAP),
        );
    }
    out
}

/// The ANSWER: every `<think>…</think>` span dropped whole — what dispatch reads
/// and what goes into durable history. An UNTERMINATED span runs to the end: the
/// rest is thinking. Python `without_thinking`.
#[must_use]
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
            break;
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

/// Decode ONE OpenAI-style streaming `delta` into text pieces, reading BOTH
/// channels. `wrap` carries the per-TURN reasoning span across deltas. A
/// property of the `/v1/chat/completions` streaming WIRE, not of a vendor.
///
/// REASONING MODELS SPLIT THEIR OUTPUT. nemotron/deepseek-style models stream
/// most tokens as `reasoning_content` and only the conclusion as `content`.
/// Reading `content` alone silently drops whole turns — a model that spends an
/// entire pass reasoning returns an EMPTY completion, and the chain dies with no
/// error anywhere.
pub fn decode_openai_delta(delta: &serde_json::Value, wrap: &mut ThinkWrap) -> Vec<String> {
    let mut out = Vec::new();
    let think = delta
        .get("reasoning_content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !think.is_empty() {
        if let Some(m) = wrap.open_marker() {
            out.push(m.to_string());
        }
        out.push(think.to_string());
    }
    let content = delta
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !content.is_empty() {
        if let Some(m) = wrap.close_marker() {
            out.push(m.to_string());
        }
        out.push(content.to_string());
    }
    out
}

/// PROVIDER-REPORTED USAGE off one OpenAI-style streaming chunk:
/// `usage.prompt_tokens` (on the last chunk, with
/// `stream_options.include_usage`) is what the window really held, and a
/// `finish_reason` of `length` means the output stopped AT its bound
/// (`max_tokens`). Python `VllmProvider.chat`.
pub fn note_openai_usage(chunk: &serde_json::Value, report: &crate::provider::ReportCell) {
    let mut r = report.lock().expect("report poisoned");
    if let Some(n) = chunk
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .filter(|v| fantastic_bundle::py_truthy(v))
        .and_then(serde_json::Value::as_i64)
    {
        r.prompt_tokens = Some(n);
    }
    if let Some(n) = chunk
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .filter(|v| fantastic_bundle::py_truthy(v))
        .and_then(serde_json::Value::as_i64)
    {
        r.completion_tokens = Some(n);
    }
    let fr = chunk
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("finish_reason"))
        .filter(|v| fantastic_bundle::py_truthy(v));
    if let Some(fr) = fr {
        r.stop_reason = Some(if fr.as_str() == Some("length") {
            "max_tokens".to_string()
        } else {
            fantastic_bundle::py_str(fr)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PROVIDER-REPORTED USAGE, the OpenAI-compatible shape (vllm):
    /// `usage.prompt_tokens` calibrates the estimate, and a `finish_reason` of
    /// `length` is the output stopping AT its bound.
    #[test]
    fn openai_usage_reports_prompt_tokens_and_a_cut_as_max_tokens() {
        let report = crate::provider::ReportCell::default();
        note_openai_usage(
            &serde_json::json!({"choices": [{"delta": {"content": "x"}, "finish_reason": null}]}),
            &report,
        );
        assert_eq!(
            *report.lock().unwrap(),
            crate::provider::ProviderReport::default()
        );
        note_openai_usage(
            &serde_json::json!({"choices": [{"finish_reason": "length"}], "usage": {"prompt_tokens": 1234}}),
            &report,
        );
        let r = report.lock().unwrap().clone();
        assert_eq!(r.prompt_tokens, Some(1234));
        assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
        note_openai_usage(
            &serde_json::json!({"choices": [{"finish_reason": "stop"}]}),
            &report,
        );
        assert_eq!(report.lock().unwrap().stop_reason.as_deref(), Some("stop"));
    }

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
    fn a_call_written_inside_a_span_is_neither_dispatched_nor_stored() {
        // ONLY THE ANSWER ACTS: dispatch and history both read
        // the answer, so a call inside a thought does neither.
        let envelope = r#"<call name="send"><target_id>self</target_id><type>recall</type></call>"#;
        let kept = without_thinking(&format!("<think>I should {envelope}</think>done"));
        assert_eq!(kept, "done");
        assert!(crate::tool_parse::extract_tool_calls(&kept).is_empty());
        // ...an unterminated span runs to the end, its call included
        assert_eq!(
            without_thinking(&format!("visible<think>cut mid {envelope}")),
            "visible"
        );
        // ...and only the answer's own call counts
        let both = without_thinking(&format!("<think>a {envelope} b</think>{envelope}"));
        assert_eq!(crate::tool_parse::extract_tool_calls(&both).len(), 1);
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

#[cfg(test)]
mod thinking_fields_tests {
    use super::thinking_fields;
    use serde_json::json;

    /// python `thinking_fields`: every span, joined by a blank line; none → empty.
    #[test]
    fn thinking_is_every_span_joined() {
        assert_eq!(
            serde_json::Value::Object(thinking_fields("<think>a\nb</think>answer")),
            json!({"thinking": "a\nb"})
        );
        assert_eq!(
            serde_json::Value::Object(thinking_fields("<think>x</think>mid<think>y")),
            json!({"thinking": "x\n\ny"})
        );
        assert!(thinking_fields("plain").is_empty());
        let long = format!("<think>{}</think>", "z".repeat(super::THINKING_CAP + 5));
        assert_eq!(thinking_fields(&long)["thinking_cut"], json!(5));
    }
}
