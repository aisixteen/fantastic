//! python `io_bridge._codec.set_path` — WHERE a raw body sits in an envelope.
//!
//! A binary frame's `_binary_path` is a JSON ARRAY of steps from the envelope
//! root: a STRING step is an object key, taken literally (it may hold `.`, be
//! empty or look like a number); an INTEGER step is a list index.
//! `["payload", "a.b"]` is the top-level payload key `"a.b"`, never `a` → `b`;
//! `["data", "items", 0, "data"]` goes through a list. A dotted string could not
//! tell those apart, so it is refused.
//!
//! [`Step`] is that array's typed form — the kernel's `Blob` path, a watcher
//! item's body, a reply's body. Every place a body is put or taken goes through
//! [`set_path`] (a path off the wire, checked step by step with python's own
//! refusal texts) or its typed twins [`place`] / [`take`].

use serde_json::Value;

/// One step of a body's location: an object key, taken literally, or a list
/// index. The JSON spelling is a string or an integer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Step {
    /// An object key — literal: `"a.b"` is ONE key.
    Key(String),
    /// A list index.
    Index(usize),
}

impl Step {
    /// The step as `_binary_path` spells it: a string or an integer.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Step::Key(k) => Value::String(k.clone()),
            Step::Index(i) => Value::from(*i),
        }
    }
}

impl From<&str> for Step {
    fn from(k: &str) -> Self {
        Step::Key(k.to_string())
    }
}

impl From<String> for Step {
    fn from(k: String) -> Self {
        Step::Key(k)
    }
}

impl From<usize> for Step {
    fn from(i: usize) -> Self {
        Step::Index(i)
    }
}

/// A path as `_binary_path` spells it: a JSON array of steps.
#[must_use]
pub fn steps_value(path: &[Step]) -> Value {
    Value::Array(path.iter().map(Step::to_value).collect())
}

/// The JSON name of a value, for an error — python `_kind` for a scalar (the
/// only kind [`set_path`] names), `object` / `list` for a container.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

/// A step off the wire as a list index: an INTEGER, not negative — python's
/// `type(step) is int and step >= 0` (a bool, a float such as `0.0`, a string
/// such as `"1"` are not). `None` otherwise.
fn index_of(step: &Value) -> Option<u64> {
    let Value::Number(n) = step else {
        return None;
    };
    if n.to_string().contains(['.', 'e', 'E']) {
        return None;
    }
    n.as_u64()
}

/// Put `value` at `path` — a `_binary_path` off the wire, a non-empty JSON array
/// of steps — in `obj`: python's `set_path`. Checked step by step: at an object
/// the step is a string key (every key before the last must exist; the last is
/// set, in place when present, appended when absent), at a list a non-negative
/// integer index inside it. Returns the path's typed steps.
///
/// # Errors
/// Python's `ValueError` text for the first step that does not hold — exactly
/// one of: `not a list of steps`, `an empty path names no slot`, `step <repr> at
/// an object is not a string key`, `step <repr> at a list is not a non-negative
/// integer index`, `index <i> is past the end of a list of <n>`, `no key <repr>`,
/// `step <repr> goes into a <null|boolean|number|string>, not a container`.
pub fn set_path(obj: &mut Value, path: &Value, value: Value) -> Result<Vec<Step>, String> {
    let Value::Array(steps) = path else {
        return Err("not a list of steps".to_string());
    };
    if steps.is_empty() {
        return Err("an empty path names no slot".to_string());
    }
    let mut typed = Vec::with_capacity(steps.len());
    let mut cur = obj;
    let mut value = Some(value);
    for (n, step) in steps.iter().enumerate() {
        let last = n == steps.len() - 1;
        let repr = || crate::py_repr_value(step);
        cur = match cur {
            Value::Object(m) => {
                let Value::String(k) = step else {
                    return Err(format!("step {} at an object is not a string key", repr()));
                };
                typed.push(Step::Key(k.clone()));
                if last {
                    m.insert(k.clone(), value.take().expect("set once"));
                    return Ok(typed);
                }
                match m.get_mut(k) {
                    Some(next) => next,
                    None => return Err(format!("no key {}", repr())),
                }
            }
            Value::Array(a) => {
                let Some(i) = index_of(step) else {
                    return Err(format!(
                        "step {} at a list is not a non-negative integer index",
                        repr()
                    ));
                };
                let len = a.len();
                let Some(i) = usize::try_from(i).ok().filter(|i| *i < len) else {
                    return Err(format!("index {i} is past the end of a list of {len}"));
                };
                typed.push(Step::Index(i));
                if last {
                    a[i] = value.take().expect("set once");
                    return Ok(typed);
                }
                &mut a[i]
            }
            other => {
                return Err(format!(
                    "step {} goes into a {}, not a container",
                    repr(),
                    kind(other)
                ))
            }
        };
    }
    unreachable!("the last step returns")
}

/// [`set_path`] over a typed path — the same rules, the same refusals.
///
/// # Errors
/// What [`set_path`] answers for the path.
pub fn place(obj: &mut Value, path: &[Step], value: Value) -> Result<(), String> {
    set_path(obj, &steps_value(path), value).map(|_| ())
}

/// The value at `path` in `obj`, when every step resolves: a key at an object,
/// an index at a list.
#[must_use]
pub fn get<'a>(obj: &'a Value, path: &[Step]) -> Option<&'a Value> {
    let mut cur = obj;
    for step in path {
        cur = match (cur, step) {
            (Value::Object(m), Step::Key(k)) => m.get(k)?,
            (Value::Array(a), Step::Index(i)) => a.get(*i)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Take the value at `path` out of `obj`: an object key is REMOVED (python
/// `del d[k]`, the other keys keep their order), a list slot becomes `null`
/// (removing it would shift every later index).
///
/// # Errors
/// The path is empty or does not resolve to an existing slot.
pub fn take(obj: &mut Value, path: &[Step]) -> Result<(), String> {
    let Some((last, head)) = path.split_last() else {
        return Err("an empty path names no slot".to_string());
    };
    let mut cur = obj;
    for step in head {
        cur = match (cur, step) {
            (Value::Object(m), Step::Key(k)) => m
                .get_mut(k)
                .ok_or_else(|| format!("no key {}", crate::py_repr(k)))?,
            (Value::Array(a), Step::Index(i)) => {
                let len = a.len();
                a.get_mut(*i)
                    .ok_or_else(|| format!("index {i} is past the end of a list of {len}"))?
            }
            (other, s) => {
                return Err(format!(
                    "step {} does not resolve in a {}",
                    crate::py_repr_value(&s.to_value()),
                    kind(other)
                ))
            }
        };
    }
    match (cur, last) {
        (Value::Object(m), Step::Key(k)) => m
            .shift_remove(k)
            .map(|_| ())
            .ok_or_else(|| format!("no key {}", crate::py_repr(k))),
        (Value::Array(a), Step::Index(i)) => {
            let len = a.len();
            let slot = a
                .get_mut(*i)
                .ok_or_else(|| format!("index {i} is past the end of a list of {len}"))?;
            *slot = Value::Null;
            Ok(())
        }
        (other, s) => Err(format!(
            "step {} does not resolve in a {}",
            crate::py_repr_value(&s.to_value()),
            kind(other)
        )),
    }
}
