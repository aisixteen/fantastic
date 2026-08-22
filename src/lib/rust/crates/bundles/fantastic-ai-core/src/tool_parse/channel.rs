//! A THIRD text envelope, from the SPECIAL-TOKEN family. A `|` inside the
//! brackets marks a token that lives in the tokenizer's VOCABULARY rather than in
//! ordinary text, so the delimiter cannot be forged by the content it wraps — the
//! same idea as ChatML's `<|im_start|>`, Llama-3's `<|eot_id|>` / `<|python_tag|>`,
//! harmony's `<|channel|>`, and DeepSeek's tool-call markers. What distinguishes
//! THIS grammar is that the pipe MIRRORS across the pair — `<|tool_call>` opens,
//! `<tool_call|>` closes — and that strings carry a quoting token of their own,
//! `<|"|>`, instead of `"`:
//!
//! ```text
//! <|tool_call>call:get_current_weather{location:<|"|>Tokyo, JP<|"|>}<tool_call|>
//! ```
//!
//! So the argument body is NOT JSON. Keys are bare, strings wear `<|"|>`, and
//! numbers, booleans, lists and nested objects are written bare. A JSON parse over
//! that body succeeds ONLY for the empty `{}` — every call carrying an actual
//! argument parses as malformed and is silently dropped, which is the worst
//! possible failure here.
//!
//! Measured 2026-08-04 on gemma-4-12B-it: it planned correctly in its thought
//! channel, chose the right agent AND verb, emitted
//! `<|tool_call>call:kernel_state.list_agents{}<tool_call|>`, and the run died at
//! pass 1 with zero calls extracted. `<tool_call>` is not a substring of either
//! tag — the pipe breaks the match — so the taught envelope's finder never saw it
//! at all.
//!
//! Two body shapes reach us, because a model compresses our ONE universal tool:
//!
//! ```text
//! call:send{target_id:<|"|>x<|"|>,payload:{…}}   — our taught tool, in this syntax
//! call:<target>.<verb>{…}                        — the same thing, compressed
//! ```
//!
//! The second maps onto `send(target_id=<target>, payload={"type": <verb>, **args})`.
//! `reply_to` has no slot and needs none: it defaults to SELF, so the reply
//! arrives as the next turn, which is what a loop wants.
//!
//! Still raw prompt-and-parse — plain text the model typed. No `tools=[]`, no
//! `tool_choice`, no native tool_use block; the provider is untouched.
//!
//! Ported from python `tool_parse.py:84-356`.

use super::shape::{inline_send, ToolCall};
use serde_json::{Map, Number, Value};

/// Opening token of the special-token channel envelope.
pub const CHANNEL_OPEN: &str = "<|tool_call>";
/// Closing token of the special-token channel envelope — the pipe MIRRORS.
pub const CHANNEL_CLOSE: &str = "<tool_call|>";
/// The grammar's own quoting token, in place of `"`.
pub const CHANNEL_STR: &str = r#"<|"|>"#;

