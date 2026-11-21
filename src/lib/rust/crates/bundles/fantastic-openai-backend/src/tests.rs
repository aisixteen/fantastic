use super::*;

fn st() -> SseState {
    SseState { buf: Vec::new(), wrap: ThinkWrap::new(), report: ReportCell::default() }
}

fn tokens(out: &VecDeque<Result<ProviderEvent, ProviderError>>) -> String {
    out.iter()
        .map(|e| match e {
            Ok(ProviderEvent::Token(t)) => t.clone(),
            _ => String::new(),
        })
        .collect()
}

#[test]
fn reasoning_then_content_wraps_the_thought_once() {
    let mut s = st();
    let mut out = VecDeque::new();
    for l in [
        r#"data: {"choices":[{"delta":{"reasoning_content":"let me "}}]}"#,
        r#"data: {"choices":[{"delta":{"reasoning_content":"think"}}]}"#,
        r#"data: {"choices":[{"delta":{"content":"<call name=\"done\"></call>"}}]}"#,
        "data: [DONE]",
    ] {
        decode_line(l.as_bytes(), &mut s, &mut out);
    }
    let text = tokens(&out);
    assert!(text.contains("let me think"), "{text}");
    assert!(text.ends_with("<call name=\"done\"></call>"), "{text}");
    assert_eq!(text.matches("think>").count(), 2, "one open and one close marker: {text}");
}

#[test]
fn raw_mode_ignores_tool_calls_and_keeps_text() {
    let mut s = st();
    let mut out = VecDeque::new();
    let l = r#"data: {"choices":[{"delta":{"content":"hi","tool_calls":[{"id":"x"}]}}]}"#;
    decode_line(l.as_bytes(), &mut s, &mut out);
    assert_eq!(tokens(&out), "hi");
}

#[test]
fn usage_and_length_stop_are_reported() {
    let mut s = st();
    let mut out = VecDeque::new();
    decode_line(br#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#, &mut s, &mut out);
    decode_line(br#"data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3}}"#, &mut s, &mut out);
    let r = s.report.lock().unwrap().clone();
    assert_eq!((r.prompt_tokens, r.completion_tokens), (Some(12), Some(3)));
    assert_eq!(r.stop_reason.as_deref(), Some("max_tokens"));
}

#[test]
fn non_data_lines_are_skipped() {
    let mut s = st();
    let mut out = VecDeque::new();
    for l in [": keep-alive", "event: x", "data:", "data: not json"] {
        decode_line(l.as_bytes(), &mut s, &mut out);
    }
    assert!(out.is_empty());
}

#[test]
fn the_stop_knob_is_a_string_or_a_list_of_strings() {
    let m = |v: Value| v.as_object().unwrap().clone();
    assert_eq!(stop_of(&m(json!({"stop": "<|x|>"}))), Some(json!(["<|x|>"])));
    assert_eq!(stop_of(&m(json!({"stop": ["a", "b"]}))), Some(json!(["a", "b"])));
    assert_eq!(stop_of(&m(json!({"stop": []}))), None);
    assert_eq!(stop_of(&m(json!({"stop": [1]}))), None);
    assert_eq!(stop_of(&m(json!({}))), None);
}

#[test]
fn no_temperature_is_sent_unless_the_chat_names_one() {
    let m = |v: Value| v.as_object().unwrap().clone();
    assert_eq!(m(json!({})).get("temperature").and_then(Value::as_f64), None);
    assert_eq!(m(json!({"temperature": 1.0})).get("temperature").and_then(Value::as_f64), Some(1.0));
    assert!(RECORD.get(&m(json!({})), "temperature").is_none_or(|v| v.is_null()), "no declared default");
}
