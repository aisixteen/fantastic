//! SECRET VALUES on a call — the caller marks them, the kernel never records them.
//!
//! Port of python `kernel/_secret.py` (canonical, owner 2026-10-01).
//!
//! A batch item may carry `secret: [mark, …]` beside `target`/`payload`/`timeout`
//! ([`crate::outcome::Item::with_secret`]). A mark is rooted at the item:
//! `payload` + a field of the payload, `result` (the whole reply), or `result` + a
//! field of it. The TARGET is handed the real payload and the CALLER the real
//! reply; every place the kernel RECORDS or SHOWS the call carries `<secret>`
//! instead — the call and settle rows (and so the memory store), the `ledger`
//! read-back and dedup, the state stream's `payload` and `summary`, the inbox and
//! watcher mirrors, and any error text the call's outcome carries.
//!
//! A mark is spelled one of two ways (one list may mix them):
//!   * a DOTTED STRING — `payload.key`, `result`, `result.fields.api_key` — split
//!     on its dots; at a list, a step of ASCII digits is an index;
//!   * an ARRAY OF STEPS (owner, 2026-10-03) — `["payload", "a.b"]`,
//!     `["result", "items", 0]` — exact, with the step rules of the binary frame's
//!     `_binary_path`: a STRING step is an object key taken literally (it may hold
//!     a `.`), an INTEGER step (non-negative) a list index. It is the only way to
//!     mark a key that holds a dot.
//!
//! Two limits, by design:
//!   * masking is not isolation — the receiver holds the value; send secrets to a
//!     bundle that keeps them, never to an AI agent (they would enter its prompt
//!     and its memory);
//!   * a call whose record holds placeholders cannot be answered FROM that
//!     record: re-sending a named execution with secrets after it left RAM is
//!     refused, loud.
//!
//! Not secret-able: `payload.type` — the verb is how every row is read; and any
//! payload field of `create_agent`/`update_agent` — the kernel writes those into
//! the agent's RECORD (its `agent_created`/`agent_updated` events, `agent.yaml`),
//! where no mark can hold. A value an agent must keep belongs in a
//! `credential_store`, its record naming the ref (owner-approved 2026-10-03,
//! audit S2).
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

/// The verbs whose every payload field (`type` aside) lands in an agent's record
/// — the id, the handler, the placement, the meta. A mark there is refused
/// ([`check`]). Python `_RECORD_VERBS`.
const RECORD_VERBS: [&str; 2] = ["create_agent", "update_agent"];

/// One mark below its root (`payload` / `result`): the steps, and whether they
/// are EXACT (an array mark: a key step is only an object key, an index step only
/// a list index) or DOTTED (every step a key, which at a list is an index when it
/// is ASCII digits). Python `_Path`.
#[derive(Debug, Clone)]
struct Path {
    keys: Vec<Step>,
    exact: bool,
}

/// What one item asked to be kept: the payload/result paths, the marks as given
/// (recorded on the call row — marks are not secret), and the payload's secret
/// VALUES (RAM only — scrubbed out of every recorded string).
#[derive(Debug, Clone)]
pub struct Secret {
    /// The marks exactly as the item gave them — each a dotted string or an
    /// array of steps.
    pub paths: Vec<Value>,
    payload_paths: Vec<Path>,
    result_paths: Vec<Path>,
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

/// python `key.isascii() and key.isdigit()`.
fn is_digits(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit())
}

/// The list index a DOTTED step names — python `int(key)` after the digits test:
/// a digit run too long for a usize is past any list's end either way.
fn dotted_index(key: &str) -> Option<usize> {
    is_digits(key).then(|| key.parse::<usize>().ok()).flatten()
}

fn step<'a>(cur: &'a Value, key: &Step, exact: bool) -> Option<&'a Value> {
    match (cur, key) {
        (Value::Object(m), Step::Key(k)) => m.get(k),
        // an index step comes only from an array mark
        (Value::Array(a), Step::Index(i)) => a.get(*i),
        (Value::Array(a), Step::Key(k)) if !exact => dotted_index(k).and_then(|i| a.get(i)),
        _ => None,
    }
}

