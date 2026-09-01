//! The TAUGHT envelope (Hermes-style): `<tool_call>{…}</tool_call>` with a JSON
//! body. Ported from python `tool_parse.py:382-447`.

use super::shape::{inline_send, ToolCall};
use serde_json::{Map, Value};

/// Opening tag of the taught tool-call envelope.
pub const OPEN: &str = "<tool_call>";
/// Closing tag of the taught tool-call envelope.
pub const CLOSE: &str = "</tool_call>";

/// The position immediately AFTER a `"bytes"\s*:\s*"` opener, if `inner` holds
/// one — the byte offset of the RAW program's first char. Hand-written because
/// there is no `regex` dependency in this workspace (see `invoke_envelope`).
/// Whitespace-tolerant around the `:` exactly as python's `_SB_BYTES` regex is.
fn bytes_value_start(inner: &str) -> Option<usize> {
    let bytes = inner.as_bytes();
    let mut from = 0;
    while let Some(rel) = inner[from..].find("\"bytes\"") {
        let mut i = from + rel + "\"bytes\"".len();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b':' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'"' {
                return Some(i + 1); // first char of the raw program
            }
        }
        from += rel + "\"bytes\"".len();
    }
    None
}

/// `send_binary` whose `bytes` field is a program that must NOT be JSON-escaped.
/// Ported from python `tool_parse.py::_send_binary_raw`.
///
/// The whole reason a program-in-JSON fails is that a strict parse demands every
/// quote, newline and backslash be escaped, which a model cannot do reliably on a
/// big program. But the PROGRAM is not data we need parsed — it is a string we
/// read. So for `send_binary` we lift `bytes` out RAW: everything from `"bytes":"`
/// to the FINAL `"` in the envelope is the program, verbatim (no unescaping), and
/// only the STRUCTURE around it is JSON-parsed. `bytes` must be the LAST field
/// (nothing but closing braces after its value) so its end is unambiguous — that
/// is the whole contract.
fn send_binary_raw(inner: &str) -> Option<ToolCall> {
    if !inner.contains("\"send_binary\"") {
        return None;
    }
    let val_start = bytes_value_start(inner)?;
    let val_end = inner.rfind('"')?; // bytes is last → its closing quote is the last quote
    if val_end <= val_start {
        return None;
    }
    let raw = &inner[val_start..val_end]; // the program, exactly as written
                                          // Re-close the JSON with an EMPTY bytes value → a well-formed object we can
                                          // parse for the STRUCTURE (name, target_id, payload metadata) without touching
                                          // `raw`.
    let skeleton = format!("{}{}", &inner[..val_start], &inner[val_end..]);
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&skeleton) else {
        return None;
    };
    if obj.get("name").and_then(Value::as_str) != Some("send_binary") {
        return None;
    }
    let Some(Value::Object(mut args)) = obj.get("arguments").cloned() else {
        return None;
    };
    // Reinsert the RAW program string. Prefer the payload sub-object (where a
    // `write_stream` carries it); fall back to the args themselves.
    match args.get_mut("payload") {
        Some(Value::Object(payload)) => {
            payload.insert("bytes".to_string(), Value::String(raw.to_string()));
        }
        _ => {
            args.insert("bytes".to_string(), Value::String(raw.to_string()));
        }
    }
    Some(ToolCall::new("send_binary", args))
}

// ─── the FENCED form: the program rides OUTSIDE the JSON ────────────────────
//
// Ported from python `tool_parse.py` (fence machinery). Measured 2026-09-01
// (exercism gym, rust/word-count): the model followed the raw-tail contract
// exactly — real newlines, real apostrophes — and still lost the call to ONE
// missing closing brace, because the raw tail keeps the payload inside JSON
// syntax. The general fix removes closing syntax from the payload path: small
// flat metadata JSON with "@body" where the program goes, and the program
// verbatim between two column-0 marker lines. A marker at column 0 is never
// valid code at that position in any language — nothing to escape or count.

/// Column-0 opening marker of the fenced form.
pub const FENCE_OPEN: &str = "*** Begin Bytes";
/// Column-0 closing marker of the fenced form.
pub const FENCE_CLOSE: &str = "*** End Bytes";

/// If `marker` at byte `at` sits alone on its line (column 0, only whitespace
/// after), the byte index just past that line's newline (or `inner.len()`).
fn marker_line_end(inner: &str, at: usize, marker: &str) -> Option<usize> {
    if at != 0 && inner.as_bytes()[at - 1] != b'\n' {
        return None;
    }
    let rest = &inner[at + marker.len()..];
    let (line, next) = match rest.find('\n') {
        Some(n) => (&rest[..n], at + marker.len() + n + 1),
        None => (rest, inner.len()),
    };
    if line
        .trim_matches(|c: char| c == '\r' || c == ' ' || c == '\t')
        .is_empty()
    {
        Some(next)
    } else {
        None
    }
}