/// Parse one `call:<name>{…}` body into the universal `send` call.
pub fn parse_channel_call(block: &str) -> Option<ToolCall> {
    let (name, args_at) = channel_head(block)?;
    // Malformed args are NOT repaired — a parser that guesses what the model meant
    // is measuring its own guess. Drop the call; the model gets no reply and can
    // restate it. (`malformed_calls` is what gives that drop a voice.)
    //
    // The end offset is DISCARDED, as python discards it here: it served only as the
    // malformed sentinel, which `?` now carries. Text after the closing brace is
    // ignored rather than refused — the closing token already bounded the block.
    let (args, _) = parse_channel_object(block, args_at)?;
    // THE ARGS DECIDE, not the name. A model that already wrote `target_id` has
    // written canonical `send` arguments, and any dotted prefix on the name is a
    // NAMESPACE it added, not a target. Measured 2026-08-04: gemma-4-12B-it emitted
    //   call:kernel_state.send{"payload":{"type":"reflect"},"target_id":"kernel_state"}
    // Reading the prefix as the target turned that into
    // `send(kernel_state, {"type":"send", …})` and came back
    // `{"error": "kernel_state: unknown type 'send'"}` eight times in one run.
    if let Some(target) = args.get("target_id").cloned() {
        if args.contains_key("payload") {
            return Some(ToolCall::new("send", args));
        }
        // target_id, no payload, but a `type` sitting among the args: the payload is
        // INLINE — the model flattened one level instead of nesting. Measured
        // 2026-08-06, gemma-4-12B emitted
        //   call:kernel_state{type: 'reflect', target_id: 'readings'}
        // meaning "reflect readings". Before this branch the name was not dotted, so
        // the verb was looked for only in the name, found nowhere, and a complete
        // instruction was discarded. `type` beats a dotted verb here for the reason
        // the block below already gives: THE ARGS DECIDE.
        if args.contains_key("type") {
            return inline_send(&target, &args);
        }
        // target_id but NO payload: the VERB is still in the name and must not be
        // dropped. Measured 2026-08-04 — `call:kernel_state.reflect{"target_id":
        // "transcripts"}` means "reflect transcripts". An earlier revision of this
        // branch returned a send with no payload at all; the router dispatched a
        // null payload and python's handler raised `'NoneType' object has no
        // attribute 'get'`, three times in one run.
        if let Some((_, verb)) = name.split_once('.') {
            let mut payload = Map::new();
            payload.insert("type".to_string(), Value::String(verb.to_string()));
            for (k, v) in &args {
                if k != "target_id" {
                    payload.insert(k.clone(), v.clone());
                }
            }
            let mut out = Map::new();
            out.insert("target_id".to_string(), target);
            out.insert("payload".to_string(), Value::Object(payload));
            return Some(ToolCall::new("send", out));
        }
        // No payload and no verb anywhere — there is no call here to make.
        return None;
    }
    if let Some((target, verb)) = name.split_once('.') {
        let mut payload = Map::new();
        payload.insert("type".to_string(), Value::String(verb.to_string()));
        // The args come SECOND, so an explicit `type` among them overrides the
        // dotted verb: the args decide here too.
        for (k, v) in &args {
            payload.insert(k.clone(), v.clone());
        }
        let mut out = Map::new();
        out.insert("target_id".to_string(), Value::String(target.to_string()));
        out.insert("payload".to_string(), Value::Object(payload));
        return Some(ToolCall::new("send", out));
    }
    if args.is_empty() {
        return None;
    }
    // No target_id and an undotted name, but a `type` among the args: then the NAME
    // is the agent and the args are its payload — `call:kernel_state{type:'reflect'}`
    // is `send('kernel_state', {'type':'reflect'})`. Without this the call kept the
    // agent's name in `name` where nothing reads it, arrived with no `target_id`, and
    // earned a `no target_id` error for a request that was perfectly clear.
    if args.contains_key("type") {
        return inline_send(&Value::String(name.to_string()), &args);
    }
    Some(ToolCall::new(name, args))
}

// ─── the VALUE grammar: how a not-JSON body is read ─────────────
//
// Python returns `(value, next_index)` with `next_index == -1` as the malformed
// sentinel and a separate `_BAD` object because `None` is a legal VALUE and cannot
// double as the failure signal. `Option<(T, usize)>` says the same thing in one
// type, so there is no sentinel to leak into a caller that forgot to check it.

/// The escapes a quoted channel string may carry. Shared by both quote styles so
/// `'a\nb'` and `"a\nb"` decode identically — a model should not get a different
/// string back for picking a different quote character.
fn escape(nxt: char) -> Option<char> {
    match nxt {
        '"' => Some('"'),
        '\'' => Some('\''),
        '\\' => Some('\\'),
        '/' => Some('/'),
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        'b' => Some('\u{8}'),
        'f' => Some('\u{c}'),
        _ => None,
    }
}