fn step_mut<'a>(cur: &'a mut Value, key: &Step, exact: bool) -> Option<&'a mut Value> {
    match (cur, key) {
        (Value::Object(m), Step::Key(k)) => m.get_mut(k),
        (Value::Array(a), Step::Index(i)) => a.get_mut(*i),
        (Value::Array(a), Step::Key(k)) if !exact => {
            dotted_index(k).and_then(move |i| a.get_mut(i))
        }
        _ => None,
    }
}

fn get<'a>(obj: &'a Value, path: &Path) -> Option<&'a Value> {
    let mut cur = obj;
    for k in &path.keys {
        cur = step(cur, k, path.exact)?;
    }
    Some(cur)
}

/// A raw body beside a JSON object: the steps to the slot it sits at
/// ([`crate::outcome::Blob::steps`]), and its bytes.
type BodyAt<'a> = Option<(&'a [Step], &'a [u8])>;

/// Whether the mark's step `k` names the body's step `p` — the same step `step`
/// takes: an object key verbatim, a list index by an index step or (dotted) by
/// its digits.
fn names_step(k: &Step, p: &Step, exact: bool) -> bool {
    match (k, p) {
        (Step::Key(a), Step::Key(b)) => a == b,
        (Step::Index(a), Step::Index(b)) => a == b,
        (Step::Key(a), Step::Index(i)) => !exact && dotted_index(a) == Some(*i),
        (Step::Index(_), Step::Key(_)) => false,
    }
}

/// `get` over the value AND its body: the body is the value at its own path of
/// an object, and nothing lies below it (python `_step` into bytes fails).
fn find<'a>(root: &'a Value, body: BodyAt<'a>, path: &Path) -> Option<Found<'a>> {
    let keys = &path.keys;
    if let (Some((at, b)), Value::Object(_)) = (body, root) {
        if keys.len() >= at.len()
            && keys
                .iter()
                .zip(at)
                .all(|(k, p)| names_step(k, p, path.exact))
        {
            return (keys.len() == at.len()).then_some(Found::Body(b));
        }
    }
    get(root, path).map(Found::Json)
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

/// Where a mark is rooted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Root {
    Payload,
    Result,
}

/// A dotted-string mark → (root, path below it), or why not. Python `_dotted`.
fn dotted(p: &str) -> Result<(Root, Path), String> {
    if p.is_empty() {
        return Err(format!(
            "path must be a non-empty string, got {}",
            py_repr(p)
        ));
    }
    let keys: Vec<&str> = p.split('.').collect();
    if keys.iter().any(|k| k.is_empty()) {
        return Err(format!("path {} has an empty step", py_repr(p)));
    }
    let below = || Path {
        keys: keys[1..]
            .iter()
            .map(|k| Step::Key((*k).to_string()))
            .collect(),
        exact: false,
    };
    if keys[0] == "result" {
        return Ok((Root::Result, below()));
    }
    if keys[0] == "payload" && keys.len() > 1 {
        return Ok((Root::Payload, below()));
    }
    Err(format!(
        "path {} must start with 'payload.' or be 'result[.…]'",
        py_repr(p)
    ))
}

/// An array-of-steps mark → (root, path below it), or why not. Python `_array`:
/// a step is a string (an object key) or a non-negative integer (a list index) —
/// never a bool, a float, a negative number, null, a list or an object.
fn array(p: &Value, steps: &[Value]) -> Result<(Root, Path), String> {
    if steps.is_empty() {
        return Err("path [] has no steps".to_string());
    }
    let mut keys: Vec<Step> = Vec::with_capacity(steps.len());
    for s in steps {
        keys.push(match s {
            Value::String(k) => Step::Key(k.clone()),
            // python's `int`, however wide: the number keeps the digits it was
            // read from (`arbitrary_precision`), and a bare digit run past
            // `u64` is still an integer — an index past any list's end, as is
            // one past `usize`. A float spelling of it (`.`, `e`) is not.
            Value::Number(n) if n.as_u64().is_some() || is_digits(n.as_str()) => Step::Index(
                n.as_u64()
                    .and_then(|i| usize::try_from(i).ok())
                    .unwrap_or(usize::MAX),
            ),
            other => {
                return Err(format!(
                    "path {} step {} is neither a string (an object key) nor a \
                     non-negative integer (a list index)",
                    py_repr_value(p),
                    py_repr_value(other)
                ))
            }
        });
    }
    let below = |keys: &[Step]| Path {
        keys: keys[1..].to_vec(),
        exact: true,
    };
    if steps[0] == "result" {
        return Ok((Root::Result, below(&keys)));
    }
    if steps[0] == "payload" && steps.len() > 1 {
        return Ok((Root::Payload, below(&keys)));
    }
    Err(format!(
        "path {} must start with 'payload' and a step under it, or with 'result'",
        py_repr_value(p)
    ))
}