/// First line-anchored `FENCE_OPEN`: `(marker_start, body_start)`.
fn fence_open(inner: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(rel) = inner[from..].find(FENCE_OPEN) {
        let at = from + rel;
        if let Some(body_start) = marker_line_end(inner, at, FENCE_OPEN) {
            return Some((at, body_start));
        }
        from = at + FENCE_OPEN.len();
    }
    None
}

/// Start of the LAST line-anchored `FENCE_CLOSE` at/after `from` — last, so a
/// program that somehow contains the closer is still taken whole. `None` means
/// no closer: the caller takes the rest of the block (the envelope's own
/// closing tag already bounds it; dropping an unterminated program loses an
/// unambiguous instruction — same stance as the scanner's unterminated
/// envelope tolerance).
fn fence_close_last(inner: &str, from: usize) -> Option<usize> {
    let mut best = None;
    let mut cur = from;
    while let Some(rel) = inner[cur..].find(FENCE_CLOSE) {
        let at = cur + rel;
        if marker_line_end(inner, at, FENCE_CLOSE).is_some() {
            best = Some(at);
        }
        cur = at + FENCE_CLOSE.len();
    }
    best
}

/// Replace the first `"@body"` placeholder anywhere in the value with the raw
/// program. Returns whether one was found.
fn plant_body(node: &mut Value, raw: &str) -> bool {
    match node {
        Value::Object(map) => {
            for (_k, v) in map.iter_mut() {
                if v.as_str() == Some("@body") {
                    *v = Value::String(raw.to_string());
                    return true;
                }
                if plant_body(v, raw) {
                    return true;
                }
            }
            false
        }
        Value::Array(items) => items.iter_mut().any(|v| plant_body(v, raw)),
        _ => false,
    }
}

/// Parse the JSON between one tag pair into a [`ToolCall`].
///
/// Lenient (tiny models drift): accepts `{"name","arguments"}`, a `tool` alias
/// for `name`, a flattened object (remaining keys become arguments), and
/// double-encoded (stringified) arguments. `None` on unparseable JSON.
pub fn parse_one(inner: &str) -> Option<ToolCall> {
    // FENCED form first (on the UNTRIMMED block — a trim would eat the
    // program's trailing newline when the close marker was omitted): the JSON
    // part is ONLY what sits above the markers. Parsing the whole block would
    // either choke on the raw program or silently keep the "@body" placeholder
    // as the written contents.
    if let Some((open_at, body_start)) = fence_open(inner) {
        let raw = match fence_close_last(inner, body_start) {
            Some(close_at) => &inner[body_start..close_at],
            None => &inner[body_start..],
        };
        let mut call = parse_one(&inner[..open_at])?;
        let mut args = Value::Object(std::mem::take(&mut call.arguments));
        if !plant_body(&mut args, raw) {
            // No placeholder written — the program still has exactly one home.
            if let Value::Object(map) = &mut args {
                match map.get_mut("payload") {
                    Some(Value::Object(p)) => {
                        p.insert("bytes".to_string(), Value::String(raw.to_string()));
                    }
                    _ => {
                        map.insert("bytes".to_string(), Value::String(raw.to_string()));
                    }
                }
            }
        }
        if let Value::Object(map) = args {
            call.arguments = map;
        }
        return Some(call);
    }
    let inner = inner.trim();
    // send_binary carrying a RAW program in `bytes` (real newlines, real
    // quotes) is not valid JSON, so the raw-tail lift applies ONLY when the
    // strict parse fails. A model that escaped its whole call correctly parses
    // strictly and gets its escapes DECODED — measured 2026-08-31 (python E44):
    // the raw path ran first, kept `\n` literal, and a valid patch died on
    // "no blocks" twice with no way for the model to see why.
    if serde_json::from_str::<Value>(inner).is_err() {
        if let Some(sb) = send_binary_raw(inner) {
            return Some(sb);
        }
    }
    let (obj, rest) = parse_prefix(inner)?;
    let Value::Object(obj) = obj else {
        return None;
    };
    let name = truthy_string(obj.get("name"))
        .or_else(|| truthy_string(obj.get("tool")))
        .unwrap_or_else(|| "send".to_string());
    let args = match obj.get("arguments") {
        // Absent (or JSON `null`, which python's `.get` cannot tell apart):
        // FLATTENED — the remaining keys are the arguments.
        None | Some(Value::Null) => strays(&obj),
        Some(Value::Object(inner_args)) => {
            // Top-level keys that belong INSIDE arguments. Models routinely put
            // `reply_to` one level too high; `arguments` wins on conflict.
            let mut merged = strays(&obj);
            if !rest.is_empty() {
                // `,"reply_to":"self"}}` → recover the k/v pairs the stray tail held.
                if let Some(Value::Object(tail)) = recover_tail(rest) {
                    for (k, v) in tail {
                        merged.insert(k, v);
                    }
                }
            }
            for (k, v) in inner_args {
                merged.insert(k.clone(), v.clone());
            }
            merged
        }
        // Double-encoded: the arguments object arrived as a JSON string. Anything
        // that is not an object once decoded is no arguments at all.
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        },
        Some(_) => Map::new(),
    };
    // THE VERB IN THE TOOL NAME. `send` is the only tool there is, so a model
    // that writes any other name has compressed verb and tool into one — the
    // same compression the `<|tool_call>` channel grammar handles for
    // `call:<agent>.<verb>`, arriving here in the JSON envelope instead.
    // Measured 2026-08-06, gemma-4-12B:
    //   {"name":"reflect","arguments":{"target_id":"clock"}}
    // meaning "reflect the clock". Untranslated, python's `_route_reply` read
    // `payload` as absent and dispatched `send("clock", None)`, so the handler
    // was called with None and raised `'NoneType' object has no attribute 'get'`
    // — three times in the run this was found in. An explicit `type` in the args
    // still wins (it is inserted after the name below), since the args decide.
    if name != "send" && !args.contains_key("payload") {
        if let Some(target) = args.get("target_id") {
            let mut with_type = Map::new();
            with_type.insert("type".to_string(), Value::String(name.clone()));
            for (k, v) in &args {
                with_type.insert(k.clone(), v.clone());
            }
            if let Some(inline) = inline_send(target, &with_type) {
                return Some(inline);
            }
        }
    }
    Some(ToolCall::new(name, args))
}

