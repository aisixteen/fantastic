//! CPython `repr()` / `str()` / truthiness over a JSON value.

use std::fmt::Write as _;

use serde_json::Value;

use crate::unicode::is_printable;

/// `repr(s)` for a python `str` — what `{x!r}` and `%r` print.
///
/// Single quotes, or double quotes when the text holds `'` and no `"`; the
/// chosen quote and `\` are backslash-escaped; TAB/LF/CR are `\t` `\n` `\r`;
/// every other C0 control and DEL is `\xHH`; a non-ASCII character CPython
/// deems unprintable is `\xHH`, `\uHHHH` or `\UHHHHHHHH`.
#[must_use]
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        let cp = c as u32;
        if c == quote || c == '\\' {
            out.push('\\');
            out.push(c);
        } else if c == '\t' {
            out.push_str("\\t");
        } else if c == '\n' {
            out.push_str("\\n");
        } else if c == '\r' {
            out.push_str("\\r");
        } else if cp < 0x20 || cp == 0x7f {
            let _ = write!(out, "\\x{cp:02x}");
        } else if cp < 0x7f || is_printable(c) {
            out.push(c);
        } else if cp <= 0xff {
            let _ = write!(out, "\\x{cp:02x}");
        } else if cp <= 0xffff {
            let _ = write!(out, "\\u{cp:04x}");
        } else {
            let _ = write!(out, "\\U{cp:08x}");
        }
    }
    out.push(quote);
    out
}

/// `repr(x)` for the python object a JSON value decodes to: `None`, `True`,
/// `False`, int digits, float repr, a quoted `str`, and lists/dicts written with
/// `, ` and `: ` and their elements' reprs.
#[must_use]
pub fn py_repr_value(v: &Value) -> String {
    let mut out = String::new();
    write_repr(v, &mut out);
    out
}

fn write_repr(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Number(n) => out.push_str(&number_repr(n)),
        Value::String(s) => out.push_str(&py_repr(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_repr(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&py_repr(k));
                out.push_str(": ");
                write_repr(item, out);
            }
            out.push('}');
        }
    }
}

/// `str(x)` — what `{x}` and `%s` print: a `str` as itself, everything else as
/// its repr (a container's elements are always repr'd).
#[must_use]
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr_value(other),
    }
}

/// python truthiness: `None`, `False`, `0`, `0.0`, `""`, `[]` and `{}` are false.
#[must_use]
pub fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(u) = n.as_u64() {
                u != 0
            } else {
                n.as_f64().is_some_and(|f| f != 0.0)
            }
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// A JSON number the way python writes it: an integer's digits, a float's repr.
pub(crate) fn number_repr(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else {
        py_float_repr(n.as_f64().unwrap_or(0.0))
    }
}

/// `repr(f)` for a python float: the shortest round-tripping digits, positional
/// when the decimal point sits within `-4 < decpt <= 16`, else `d.ddde±XX` with at
/// least two exponent digits; a whole positional float keeps its `.0`.
#[must_use]
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    let sign = if f.is_sign_negative() { "-" } else { "" };
    if f == 0.0 {
        return format!("{sign}0.0");
    }
    // Rust's `{:e}` is the shortest round-trip form, the same digits CPython's
    // `repr` picks (both are shortest-roundtrip); only the layout differs.
    let exp_form = format!("{:e}", f.abs());
    let (mantissa, exp) = exp_form
        .split_once('e')
        .expect("LowerExp always writes an exponent");
    let exp: i64 = exp.parse().expect("LowerExp exponent is an integer");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    // value = 0.<digits> × 10^decpt
    let decpt = exp + 1;
    if decpt <= -4 || decpt > 16 {
        let mut m = digits[..1].to_string();
        if digits.len() > 1 {
            m.push('.');
            m.push_str(&digits[1..]);
        }
        let x = decpt - 1;
        let xsign = if x < 0 { '-' } else { '+' };
        return format!("{sign}{m}e{xsign}{:02}", x.abs());
    }
    if decpt <= 0 {
        let zeros = "0".repeat(usize::try_from(-decpt).expect("decpt in (-4, 0]"));
        return format!("{sign}0.{zeros}{digits}");
    }
    let point = usize::try_from(decpt).expect("decpt in (0, 16]");
    if point >= digits.len() {
        let zeros = "0".repeat(point - digits.len());
        format!("{sign}{digits}{zeros}.0")
    } else {
        format!("{sign}{}.{}", &digits[..point], &digits[point..])
    }
}