/// `(string, index just past the closing quote)` for a `'…'` or `"…"` value.
/// `None` is malformed.
///
/// BOTH quote styles, because which one a model reaches for is a property of its
/// training, not of our grammar — the documented gemma grammar uses `<|"|>`, the
/// 2026-08-04 run mixed in JSON double quotes, and the 2026-08-06 run emitted
/// `call:kernel_state{type: 'send', target_id: 'kernel', …}` in single quotes. That
/// last one was a fully correct intent and the parser threw it away, which ended the
/// run: the model had nothing left to fix.
///
/// ONLY the matching quote closes. So an opener of one style closed by the other
/// never terminates, runs to the end of the body, and is reported malformed — which
/// is right, because a mismatched pair is a real error and not a shape any model
/// actually means.
fn parse_quoted(s: &str, at: usize) -> Option<(String, usize)> {
    let quote = s[at..].chars().next()?;
    let mut out = String::new();
    let mut i = at + quote.len_utf8();
    while i < s.len() {
        let mut chars = s[i..].chars();
        let c = chars.next()?;
        // A LONE trailing backslash is not an escape (python guards with
        // `i + 1 < len(s)`): it falls through to the literal branch, is kept, and the
        // string then runs out unterminated.
        if c == '\\' {
            if let Some(nxt) = chars.next() {
                if nxt == 'u' {
                    // `\uXXXX`. `get` yields None when the four bytes are absent or
                    // land off a character boundary, and non-hex fails to parse —
                    // both fall through to the escape table below, exactly as
                    // python's `except ValueError: pass` does.
                    //
                    // `char::from_u32` also rejects a lone SURROGATE, which python's
                    // `chr` accepts and rust's `char` cannot represent at all. Same
                    // fall-through: `\u` survives literally rather than being
                    // replaced by a guess.
                    if let Some(cp) = s
                        .get(i + 2..i + 6)
                        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                        .and_then(char::from_u32)
                    {
                        out.push(cp);
                        i += 6;
                        continue;
                    }
                }
                // An unknown escape keeps both characters rather than guessing: a
                // Windows path in a program body (`\d`) must survive verbatim.
                match escape(nxt) {
                    Some(decoded) => out.push(decoded),
                    None => {
                        out.push('\\');
                        out.push(nxt);
                    }
                }
                i += '\\'.len_utf8() + nxt.len_utf8();
                continue;
            }
        }
        if c == quote {
            return Some((out, i + c.len_utf8()));
        }
        out.push(c);
        i += c.len_utf8();
    }
    None
}

/// `(value, index just past it)`. `None` is malformed.
fn parse_channel_value(s: &str, at: usize) -> Option<(Value, usize)> {
    let i = skip_ws(s, at);
    if i >= s.len() {
        return None;
    }
    if s[i..].starts_with(CHANNEL_STR) {
        // The grammar's own string token. Its body is taken VERBATIM — the token
        // cannot be forged by the content it wraps, so there is nothing to escape.
        let from = i + CHANNEL_STR.len();
        let close = from + s[from..].find(CHANNEL_STR)?;
        return Some((
            Value::String(s[from..close].to_string()),
            close + CHANNEL_STR.len(),
        ));
    }
    let head = s[i..].chars().next()?;
    if head == '"' || head == '\'' {
        // MIXED SYNTAX — the envelope is the channel one, the strings are ordinary
        // quoted text. Measured 2026-08-04: gemma-4-12B-it emitted
        //   call:kernel_state.update_agent{"id":"assets","ingress_rule":"allow_all_ingress"}
        // Without this branch the bare-token path returns `"assets"` WITH its quote
        // characters, `id` never arrives, and the call comes back
        // `{"error": "update_agent: id required"}` — which is what killed all three
        // posture changes in that run.
        let (text, end) = parse_quoted(s, i)?;
        return Some((Value::String(text), end));
    }
    if head == '{' {
        let (obj, end) = parse_channel_object(s, i)?;
        return Some((Value::Object(obj), end));
    }
    if head == '[' {
        let mut out: Vec<Value> = Vec::new();
        let mut i = skip_ws(s, i + 1);
        if s[i..].starts_with(']') {
            return Some((Value::Array(out), i + 1));
        }
        loop {
            let (v, next) = parse_channel_value(s, i)?;
            out.push(v);
            i = skip_ws(s, next);
            if s[i..].starts_with(',') {
                i += 1;
                continue;
            }
            if s[i..].starts_with(']') {
                return Some((Value::Array(out), i + 1));
            }
            return None;
        }
    }
    // A BARE token, ending at the first structural character. Byte-wise scanning is
    // safe and exact here: `,`, `}` and `]` are ASCII, and a UTF-8 continuation byte
    // can never equal one, so the stop always lands on a character boundary.
    let mut j = i;
    while j < s.len() && !matches!(s.as_bytes()[j], b',' | b'}' | b']') {
        j += 1;
    }
    let raw = s[i..j].trim();
    if raw.is_empty() {
        return None;
    }
    let low = raw.to_lowercase();
    if low == "true" {
        return Some((Value::Bool(true), j));
    }
    if low == "false" {
        return Some((Value::Bool(false), j));
    }
    if low == "null" || low == "none" {
        return Some((Value::Null, j));
    }
    if let Ok(n) = raw.parse::<i64>() {
        return Some((Value::Number(Number::from(n)), j));
    }
    // Then a float, as python tries `int` before `float`. `is_finite` is the one
    // divergence worth naming: python's `float("nan")`/`float("inf")` succeed and
    // JSON has no such number, so those tokens stay STRINGS here rather than
    // becoming a value no consumer could serialize. An integer past `i64` lands as a
    // float for the same reason — that is the widest number JSON carries.
    if let Some(n) = raw
        .parse::<f64>()
        .ok()
        .filter(|f| f.is_finite())
        .and_then(Number::from_f64)
    {
        return Some((Value::Number(n), j));
    }
    Some((Value::String(raw.to_string()), j))
}

