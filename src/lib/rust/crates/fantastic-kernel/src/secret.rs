//! SECRET VALUES on a call — the caller marks them, the kernel never records them.
//!
//! Port of python `kernel/_secret.py` (canonical, owner 2026-10-01).
//!
//! A batch item may carry `secret: [path, …]` beside `target`/`payload`/`timeout`
//! ([`crate::outcome::Item::with_secret`]). A path is dotted and rooted at the
//! item: `payload.<field…>` names a field of the payload, `result` the whole
//! reply, `result.<field…>` a field of it. The TARGET is handed the real payload
//! and the CALLER the real reply; every place the kernel RECORDS or SHOWS the call
//! carries `<secret>` instead — the call and settle rows (and so the memory
//! store), the `ledger` read-back and dedup, the state stream's `payload` and
//! `summary`, the inbox and watcher mirrors, and any error text the call's
//! outcome carries.
//!
//! Two limits, by design:
//!   * masking is not isolation — the receiver holds the value; send secrets to a
//!     bundle that keeps them, never to an AI agent (they would enter its prompt
//!     and its memory);
//!   * a call whose record holds placeholders cannot be answered FROM that
//!     record: re-sending a named execution with secrets after it left RAM is
//!     refused, loud.
//!
//! Not secret-able: `payload.type` — the verb is how every row is read.
//!
//! THE RAW BODY. Python's payload/reply dict holds bytes natively; a rust `Value`
//! cannot, so the body rides beside it ([`crate::outcome::Item::binary`], at the
//! payload path its [`crate::outcome::Blob`] names; [`Outcome::body`], at the
//! reply's `bytes` key). Here the body IS the value at that path —
//! `payload.<its path>` / `result.bytes` name it, and marking it hides it from
//! every record and mirror (python `_put` writes the placeholder over the bytes).
//! Its scrub value is what python's `str(bytes)` gives: the bytes' repr.

use crate::outcome::{Blob, Item, Outcome, Step};
use fantastic_bundle::{py_repr, py_repr_value, py_str};
use serde_json::{json, Value};

#[cfg(test)]
#[path = "secret/tests.rs"]
mod tests;

/// What every record and mirror carries where a secret value was.
pub const PLACEHOLDER: &str = "<secret>";

/// What one item asked to be kept: the payload/result paths (as key lists), the
/// paths as given (recorded on the call row — paths are not secret), and the
/// payload's secret VALUES (RAM only — scrubbed out of every recorded string).
#[derive(Debug, Clone)]
pub struct Secret {
    /// The paths exactly as the item gave them.
    pub paths: Vec<String>,
    payload_paths: Vec<Vec<String>>,
    result_paths: Vec<Vec<String>>,
    values: Vec<String>,
}

/// The payload as recorded and shown.
#[derive(Debug, Clone)]
pub struct Shown {
    /// The payload with every secret field → [`PLACEHOLDER`] and every secret
    /// value scrubbed out of its strings.
    pub payload: Value,
    /// The item's raw request body was itself marked (`payload.<its path>`): no
    /// record or mirror carries it — its slot holds the placeholder instead.
    pub body_hidden: bool,
}

/// Where a path landed: a JSON value, or the raw body beside it.
enum Found<'a> {
    Json(&'a Value),
    Body(&'a [u8]),
}

fn step<'a>(cur: &'a Value, key: &str) -> Option<&'a Value> {
    match cur {
        Value::Object(m) => m.get(key),
        // python `key.isascii() and key.isdigit()` then `int(key)`: a digit run
        // too long for a usize is past any list's end either way.
        Value::Array(a) if !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()) => {
            key.parse::<usize>().ok().and_then(|i| a.get(i))
        }
        _ => None,
    }
}

fn get<'a>(obj: &'a Value, keys: &[String]) -> Option<&'a Value> {
    let mut cur = obj;
    for k in keys {
        cur = step(cur, k)?;
    }
    Some(cur)
}

