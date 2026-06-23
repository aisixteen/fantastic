//! RAW tool-call parsing — the ONE shared layer that owns tool-calling.
//!
//! Fantastic NEVER uses a provider's native tool API. Providers are pure
//! raw-text streamers ([`ProviderEvent::Token`] only); THIS module wraps that
//! stream and extracts the `send` tool calls from the text. Used by
//! [`crate::agent_loop`] for every backend.
//!
//! The envelope (Hermes-style — widely trained, unambiguous, stream-friendly):
//!
//! ```text
//! <tool_call>{"name": "send", "arguments": {"target_id": "...", "payload": {...}}}</tool_call>
//! ```
//!
//! Text OUTSIDE the tags is content shown to the user. Tags may repeat. The
//! parser yields the SAME [`ProviderEvent`]s the loop already consumes; a
//! pre-formed [`ProviderEvent::ToolCall`] (e.g. from a test) passes through
//! untouched — real providers never emit one.

use crate::provider::{ProviderEvent, ProviderStream};
use futures_util::stream::{self, StreamExt};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

/// Opening tag of the tool-call envelope.
pub const OPEN: &str = "<tool_call>";
/// Closing tag of the tool-call envelope.
pub const CLOSE: &str = "</tool_call>";

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn mint_id() -> String {
    format!("call_{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Serialize a call into the envelope — used in the prompt example and when
/// persisting an assistant turn so the model re-reads its own call as text.
pub fn render_tool_call(name: &str, args: &Value) -> String {
    let obj = json!({"name": name, "arguments": args});
    format!(
        "{OPEN}{}{CLOSE}",
        serde_json::to_string(&obj).unwrap_or_default()
    )
}

/// Parse the JSON between one tag pair into `(name, arguments)`.
///
/// Lenient (tiny models drift): accepts `{"name","arguments"}`, a `tool` alias
/// for `name`, a flattened object (remaining keys become arguments), and a
/// double-encoded (stringified) arguments value. Returns `None` on unparseable
/// JSON — the caller surfaces the raw text as content so nothing is lost.
pub fn parse_one(inner: &str) -> Option<(String, Value)> {
    let parsed: Value = serde_json::from_str(inner.trim()).ok()?;
    let obj = parsed.as_object()?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| obj.get("tool").and_then(Value::as_str))
        .unwrap_or("send")
        .to_string();
    let args = match obj.get("arguments") {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        _ => {
            // flattened: remaining keys become the arguments object
            let mut m = Map::new();
            for (k, v) in obj.iter() {
                if k != "name" && k != "tool" && k != "arguments" {
                    m.insert(k.clone(), v.clone());
                }
            }
            Value::Object(m)
        }
    };
    let args = if args.is_object() { args } else { json!({}) };
    Some((name, args))
}

/// Non-streaming: pull every finalized `<tool_call>` out of a complete string.
/// Used by the durable-history reader (the compaction reaction).
pub fn extract_tool_calls(text: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(a) = text[i..].find(OPEN) {
        let start = i + a + OPEN.len();
        let Some(brel) = text[start..].find(CLOSE) else {
            break;
        };
        if let Some(c) = parse_one(&text[start..start + brel]) {
            out.push(c);
        }
        i = start + brel + CLOSE.len();
    }
    out
}

/// Longest suffix of `buf` that is a proper prefix of OPEN (a tag possibly split
/// across chunks) — hold it back rather than emit it as content. OPEN is ASCII,
/// so byte-prefix matching is char-boundary safe.
fn partial_open_len(buf: &str) -> usize {
    let max = std::cmp::min(buf.len(), OPEN.len() - 1);
    for k in (1..=max).rev() {
        if buf.as_bytes().ends_with(&OPEN.as_bytes()[..k]) {
            return k;
        }
    }
    0
}

struct PState {
    inner: ProviderStream,
    buf: String,
    inside: bool,
    pending: VecDeque<Result<ProviderEvent, String>>,
    inner_done: bool,
    flushed: bool,
}

/// Process the current buffer: emit content tokens for text outside tags and
/// `ToolCall` events for closed tags; hold a partial open tag at the tail.
fn process_buf(st: &mut PState) {
    loop {
        if !st.inside {
            if let Some(idx) = st.buf.find(OPEN) {
                if idx > 0 {
                    st.pending
                        .push_back(Ok(ProviderEvent::Token(st.buf[..idx].to_string())));
                }
                st.buf = st.buf[idx + OPEN.len()..].to_string();
                st.inside = true;
            } else {
                let hold = partial_open_len(&st.buf);
                let emit_len = st.buf.len() - hold;
                if emit_len > 0 {
                    st.pending
                        .push_back(Ok(ProviderEvent::Token(st.buf[..emit_len].to_string())));
                }
                st.buf = st.buf[emit_len..].to_string();
                break;
            }
        } else if let Some(cidx) = st.buf.find(CLOSE) {
            let inner = st.buf[..cidx].to_string();
            st.buf = st.buf[cidx + CLOSE.len()..].to_string();
            st.inside = false;
            match parse_one(&inner) {
                Some((name, args)) => st.pending.push_back(Ok(ProviderEvent::ToolCall {
                    id: mint_id(),
                    name,
                    args,
                })),
                None => st
                    .pending
                    .push_back(Ok(ProviderEvent::Token(format!("{OPEN}{inner}{CLOSE}")))),
            }
        } else {
            break; // need more to close the tag
        }
    }
}

/// Wrap a provider stream → content tokens + finalized tool-calls. Buffers
/// across chunks (a tag may split mid-token); malformed JSON inside a tag (or
/// an unterminated tag at EOF) is surfaced as content, never dropped.
pub fn parse_tool_calls(inner: ProviderStream) -> ProviderStream {
    let st = PState {
        inner,
        buf: String::new(),
        inside: false,
        pending: VecDeque::new(),
        inner_done: false,
        flushed: false,
    };
    Box::pin(stream::unfold(st, |mut st| async move {
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((ev, st));
            }
            if st.inner_done {
                if !st.flushed {
                    st.flushed = true;
                    if st.inside {
                        st.pending
                            .push_back(Ok(ProviderEvent::Token(format!("{OPEN}{}", st.buf))));
                        st.buf.clear();
                    } else if !st.buf.is_empty() {
                        st.pending
                            .push_back(Ok(ProviderEvent::Token(std::mem::take(&mut st.buf))));
                    }
                    continue;
                }
                return None;
            }
            match st.inner.next().await {
                // Pre-formed event (tests / a future structured source) passes through;
                // real providers only yield Token. Flush buffered content first.
                Some(Ok(ev @ ProviderEvent::ToolCall { .. })) => {
                    if !st.inside && !st.buf.is_empty() {
                        st.pending
                            .push_back(Ok(ProviderEvent::Token(std::mem::take(&mut st.buf))));
                    }
                    st.pending.push_back(Ok(ev));
                }
                Some(Ok(ProviderEvent::Token(t))) => {
                    st.buf.push_str(&t);
                    process_buf(&mut st);
                }
                Some(Err(e)) => st.pending.push_back(Err(e)),
                None => st.inner_done = true,
            }
        }
    }))
}

