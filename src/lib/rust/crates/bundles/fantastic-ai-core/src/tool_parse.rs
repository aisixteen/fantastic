//! Tool-call parsing — retained ONLY for the context-reaction read-model.
//!
//! The backend no longer runs an agentic loop and does NOT parse tool calls at
//! send time: the loop is emergent (an authored agent drives tool dispatch).
//! What remains is the non-streaming [`extract_tool_calls`], used by
//! [`crate::projection`]'s reaction reader to scan the DURABLE transcript for
//! whether the model reacted to a compaction notice (e.g. by emitting a
//! `recall`/persist call). The envelope it recognizes (Hermes-style):
//!
//! ```text
//! <tool_call>{"name": "send", "arguments": {"target_id": "...", "payload": {...}}}</tool_call>
//! ```

use serde_json::{json, Map, Value};

/// Opening tag of the tool-call envelope.
pub const OPEN: &str = "<tool_call>";
/// Closing tag of the tool-call envelope.
pub const CLOSE: &str = "</tool_call>";

/// Parse the JSON between one tag pair into `(name, arguments)`.
///
/// Lenient (tiny models drift): accepts `{"name","arguments"}`, a `tool` alias
/// for `name`, a flattened object (remaining keys become arguments), and a
/// double-encoded (stringified) arguments value. Returns `None` on unparseable JSON.
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
/// Used by the durable-history reader (the compaction reaction read-model).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_one_canonical_and_lenient() {
        let (n, a) =
            parse_one(r#"{"name":"send","arguments":{"target_id":"core","payload":{"type":"x"}}}"#)
                .unwrap();
        assert_eq!(n, "send");
        assert_eq!(a["target_id"], "core");
        // tool alias + flattened
        let (n2, a2) = parse_one(r#"{"tool":"send","target_id":"foo"}"#).unwrap();
        assert_eq!(n2, "send");
        assert_eq!(a2["target_id"], "foo");
        // stringified arguments
        let (_, a3) = parse_one(r#"{"name":"send","arguments":"{\"target_id\":\"x\"}"}"#).unwrap();
        assert_eq!(a3["target_id"], "x");
        assert!(parse_one("{not json").is_none());
    }

    #[test]
    fn extract_pulls_every_call_ignores_bad() {
        let text = concat!(
            "doing ",
            r#"<tool_call>{"name":"send","arguments":{"payload":{"type":"recall"}}}</tool_call>"#,
            " and ",
            r#"<tool_call>{"name":"send","arguments":{"payload":{"type":"set"}}}</tool_call>"#,
        );
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1["payload"]["type"], "recall");
        assert!(extract_tool_calls("<tool_call>{unterminated").is_empty());
    }
}
