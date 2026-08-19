//! A SECOND text envelope, because a model's trained format beats our
//! instruction. Claude models routinely emit their own shape instead of the
//! taught one:
//!
//! ```text
//! <invoke name="send">
//!   <parameter name="arguments">{"target_id":"notes", …, "reply_to":"self"}</parameter>
//! </invoke>
//! ```
//!
//! with the arguments perfectly correct — only the wrapper differs. Refusing it
//! dropped whole runs at pass 1 (observed on claude-haiku-4.5), after which the
//! model, never receiving a reply, invented the entire remaining session.
//!
//! This is STILL raw prompt-and-parse: it is plain text the model typed. There is
//! no `tools=[]`, no `tool_choice`, no native `tool_use` block — the provider is
//! untouched and knows nothing. Parsing more text shapes is exactly this layer's
//! job.
//!
//! Ported from python `tool_parse.py:33-81`.

use super::shape::ToolCall;
use serde_json::{Map, Value};

/// Opening tag of the `<invoke …>` envelope. Deliberately unterminated: the tag
/// carries attributes, so the opener ends at the name, not at `>`.
pub const INVOKE_OPEN: &str = "<invoke";
/// Closing tag of the `<invoke …>` envelope.
pub const INVOKE_CLOSE: &str = "</invoke>";

/// Parse one `<invoke name="…">…</invoke>` body into a [`ToolCall`].
///
/// The body arrives WITHOUT the opener, so it begins mid-tag: everything up to the
/// first `>` is the attribute head, the rest is the `<parameter>` children.
///
/// `name` defaults to `send`, which is the only tool there is — a missing or empty
/// attribute names it rather than failing the parse.
pub fn parse_invoke(block: &str) -> Option<ToolCall> {
    // python `head, _, body = block.partition(">")`: no `>` at all leaves the whole
    // block as the head and an EMPTY body, which then yields no parameters and no
    // call. Mirrored, because a truncated opening tag has no arguments to read.
    let (head, body) = match block.find('>') {
        Some(p) => (&block[..p], &block[p + 1..]),
        None => (block, ""),
    };
    let name = match attr_name(head) {
        // `(m.group(1) if m else "") or "send"` — an EMPTY attribute value falls
        // through to the default too, not just an absent one.
        Some(n) if !n.is_empty() => n.to_string(),
        _ => "send".to_string(),
    };
    let params = parameters(body);
    if params.is_empty() {
        return None;
    }
    let mut args = Map::new();
    // The `arguments` parameter carries the whole canonical argument object as one
    // JSON blob — the shape claude-haiku-4.5 emits. Anything that is not an object
    // once decoded contributes nothing, and the remaining parameters still get their
    // chance below.
    if let Some((_, blob)) = params.iter().find(|(k, _)| k == "arguments") {
        if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(blob) {
            args = parsed;
        }
    }
    // Remaining `<parameter name="x">` pairs are arguments too (models split them
    // either way). JSON-decode each value so objects/ints survive; else keep the
    // text. These are applied AFTER the blob, so a split-out parameter wins.
    for (k, v) in &params {
        if k == "arguments" {
            continue;
        }
        let value = serde_json::from_str::<Value>(v).unwrap_or_else(|_| Value::String(v.clone()));
        args.insert(k.clone(), value);
    }
    if args.is_empty() {
        return None;
    }
    Some(ToolCall::new(name, args))
}

/// The `name="…"` attribute value, from the tag head.
///
/// Hand-written scanner: there is no `regex` dependency in this workspace, and
/// python's parser is hand-written too. Mirrors
/// `re.compile(r'name\s*=\s*"([^"]*)"').search(head)` — UNANCHORED, so a match may
/// start anywhere in the head and a failed attempt resumes one character later,
/// exactly as a regex engine's scan does.
fn attr_name(head: &str) -> Option<&str> {
    let mut from = 0usize;
    while let Some(rel) = head[from..].find("name") {
        let at = from + rel;
        if let Some(found) = attr_name_at(head, at) {
            return Some(found);
        }
        from = at + 1;
    }
    None
}

/// One `name\s*=\s*"([^"]*)"` attempt, anchored at `at`.
fn attr_name_at(head: &str, at: usize) -> Option<&str> {
    let mut i = skip_ws(head, at + "name".len());
    if !head[i..].starts_with('=') {
        return None;
    }
    i = skip_ws(head, i + 1);
    if !head[i..].starts_with('"') {
        return None;
    }
    i += 1;
    let close = head[i..].find('"')?;
    Some(&head[i..i + close])
}

/// Every `<parameter name="k">v</parameter>` child as `(name, TRIMMED value)` in
/// document order (python `{k: v.strip() for k, v in _INVOKE_PARAM.findall(body)}`).
///
/// Mirrors `r'<parameter\s+name\s*=\s*"([^"]*)"\s*>(.*?)</parameter>'` under
/// `re.DOTALL` — the value may span newlines, and `.*?` is non-greedy, so the FIRST
/// `</parameter>` closes it.
///
/// A `Vec` rather than a map because python's comprehension is a DICT: order is
/// insertion order, and a repeated name keeps its first POSITION with its last
/// VALUE. `serde_json::Map` only exists for `Value`, and a `BTreeMap` would sort the
/// parameters, so the dict is modelled directly.
fn parameters(body: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = body[from..].find("<parameter") {
        let at = from + rel;
        match parameter_at(body, at) {
            Some((key, value, end)) => {
                let value = value.trim().to_string();
                match out.iter_mut().find(|(k, _)| k == key) {
                    Some(slot) => slot.1 = value,
                    None => out.push((key.to_string(), value)),
                }
                from = end;
            }
            // A failed match resumes ONE CHARACTER on, not past the whole tag: a
            // `<parameter` that is not a well-formed child must not hide a real one
            // that starts inside it. This is what the regex engine's scan does.
            None => from = at + 1,
        }
    }
    out
}

/// One `<parameter …>…</parameter>` attempt anchored at `at`, as
/// `(name, raw value, index just past the closing tag)`.
fn parameter_at(body: &str, at: usize) -> Option<(&str, &str, usize)> {
    let mut i = at + "<parameter".len();
    // `\s+` — at least one space between the tag and the attribute.
    let after_ws = skip_ws(body, i);
    if after_ws == i {
        return None;
    }
    i = after_ws;
    if !body[i..].starts_with("name") {
        return None;
    }
    i = skip_ws(body, i + "name".len());
    if !body[i..].starts_with('=') {
        return None;
    }
    i = skip_ws(body, i + 1);
    if !body[i..].starts_with('"') {
        return None;
    }
    i += 1;
    let key_end = i + body[i..].find('"')?;
    let key = &body[i..key_end];
    i = skip_ws(body, key_end + 1);
    if !body[i..].starts_with('>') {
        return None;
    }
    i += 1;
    let close = i + body[i..].find("</parameter>")?;
    Some((key, &body[i..close], close + "</parameter>".len()))
}

/// Advance past whitespace. Shared with the channel grammar's reader
/// ([`super::channel::skip_ws`]) so `\s` means the same thing in both.
fn skip_ws(s: &str, i: usize) -> usize {
    super::channel::skip_ws(s, i)
}

/// Did the model MEAN an `<invoke` opener as a call? The attribute or a
/// `<parameter` child is the shape (python `tool_parse.py:571-572`). Receives the
/// already-trimmed body, which starts immediately after `<invoke`.
pub fn looks_like_an_attempt(body: &str) -> bool {
    body.starts_with("name=") || body.contains("<parameter")
}