#[cfg(test)]
mod parse_tests {
    use super::*;
    use futures_util::StreamExt;

    fn raw(chunks: Vec<&str>) -> ProviderStream {
        let evs: Vec<Result<ProviderEvent, String>> = chunks
            .into_iter()
            .map(|c| Ok(ProviderEvent::Token(c.to_string())))
            .collect();
        Box::pin(futures_util::stream::iter(evs))
    }

    async fn drain(s: ProviderStream) -> (String, Vec<Value>) {
        let mut content = String::new();
        let mut calls = Vec::new();
        let mut s = s;
        while let Some(ev) = s.next().await {
            match ev.unwrap() {
                ProviderEvent::Token(t) => content.push_str(&t),
                ProviderEvent::ToolCall { name, args, .. } => {
                    calls.push(json!({"name": name, "args": args}))
                }
            }
        }
        (content, calls)
    }

    #[test]
    fn parse_one_canonical_flattened_stringified_malformed() {
        let (n, a) = parse_one(
            r#"{"name":"send","arguments":{"target_id":"core","payload":{"type":"list_agents"}}}"#,
        )
        .unwrap();
        assert_eq!(n, "send");
        assert_eq!(a["target_id"], "core");
        // flattened + `tool` alias
        let (n, a) =
            parse_one(r#"{"tool":"send","target_id":"foo","payload":{"type":"reflect"}}"#).unwrap();
        assert_eq!(n, "send");
        assert_eq!(a["target_id"], "foo");
        // stringified arguments
        let (_, a) = parse_one(r#"{"name":"send","arguments":"{\"target_id\":\"x\"}"}"#).unwrap();
        assert_eq!(a["target_id"], "x");
        // malformed
        assert!(parse_one("{not json").is_none());
        assert!(parse_one("[1,2,3]").is_none());
    }

    #[tokio::test]
    async fn plain_text_no_calls() {
        let (c, calls) = drain(parse_tool_calls(raw(vec!["Hello ", "world"]))).await;
        assert_eq!(c, "Hello world");
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn single_call_and_prose() {
        let (c, calls) = drain(parse_tool_calls(raw(vec![
            "ok ",
            r#"<tool_call>{"name":"send","arguments":{"target_id":"core","payload":{"type":"reflect"}}}</tool_call>"#,
        ])))
        .await;
        assert_eq!(c, "ok ");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["args"]["target_id"], "core");
    }

    #[tokio::test]
    async fn tag_split_across_single_char_chunks() {
        let full = r#"<tool_call>{"name":"send","arguments":{"target_id":"core","payload":{"type":"list_agents"}}}</tool_call>"#;
        let chunks: Vec<&str> = full.split("").filter(|s| !s.is_empty()).collect();
        let (c, calls) = drain(parse_tool_calls(raw(chunks))).await;
        assert_eq!(c, "");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["args"]["target_id"], "core");
    }

    #[tokio::test]
    async fn multiple_calls_one_stream() {
        let a =
            r#"<tool_call>{"name":"send","arguments":{"target_id":"a","payload":{}}}</tool_call>"#;
        let b =
            r#"<tool_call>{"name":"send","arguments":{"target_id":"b","payload":{}}}</tool_call>"#;
        let (_, calls) = drain(parse_tool_calls(raw(vec![a, "\n", b]))).await;
        let ids: Vec<&str> = calls
            .iter()
            .map(|c| c["args"]["target_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn malformed_and_unterminated_surface_as_content() {
        let (c, calls) = drain(parse_tool_calls(raw(vec!["<tool_call>{nope}</tool_call>"]))).await;
        assert!(calls.is_empty());
        assert_eq!(c, "<tool_call>{nope}</tool_call>");
        let (c2, calls2) = drain(parse_tool_calls(raw(vec![r#"<tool_call>{"name":"send""#]))).await;
        assert!(calls2.is_empty());
        assert!(c2.starts_with("<tool_call>"));
    }

    #[tokio::test]
    async fn lone_angle_bracket_not_held() {
        let (c, calls) = drain(parse_tool_calls(raw(vec!["a < b ", "and c"]))).await;
        assert!(calls.is_empty());
        assert_eq!(c, "a < b and c");
    }

    #[tokio::test]
    async fn preformed_event_passthrough() {
        let s: ProviderStream = Box::pin(futures_util::stream::iter(vec![
            Ok(ProviderEvent::Token("hi ".into())),
            Ok(ProviderEvent::ToolCall {
                id: "x".into(),
                name: "send".into(),
                args: json!({"target_id": "core"}),
            }),
        ]));
        let (c, calls) = drain(parse_tool_calls(s)).await;
        assert_eq!(c, "hi ");
        assert_eq!(calls[0]["args"]["target_id"], "core");
    }

    #[test]
    fn extract_and_render_round_trip() {
        let s = render_tool_call(
            "send",
            &json!({"target_id":"core","payload":{"type":"list_agents"}}),
        );
        let calls = extract_tool_calls(&s);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["target_id"], "core");
    }
}
