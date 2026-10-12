//! CPython `int(x)` over a JSON value.

use serde_json::Value;

use crate::repr::{py_repr, py_truthy};
use crate::unicode::transform_decimal_and_space_to_ascii;

/// A python exception a helper would have raised: its class and its `str(e)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PyError {
    /// `TypeError: <message>`.
    TypeError(String),
    /// `ValueError: <message>`.
    ValueError(String),
}

impl PyError {
    /// The exception class name (`TypeError`, `ValueError`).
    #[must_use]
    pub fn class_name(&self) -> &'static str {
        match self {
            PyError::TypeError(_) => "TypeError",
            PyError::ValueError(_) => "ValueError",
        }
    }

    /// `str(e)` — the message alone.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            PyError::TypeError(m) | PyError::ValueError(m) => m,
        }
    }
}

impl std::fmt::Display for PyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for PyError {}

/// python's type name for the object a JSON value decodes to (`type(x).__name__`).
#[must_use]
pub fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `int(x)`.
///
/// A bool is 0/1, a float truncates toward zero, a string parses as CPython's
/// base-10 literal (python whitespace stripped, optional sign, Unicode `Nd`
/// digits, single `_` between digits); `None`, a list and a dict raise
/// `TypeError`. An integer wider than `i64` saturates (D13: `serde_json` holds
/// no wider integer).
///
/// # Errors
/// python's `TypeError` / `ValueError`, with CPython's message.
pub fn py_int(v: &Value) -> Result<i64, PyError> {
    match v {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if n.as_u64().is_some() {
                Ok(i64::MAX)
            } else {
                // `as` truncates toward zero and saturates, which is `int(f)`
                // for every finite float in range.
                Ok(n.as_f64().unwrap_or(0.0) as i64)
            }
        }
        Value::String(s) => parse_int_literal(s),
        other => Err(PyError::TypeError(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            py_type_name(other)
        ))),
    }
}

/// `int(x or default)`.
///
/// # Errors
/// python's `TypeError` / `ValueError` for a truthy `x` that `int()` rejects.
pub fn py_int_or(v: &Value, default: i64) -> Result<i64, PyError> {
    if py_truthy(v) {
        py_int(v)
    } else {
        Ok(default)
    }
}

/// CPython `PyLong_FromUnicodeObject(s, 10)`: every non-ASCII space becomes ` `,
/// every non-ASCII decimal digit its ASCII digit, then the ASCII literal parser
/// runs over the whole buffer.
fn parse_int_literal(s: &str) -> Result<i64, PyError> {
    let invalid = || {
        let shown: String = py_repr(s).chars().take(200).collect();
        PyError::ValueError(format!("invalid literal for int() with base 10: {shown}"))
    };
    let ascii: Vec<u8> = transform_decimal_and_space_to_ascii(s)
        .chars()
        .map(|c| if c.is_ascii() { c as u8 } else { b'?' })
        .collect();
    let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r');
    let mut i = 0;
    while i < ascii.len() && is_ws(ascii[i]) {
        i += 1;
    }
    let negative = match ascii.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut value: i128 = 0;
    let mut saw_digit = false;
    let mut prev_underscore = false;
    while let Some(&b) = ascii.get(i) {
        if b.is_ascii_digit() {
            value = (value * 10 + i128::from(b - b'0')).min(i128::from(i64::MAX) + 1);
            saw_digit = true;
            prev_underscore = false;
        } else if b == b'_' {
            if !saw_digit || prev_underscore {
                return Err(invalid());
            }
            prev_underscore = true;
        } else {
            break;
        }
        i += 1;
    }
    if !saw_digit || prev_underscore {
        return Err(invalid());
    }
    while i < ascii.len() && is_ws(ascii[i]) {
        i += 1;
    }
    if i != ascii.len() {
        return Err(invalid());
    }
    let signed = if negative { -value } else { value };
    Ok(i64::try_from(signed).unwrap_or(if negative { i64::MIN } else { i64::MAX }))
}

/// `float(x)`.
///
/// A bool is 0.0/1.0, a number is itself, a string parses as CPython's float
/// literal (non-ASCII spaces and C `isspace` stripped, never `\x1c`-`\x1f`;
/// `inf`/`infinity`/`nan` in any case; single `_` between digits); `None`, a
/// list and a dict raise `TypeError`.
///
/// # Errors
/// python's `TypeError` / `ValueError`, with CPython's message.
pub fn py_float(v: &Value) -> Result<f64, PyError> {
    match v {
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => parse_float_literal(s),
        other => Err(PyError::TypeError(format!(
            "float() argument must be a string or a real number, not '{}'",
            py_type_name(other)
        ))),
    }
}

/// CPython `PyFloat_FromString`.
fn parse_float_literal(s: &str) -> Result<f64, PyError> {
    let invalid =
        || PyError::ValueError(format!("could not convert string to float: {}", py_repr(s)));
    // Unicode `Nd` digits and NON-ASCII spaces to ASCII FIRST — the same first
    // step `int()` takes (`transform_decimal_and_space_to_ascii`) — then CPython
    // strips C `isspace` (` \t\n\v\f\r`) only: `\x1c`-`\x1f` are `str.isspace()`
    // but an ASCII char is never transformed, so `float('\x1c7')` is a
    // ValueError. Then `_` is allowed only between digits — a leading, trailing
    // or doubled one is a ValueError.
    let transformed = transform_decimal_and_space_to_ascii(s);
    let body: String = transformed
        .trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'))
        .to_string();
    if body.is_empty() {
        return Err(invalid());
    }
    let mut cleaned = String::with_capacity(body.len());
    let bytes: Vec<char> = body.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        if *c == '_' {
            let prev = i.checked_sub(1).and_then(|j| bytes.get(j));
            let next = bytes.get(i + 1);
            if !prev.is_some_and(char::is_ascii_digit) || !next.is_some_and(char::is_ascii_digit) {
                return Err(invalid());
            }
            continue;
        }
        cleaned.push(*c);
    }
    let (sign, rest) = match cleaned.strip_prefix('-') {
        Some(r) => (-1.0, r.to_string()),
        None => (
            1.0,
            cleaned.strip_prefix('+').unwrap_or(&cleaned).to_string(),
        ),
    };
    let lower = rest.to_ascii_lowercase();
    if lower == "inf" || lower == "infinity" {
        return Ok(sign * f64::INFINITY);
    }
    if lower == "nan" {
        return Ok(f64::NAN);
    }
    // rust's parser accepts exactly python's remaining grammar except for the
    // special words handled above and the forms python also refuses.
    if lower.is_empty()
        || !lower
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | '+' | '-'))
    {
        return Err(invalid());
    }
    lower
        .parse::<f64>()
        .map(|f| sign * f)
        .map_err(|_| invalid())
}