/// Validate an item's `secret`: `Ok(None)` when absent (or null) — nothing
/// secret; `Err(refusal)` for a malformed mark. A payload mark the payload does
/// not have is refused — a typo would otherwise record the value it meant to
/// hide.
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
    let mut payload_paths: Vec<Path> = Vec::new();
    let mut result_paths: Vec<Path> = Vec::new();
    let record_verb = item
        .payload
        .get("type")
        .and_then(Value::as_str)
        .filter(|v| RECORD_VERBS.contains(v));
    for p in list {
        let parsed = match p {
            Value::String(s) => dotted(s),
            Value::Array(a) => array(p, a),
            other => {
                return Err(bad(format!(
                    "path must be a dotted string or an array of steps, got {}",
                    py_repr_value(other)
                )))
            }
        };
        let (root, path) = parsed.map_err(bad)?;
        if root == Root::Result {
            result_paths.push(path);
            continue;
        }
        if matches!(path.keys.as_slice(), [Step::Key(k)] if k == "type") {
            return Err(bad(
                "cannot hide payload.type — the verb is how every row is read".to_string(),
            ));
        }
        if let Some(verb) = record_verb {
            return Err(bad(format!(
                "path {} names a field {verb} writes into the agent record: a record \
                 field cannot be secret — use a credential_store ref",
                py_repr_value(p)
            )));
        }
        if find(&item.payload, body, &path).is_none() {
            return Err(bad(format!(
                "path {} names nothing in the payload",
                py_repr_value(p)
            )));
        }
        payload_paths.push(path);
    }
    let mut values: Vec<String> = Vec::new();
    for kp in &payload_paths {
        match find(&item.payload, body, kp) {
            Some(Found::Json(v)) => strings(v, &mut values),
            Some(Found::Body(b)) => values.push(py_bytes_repr(b)),
            None => {}
        }
    }
    // the marks AS GIVEN — what the call row records and what the handler reads
    // back as `current_secret()`
    Ok(Some(Secret {
        paths: list.clone(),
        payload_paths,
        result_paths,
        values,
    }))
}

/// `obj` with the value at `path` replaced by the placeholder (a copy; a missing
/// path leaves it as is). No steps is the whole value.
fn put(obj: &Value, path: &Path) -> Value {
    if path.keys.is_empty() {
        return Value::String(PLACEHOLDER.to_string());
    }
    if get(obj, path).is_none() {
        return obj.clone();
    }
    let mut out = obj.clone();
    let mut cur = &mut out;
    for k in &path.keys {
        cur = match step_mut(cur, k, path.exact) {
            Some(v) => v,
            None => return obj.clone(),
        };
    }
    *cur = Value::String(PLACEHOLDER.to_string());
    out
}

/// Every occurrence of a secret value inside any string of `x` → placeholder —
/// an error text that quotes the key must not carry it into a row or a log.
/// Longest value first. An object KEY, a NUMBER and a BOOL whose text — its
/// [`py_str`] (`True`/`False` for a bool) — EQUALS a secret value are the
/// placeholder too: a reply keyed by the token it was given, a PIN echoed as an
/// int (owner-approved 2026-10-03, audit S7). Equal, not contained: a key is
/// structure, and `type` must survive a one-letter secret. Two keys that scrub
/// alike keep the later value. Python's `scrub`.
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

/// A key's, a number's or a bool's text IS a secret value (equal, not contained).
fn is_secret_text(text: &str, values: &[&String]) -> bool {
    values.iter().any(|v| v.as_str() == text)
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
                .map(|(k, v)| {
                    let key = if is_secret_text(k, values) {
                        PLACEHOLDER.to_string()
                    } else {
                        k.clone()
                    };
                    (key, scrub_sorted(v, values))
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|v| scrub_sorted(v, values)).collect()),
        Value::Number(_) | Value::Bool(_) if is_secret_text(&py_str(x), values) => {
            Value::String(PLACEHOLDER.to_string())
        }
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
        if kp.keys.is_empty() {
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