/// A raw body beside a JSON object: the steps to the slot it sits at
/// ([`crate::outcome::Blob::steps`]), and its bytes.
type BodyAt<'a> = Option<(&'a [Step], &'a [u8])>;

/// Whether the mark's key `k` names the body's step `p` — the same step `step`
/// takes: an object key verbatim, a list index by its digits.
fn names_step(k: &str, p: &Step) -> bool {
    match p {
        Step::Key(key) => k == key,
        Step::Index(i) => {
            !k.is_empty()
                && k.bytes().all(|b| b.is_ascii_digit())
                && k.parse::<usize>().ok() == Some(*i)
        }
    }
}

/// `get` over the value AND its body: the body is the value at its own path of
/// an object, and nothing lies below it (python `_step` into bytes fails).
fn find<'a>(root: &'a Value, body: BodyAt<'a>, keys: &[String]) -> Option<Found<'a>> {
    if let (Some((at, b)), Value::Object(_)) = (body, root) {
        if keys.len() >= at.len() && keys.iter().zip(at).all(|(k, p)| names_step(k, p)) {
            return (keys.len() == at.len()).then_some(Found::Body(b));
        }
    }
    get(root, keys).map(Found::Json)
}

/// Every string a secret value holds — the value itself, or its leaves.
fn strings(x: &Value, out: &mut Vec<String>) {
    match x {
        Value::String(s) => {
            if !s.is_empty() {
                out.push(s.clone());
            }
        }
        Value::Object(m) => m.values().for_each(|v| strings(v, out)),
        Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
        Value::Null | Value::Bool(_) => {}
        Value::Number(_) => out.push(py_str(x)),
    }
}

/// [`strings`] of a whole value whose raw body rides beside it at its slot — in
/// that slot's place (appended last when the value has no key there yet, where
/// [`crate::jsonsafe::with_body`] puts it). A body with no slot in the value is
/// not part of it.
fn strings_with_body(x: &Value, body: Option<&Blob>, out: &mut Vec<String>) {
    let Some(b) = body else {
        strings(x, out);
        return;
    };
    let mut placed = x.clone();
    if b.place(&mut placed, Value::Null).is_err() {
        strings(x, out);
        return;
    }
    strings_along(&placed, &b.path, &b.bytes, out);
}

/// [`strings`] of `x`, with the body's bytes standing at the steps `at`.
fn strings_along(x: &Value, at: &[Step], bytes: &[u8], out: &mut Vec<String>) {
    let Some((first, rest)) = at.split_first() else {
        out.push(py_bytes_repr(bytes));
        return;
    };
    match (x, first) {
        (Value::Object(m), Step::Key(key)) => {
            for (k, v) in m {
                if k == key {
                    strings_along(v, rest, bytes, out);
                } else {
                    strings(v, out);
                }
            }
        }
        (Value::Array(a), Step::Index(i)) => {
            for (j, v) in a.iter().enumerate() {
                if j == *i {
                    strings_along(v, rest, bytes, out);
                } else {
                    strings(v, out);
                }
            }
        }
        _ => strings(x, out),
    }
}

/// python `str(b)` / `repr(b)` for a bytes value.
fn py_bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };
    let mut out = String::with_capacity(b.len() + 3);
    out.push('b');
    out.push(char::from(quote));
    for &c in b {
        match c {
            _ if c == quote || c == b'\\' => {
                out.push('\\');
                out.push(char::from(c));
            }
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            _ if !(0x20..0x7f).contains(&c) => out.push_str(&format!("\\x{c:02x}")),
            _ => out.push(char::from(c)),
        }
    }
    out.push(char::from(quote));
    out
}

fn bad(why: String) -> Value {
    let mut r = crate::refusal::refusal(crate::refusal::BAD_ARG, format!("send: secret {why}"));
    crate::refusal::with(&mut r, "arg", json!("secret"));
    r
}