/// Did the model MEAN a `<tool_call>` opener as a call? The JSON body's shape
/// decides: a call is an object (python `tool_parse.py:573`). Receives the
/// already-trimmed body.
pub fn looks_like_an_attempt(body: &str) -> bool {
    body.starts_with('{')
}

/// The top-level keys that are NOT the call's own frame — candidates for
/// `arguments`, and the whole of it when `arguments` is absent.
fn strays(obj: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in obj {
        if !matches!(k.as_str(), "name" | "tool" | "arguments") {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// One JSON value from the START of `s`, plus whatever text followed it — the
/// streaming-offset equivalent of python's `json.JSONDecoder().raw_decode`.
///
/// Trailing junk after a COMPLETE object is observed live from gpt-oss:120b,
/// which closes `arguments` early and then emits `,"reply_to":"self"}}` (one
/// brace too many). Take the object that IS there; the remainder is recovered by
/// [`recover_tail`] rather than thrown away, because the model's real intent is
/// usually in it. A clean body leaves an empty remainder, so this is ONE path,
/// not a retry after a strict parse.
fn parse_prefix(s: &str) -> Option<(Value, &str)> {
    let mut stream = serde_json::Deserializer::from_str(s).into_iter::<Value>();
    let value = stream.next()?.ok()?;
    // `byte_offset` is the position immediately AFTER the value just read —
    // exactly what `raw_decode` returns as its `end`.
    Some((value, &s[stream.byte_offset()..]))
}

/// `,"reply_to":"self"}}` → `{"reply_to":"self"}`: re-read the k/v pairs a stray
/// tail held. `None` when the tail is not k/v pairs, in which case it is simply
/// dropped — we do not guess at what it might have meant.
fn recover_tail(rest: &str) -> Option<Value> {
    let body = rest.trim().trim_start_matches(',').trim_end_matches('}');
    serde_json::from_str(&format!("{{{body}}}")).ok()
}

/// Python's `obj.get("name") or obj.get("tool") or "send"` chain: an absent,
/// empty or otherwise falsy candidate falls THROUGH to the next one. A
/// non-string is stringified rather than refused — a model that wrote a number
/// in the name slot still named something (python's `str(name)`).
fn truthy_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null | Value::Bool(false) => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(true) => Some("true".to_string()),
        Value::Number(n) if n.as_f64() == Some(0.0) => None,
        Value::Number(n) => Some(n.to_string()),
        Value::Array(a) if a.is_empty() => None,
        Value::Object(o) if o.is_empty() => None,
        other => Some(other.to_string()),
    }
}
