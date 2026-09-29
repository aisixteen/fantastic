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
            // Unterminated (stream cut mid-thought) — drop the tail, but keep any
            // envelope it already contained: it was dispatched.
            out.push_str(&kept_envelopes(&text[a..]));
            break;
        };
        let span = &text[a..a + rel_close];
        // KEEP THE ENVELOPES, DROP THE REASONING. A `<call>` is an
        // INVOCATION wherever it is written, thinking included — it is parsed out
        // of the full completion and dispatched. Stripping the whole span meant
        // dispatch read the FULL text while history read the STRIPPED text, so a
        // call made inside a thought RAN, returned a reply, and left no record.
        // The model then saw an effect with no cause ("I sent a reflect to
        // file_bridge but the reply came back from exec_bridge" — correct, given
        // a record we had silently edited). 3 of 169 calls were lost in one day.
        out.push_str(&kept_envelopes(span));
        i = a + rel_close + THINK_CLOSE.len();
    }
    out.trim().to_string()
}

/// Every complete tool-call envelope inside `span`, in order, one per line.
///
/// The ACTION taken survives a dropped thought; the reasoning does not. An
/// envelope that opened and never closed is NOT kept, and neither is one that is
/// complete but does not parse: dispatch could not have run it, so it was
/// reasoning too. `call_spans` is the one place that judgement lives.
fn kept_envelopes(span: &str) -> String {
    let found = crate::tool_parse::call_spans(span);
    if found.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n");
    for e in found {
        out.push_str(e);
        out.push('\n');
    }
    out
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
/// channels. `wrap` carries the per-TURN reasoning span across deltas.
///
/// SHARED because more than one backend speaks this wire: NVIDIA NIM and
/// `llama-server` both expose `/v1/chat/completions` with the same streaming
/// shape, so this is a property of the WIRE, not of either vendor. It lived
/// inside the NIM crate until llama.cpp needed it; a copy would have drifted on
/// exactly the detail it exists to get right.
///
/// REASONING MODELS SPLIT THEIR OUTPUT. nemotron/deepseek-style models — and
/// `llama-server` started with `--reasoning-format deepseek` — stream most
/// tokens as `reasoning_content` and only the conclusion as `content`: measured
/// 740 chars of reasoning to 129 of content on one NIM turn, and 602 to 176 on
/// gemma-4-12B. Reading `content` alone silently drops whole turns — observed
/// live, a model spent an entire pass reasoning, an EMPTY completion came back,
/// and the chain died with no error anywhere.
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

/// PROVIDER-REPORTED USAGE off one OpenAI-style streaming chunk — the NIM and
/// `llama-server` wire alike: `usage.prompt_tokens` (on the last chunk, with
/// `stream_options.include_usage`) is what the window really held, and a
/// `finish_reason` of `length` means the output stopped AT its bound
/// (`max_tokens`). Python `NvidiaNimProvider.chat` / `LlamaCppProvider.chat`.
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

    /// PROVIDER-REPORTED USAGE, the OpenAI-compatible shape (nvidia NIM,
    /// llama-server): `usage.prompt_tokens` calibrates the estimate, and a
    /// `finish_reason` of `length` is the output stopping AT its bound.
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
    fn a_call_written_inside_a_span_survives_into_history() {
        // The RAW text is parsed, the STRIPPED text is persisted — so a call the
        // model wrote mid-thought dispatches AND stays in the record.
        //
        // It used to be stripped with the reasoning, and the consequence was a
        // record we had silently edited: the call RAN, returned a reply, and left
        // no cause behind it. The model then read an effect it never caused ("I
        // sent a reflect to file_bridge but the reply came back from
        // exec_bridge"). Reasoning is still dropped; the INVOCATION is not.
        let envelope = r#"<call name="send"><target_id>self</target_id><type>recall</type></call>"#;
        let raw = format!("<think>I should {envelope}</think>done");

        let calls = crate::tool_parse::extract_tool_calls(&raw);
        assert_eq!(calls.len(), 1, "the parser reads the RAW text: {calls:?}");
        assert_eq!(calls[0].arguments["payload"]["type"], "recall");
        assert_eq!(calls[0].arguments["target_id"], "self");

        let kept = without_thinking(&raw);
        assert!(
            kept.contains(envelope),
            "the invocation was erased: {kept:?}"
        );
        assert!(kept.contains("done"));
        assert!(!kept.contains("I should"), "the reasoning must still go");
        // ...and what we persisted still parses to the same one call, so history
        // and dispatch agree.
        assert_eq!(crate::tool_parse::extract_tool_calls(&kept).len(), 1);
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