/// `(object, index just past the closing brace)` for a `{k:v,k:v}` body. `None` is
/// malformed.
fn parse_channel_object(s: &str, at: usize) -> Option<(Map<String, Value>, usize)> {
    let mut i = skip_ws(s, at);
    if !s[i..].starts_with('{') {
        return None;
    }
    i = skip_ws(s, i + 1);
    let mut out = Map::new();
    if s[i..].starts_with('}') {
        return Some((out, i + 1));
    }
    loop {
        i = skip_ws(s, i);
        // Keys are bare in the documented grammar, but a model mixing JSON quotes
        // them — in EITHER style. Parsed with the same reader as a value so a key is
        // allowed to contain `:` or `,`; scanning bare to the first structural
        // character would split such a key in half and lose the rest.
        let (key, after_key) = match s[i..].chars().next() {
            Some('"') | Some('\'') => {
                let (key, end) = parse_quoted(s, i)?;
                (key, skip_ws(s, end))
            }
            _ => {
                let mut k = i;
                while k < s.len() && !matches!(s.as_bytes()[k], b':' | b',' | b'}') {
                    k += 1;
                }
                (s[i..k].trim().to_string(), k)
            }
        };
        if key.is_empty() || !s[after_key..].starts_with(':') {
            return None;
        }
        let (value, next) = parse_channel_value(s, after_key + 1)?;
        out.insert(key, value);
        i = skip_ws(s, next);
        if s[i..].starts_with(',') {
            i += 1;
            continue;
        }
        if s[i..].starts_with('}') {
            return Some((out, i + 1));
        }
        return None;
    }
}

/// Did the model MEAN a `<|tool_call>` opener as a call? The `call:<name>` head is
/// the shape (python `tool_parse.py:569-570`). Receives the already-trimmed body.
pub fn looks_like_an_attempt(body: &str) -> bool {
    channel_head(body).is_some()
}

/// The channel head — `(name, index just past it)`, or `None` when the body does
/// not open with one.
///
/// Hand-written scanner: there is no `regex` dependency in this workspace, and
/// python's parser is hand-written too. Mirrors
/// `_CHANNEL_HEAD = re.compile(r"\s*call\s*:\s*([^\s{]+)\s*")` applied with
/// `.match` (anchored at the START of the body); the returned index is the
/// regex's `m.end()`, i.e. past the trailing whitespace, which is where the
/// argument object begins.
pub fn channel_head(block: &str) -> Option<(&str, usize)> {
    let mut i = skip_ws(block, 0);
    if !block[i..].starts_with("call") {
        return None;
    }
    i = skip_ws(block, i + "call".len());
    if !block[i..].starts_with(':') {
        return None;
    }
    i = skip_ws(block, i + 1);
    let start = i;
    for ch in block[i..].chars() {
        if ch.is_whitespace() || ch == '{' {
            break;
        }
        i += ch.len_utf8();
    }
    // `[^\s{]+` needs at least one character: a bare `call:` names nothing.
    if i == start {
        return None;
    }
    Some((&block[start..i], skip_ws(block, i)))
}

/// Advance past whitespace (python `_skip_ws`, `tool_parse.py:122-125`).
pub fn skip_ws(s: &str, mut i: usize) -> usize {
    for ch in s[i..].chars() {
        if !ch.is_whitespace() {
            break;
        }
        i += ch.len_utf8();
    }
    i
}