/// Validate an item's `secret`: `Ok(None)` when absent (or null) — nothing
/// secret; `Err(refusal)` for a malformed mark. A path the payload does not have
/// is refused — a typo would otherwise record the value it meant to hide.
///
/// # Errors
/// The `bad_arg` refusal (`arg: "secret"`) naming what is wrong with the mark.
pub fn check(item: &Item) -> Result<Option<Secret>, Value> {
    let Some(raw) = item.secret.as_ref().filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let Some(list) = raw.as_array().filter(|a| !a.is_empty()) else {
        return Err(bad(format!(
            "must be a non-empty list of paths, got {}",
            py_repr_value(raw)
        )));
    };
    let steps = item.binary.as_ref().map(crate::outcome::Blob::steps);
    let body = item
        .binary
        .as_ref()
        .zip(steps)
        .map(|(b, at)| (at, b.bytes.as_slice()));
    let mut paths: Vec<String> = Vec::with_capacity(list.len());
    let mut payload_paths: Vec<Vec<String>> = Vec::new();
    let mut result_paths: Vec<Vec<String>> = Vec::new();
    for p in list {
        let Some(p) = p.as_str().filter(|s| !s.is_empty()) else {
            return Err(bad(format!(
                "path must be a non-empty string, got {}",
                py_repr_value(p)
            )));
        };
        let keys: Vec<String> = p.split('.').map(str::to_string).collect();
        if keys.iter().any(String::is_empty) {
            return Err(bad(format!("path {} has an empty step", py_repr(p))));
        }
        if keys[0] == "result" {
            result_paths.push(keys[1..].to_vec());
        } else if keys[0] == "payload" && keys.len() > 1 {
            let rest = &keys[1..];
            if rest == ["type"] {
                return Err(bad(
                    "cannot hide payload.type — the verb is how every row is read".to_string(),
                ));
            }
            if find(&item.payload, body, rest).is_none() {
                return Err(bad(format!(
                    "path {} names nothing in the payload",
                    py_repr(p)
                )));
            }
            payload_paths.push(rest.to_vec());
        } else {
            return Err(bad(format!(
                "path {} must start with 'payload.' or be 'result[.…]'",
                py_repr(p)
            )));
        }
        paths.push(p.to_string());
    }
    let mut values: Vec<String> = Vec::new();
    for kp in &payload_paths {
        match find(&item.payload, body, kp) {
            Some(Found::Json(v)) => strings(v, &mut values),
            Some(Found::Body(b)) => values.push(py_bytes_repr(b)),
            None => {}
        }
    }
    Ok(Some(Secret {
        paths,
        payload_paths,
        result_paths,
        values,
    }))
}

/// `obj` with the value at `keys` replaced by the placeholder (a copy; a missing
/// path leaves it as is). An empty `keys` is the whole value.
fn put(obj: &Value, keys: &[String]) -> Value {
    let Some((last, parents)) = keys.split_last() else {
        return Value::String(PLACEHOLDER.to_string());
    };
    if get(obj, keys).is_none() {
        return obj.clone();
    }
    let mut out = obj.clone();
    let mut cur = &mut out;
    for k in parents {
        cur = match cur {
            Value::Array(a) => match k.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                Some(v) => v,
                None => return obj.clone(),
            },
            Value::Object(m) => match m.get_mut(k) {
                Some(v) => v,
                None => return obj.clone(),
            },
            _ => return obj.clone(),
        };
    }
    let placeholder = Value::String(PLACEHOLDER.to_string());
    match cur {
        Value::Array(a) => {
            if let Some(slot) = last.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                *slot = placeholder;
            }
        }
        Value::Object(m) => {
            m.insert(last.clone(), placeholder);
        }
        _ => {}
    }
    out
}

/// Every occurrence of a secret value inside any string of `x` → placeholder —
/// an error text that quotes the key must not carry it into a row or a log.
/// Longest value first; object KEYS are left alone (python's `scrub`).
#[must_use]
pub fn scrub(x: &Value, values: &[String]) -> Value {
    if values.is_empty() {
        return x.clone();
    }
    let mut by_len: Vec<&String> = values.iter().collect();
    // python `sorted(values, key=len, reverse=True)` — stable, so equal lengths
    // keep their order. `len` counts CODE POINTS.
    by_len.sort_by_key(|v| std::cmp::Reverse(v.chars().count()));
    scrub_sorted(x, &by_len)
}

fn scrub_sorted(x: &Value, values: &[&String]) -> Value {
    match x {
        Value::String(s) => {
            let mut s = s.clone();
            for v in values {
                if s.contains(v.as_str()) {
                    s = s.replace(v.as_str(), PLACEHOLDER);
                }
            }
            Value::String(s)
        }
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), scrub_sorted(v, values)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|v| scrub_sorted(v, values)).collect()),
        other => other.clone(),
    }
}

/// The payload as recorded and shown: secret fields → placeholder (the raw body
/// too, when `payload.<its path>` is marked), then every secret value scrubbed out.
#[must_use]
pub fn shown_payload(payload: &Value, body: Option<&crate::outcome::Blob>, s: &Secret) -> Shown {
    let mut out = payload.clone();
    let mut body_hidden = false;
    let steps = body.map(crate::outcome::Blob::steps);
    for kp in &s.payload_paths {
        let live = match (body, steps) {
            (Some(b), Some(at)) if !body_hidden => Some((b, at)),
            _ => None,
        };
        let is_body = matches!(
            find(&out, live.map(|(b, at)| (at, b.bytes.as_slice())), kp),
            Some(Found::Body(_))
        );
        if let (true, Some((b, _))) = (is_body, live) {
            // the launch admitted only a path that names a slot of the payload
            let _ = b.place(&mut out, Value::String(PLACEHOLDER.to_string()));
            body_hidden = true;
        } else {
            out = put(&out, kp);
        }
    }
    Shown {
        payload: scrub(&out, &s.values),
        body_hidden,
    }
}

/// An outcome as recorded and shown: secret result fields → placeholder (a raw
/// reply body too, when `result` or `result.bytes` is marked), and every secret
/// value — the payload's and the result's own — scrubbed from its text.
#[must_use]
pub fn shown_outcome(out: &Outcome, s: &Secret) -> Outcome {
    let mut res = out.result.clone();
    let mut body = out.body.clone();
    let mut values = s.values.clone();
    for kp in &s.result_paths {
        if kp.is_empty() {
            strings_with_body(&res, body.as_ref(), &mut values);
            res = Value::String(PLACEHOLDER.to_string());
            body = None;
            continue;
        }
        let is_body = match find(
            &res,
            body.as_ref().map(|b| (b.steps(), b.bytes.as_slice())),
            kp,
        ) {
            Some(Found::Json(v)) => {
                strings(v, &mut values);
                false
            }
            Some(Found::Body(b)) => {
                values.push(py_bytes_repr(b));
                true
            }
            None => continue,
        };
        if is_body {
            if let Some(b) = &body {
                // found there, so the slot resolves
                let _ = b.place(&mut res, Value::String(PLACEHOLDER.to_string()));
            }
            body = None;
        } else {
            res = put(&res, kp);
        }
    }
    Outcome {
        pid: out.pid,
        status: out.status,
        result: scrub(&res, &values),
        attempts: out.attempts,
        body,
    }
}

/// The answer to re-sending an execution whose record holds placeholders.
#[must_use]
pub fn replay_refused(name_or_id: &Value) -> Value {
    let mut r = crate::refusal::refusal(
        crate::refusal::BAD_ARG,
        format!(
            "send: execution {} carried secrets — its record holds {PLACEHOLDER} \
             placeholders, not values, so it cannot be answered from the record; \
             send again under a new id",
            py_repr_value(name_or_id)
        ),
    );
    crate::refusal::with(&mut r, "arg", json!("id"));
    r
}
