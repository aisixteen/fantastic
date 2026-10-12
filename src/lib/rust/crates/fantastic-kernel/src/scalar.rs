//! The ONE scalar rule for text a caller typed.
//!
//! Python keeps it in `kernel/modes/parse.py` as `coerce_scalar`, and TWO paths
//! share it: the CLI's `k=v` and the model's `<k>v</k>`. It lives in the kernel
//! crate here for the same reason — `fantastic-ai-core` and `fantastic-cli` both
//! depend on this crate and neither depends on the other, so this is the only
//! place one rule can serve both.
//!
//! `true`/`false` → bool, an integer → int, a decimal → float, anything else →
//! the string as written. No JSON, no quoting: a value that has to stay text is
//! text, and a verb that needs a type gets the typed value or refuses. The CLI
//! layers a JSON object/list branch on top for the OPERATOR (`_coerce` in
//! python); the model↔harness path carries no JSON at all.

use serde_json::Value;

/// `s` with python's whitespace stripped from both ends.
///
/// NOT `str::trim`. Python's `str.strip()` removes every character where
/// `str.isspace()` is true, and that set includes the four ASCII separators
/// U+001C..U+001F, which Unicode's `White_Space` property (what `trim` uses)
/// does not. One character of difference matters here only because a value that
/// keeps a stray separator stops being a scalar the far side recognises.
#[must_use]
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// Type one scalar the way python's `coerce_scalar` does.
///
/// CPython parity notes, each one a real difference from a naive `parse()`:
///
/// - the bool test is case-insensitive (`v.lower() in ("true","false")`);
/// - `int()` and `float()` accept `_` between digits (`1_000`) and a leading
///   `+`, and reject a leading, trailing or doubled underscore;
/// - `int()` and `float()` both run `_PyUnicode_TransformDecimalAndSpaceToASCII`
///   first: every Unicode `Nd` digit becomes its ASCII value, mixed scripts
///   allowed in one literal (`int('1١')` == 11 — C72);
/// - `float()` accepts `inf`, `infinity` and `nan` in any case, with a sign.
///
/// TWO values a naive `i64`/`f64` cannot hold, both kept as an exact JSON
/// NUMBER rather than mangled into a string (C74/D13): an integer wider than
/// `i64` (python's ints are unbounded — `f64` would lose digits, so this crate
/// builds an ARBITRARY-PRECISION `serde_json::Number` instead; the workspace
/// turns on serde_json's `arbitrary_precision` feature for exactly this, at
/// the cost of every `Value::Number` in the workspace being string-backed
/// internally — measured against the full suite with no regressions) and a
/// non-finite float (`inf`, `-inf`, `nan`), stored as the bare token python's
/// `json.dumps(..., allow_nan=True)` — the kernel's default — writes.
#[must_use]
pub fn coerce_scalar(v: &str) -> Value {
    let lowered = v.to_ascii_lowercase();
    if lowered == "true" {
        return Value::Bool(true);
    }
    if lowered == "false" {
        return Value::Bool(false);
    }
    if let Some(n) = parse_python_int(v) {
        return n;
    }
    if let Ok(f) = fantastic_bundle::py_float(&Value::String(v.to_string())) {
        return float_value(f);
    }
    Value::String(v.to_string())
}

/// CPython `int(v)`, keeping every digit.
///
/// Unlike [`fantastic_bundle::py_int`] (which saturates at `i64::MAX`/`MIN` —
/// the right call for the ARITHMETIC callers that use it, a limit or a retry
/// count), a value too wide for `i64` here becomes an arbitrary-precision JSON
/// number: exactly what python's `int` holds and `json.dumps` prints, digit for
/// digit. Returns `None` for anything that is not a valid int() literal at all,
/// so the caller falls through to `float()`.
fn parse_python_int(v: &str) -> Option<Value> {
    let ascii = fantastic_bundle::transform_decimal_and_space_to_ascii(v);
    let trimmed =
        ascii.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'));
    let (negative, rest) = match trimmed.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if rest.is_empty() {
        return None;
    }
    let mut digits = String::with_capacity(rest.len());
    let mut saw_digit = false;
    let mut prev_underscore = false;
    for c in rest.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            saw_digit = true;
            prev_underscore = false;
        } else if c == '_' {
            if !saw_digit || prev_underscore {
                return None;
            }
            prev_underscore = true;
        } else {
            return None;
        }
    }
    if !saw_digit || prev_underscore {
        return None;
    }
    // Canonical digits — no leading zeros, python's own int() never prints any.
    let canon = digits.trim_start_matches('0');
    let canon = if canon.is_empty() { "0" } else { canon };
    let literal = if negative && canon != "0" {
        format!("-{canon}")
    } else {
        canon.to_string()
    };
    if let Ok(n) = literal.parse::<i64>() {
        return Some(Value::Number(n.into()));
    }
    // Wider than i64: arbitrary-precision, exact digits, still a JSON number.
    Some(Value::Number(serde_json::Number::from_string_unchecked(
        literal,
    )))
}

/// A python float as a JSON `Value`: finite as an ordinary number; non-finite
/// as the bare `Infinity`/`-Infinity`/`NaN` token python's `json.dumps` writes
/// (JSON proper has no such number, but that is what appears on the wire).
fn float_value(f: f64) -> Value {
    if let Some(n) = serde_json::Number::from_f64(f) {
        return Value::Number(n);
    }
    let literal = if f.is_nan() {
        "NaN"
    } else if f.is_sign_negative() {
        "-Infinity"
    } else {
        "Infinity"
    };
    Value::Number(serde_json::Number::from_string_unchecked(
        literal.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::coerce_scalar;
    use serde_json::json;

    #[test]
    fn the_four_cases() {
        assert_eq!(coerce_scalar("true"), json!(true));
        assert_eq!(coerce_scalar("TRUE"), json!(true));
        assert_eq!(coerce_scalar("False"), json!(false));
        assert_eq!(coerce_scalar("0"), json!(0));
        assert_eq!(coerce_scalar("12"), json!(12));
        assert_eq!(coerce_scalar("-5"), json!(-5));
        assert_eq!(coerce_scalar("+5"), json!(5));
        assert_eq!(coerce_scalar("1.5"), json!(1.5));
        assert_eq!(coerce_scalar("1e3"), json!(1000.0));
        assert_eq!(coerce_scalar("keep"), json!("keep"));
        assert_eq!(coerce_scalar(""), json!(""));
    }

    #[test]
    fn underscores_follow_cpython() {
        assert_eq!(coerce_scalar("1_0"), json!(10));
        assert_eq!(coerce_scalar("1_000_000"), json!(1_000_000));
        assert_eq!(coerce_scalar("1_0.5"), json!(10.5));
        // Every shape python's int()/float() refuse stays the string it was.
        assert_eq!(coerce_scalar("_1"), json!("_1"));
        assert_eq!(coerce_scalar("1_"), json!("1_"));
        assert_eq!(coerce_scalar("1__0"), json!("1__0"));
        assert_eq!(coerce_scalar("a_b"), json!("a_b"));
    }

    #[test]
    fn wide_integer_stays_an_integer() {
        // C74/D13: an int wider than i64 is still a NUMBER — an
        // arbitrary-precision one, exact digit for digit, never a string.
        let big = "123456789012345678901234567890";
        let v = coerce_scalar(big);
        assert!(v.is_number(), "{v:?} must be a number");
        assert_eq!(v.as_i64(), None, "too wide for i64");
        assert_eq!(serde_json::to_string(&v).unwrap(), big);
        assert_eq!(
            serde_json::to_string(&coerce_scalar("-00123456789012345678901234567890")).unwrap(),
            "-123456789012345678901234567890",
            "no leading zeros — python's int() never prints any"
        );
        assert_eq!(coerce_scalar("007"), json!(7));
    }

    #[test]
    fn inf_and_nan_are_floats() {
        // C74/D13: python types inf/-Infinity/nan as floats; the wire text is
        // the bare token `json.dumps(..., allow_nan=True)` writes — the
        // kernel's default.
        for (text, want) in [
            ("inf", "Infinity"),
            ("Infinity", "Infinity"),
            ("-inf", "-Infinity"),
            ("-INFINITY", "-Infinity"),
            ("NaN", "NaN"),
            ("nan", "NaN"),
        ] {
            let v = coerce_scalar(text);
            assert!(v.is_number(), "{text} must coerce to a number, got {v:?}");
            assert_eq!(serde_json::to_string(&v).unwrap(), want, "{text}");
        }
    }

    #[test]
    fn unicode_digits_follow_cpython() {
        // C72: mixed scripts allowed in one literal — every Unicode `Nd`
        // digit is its ASCII value, ASCII and non-ASCII interchangeably.
        assert_eq!(coerce_scalar("١٢"), json!(12)); // Arabic-Indic
        assert_eq!(coerce_scalar("1١"), json!(11)); // ASCII + Arabic-Indic
        assert_eq!(coerce_scalar("١.٥"), json!(1.5));
        assert_eq!(coerce_scalar("１２"), json!(12)); // fullwidth digits
                                                      // Numeric_Type=Digit (not Nd) stays text — CPython's int()/float()
                                                      // refuse it too.
        assert_eq!(coerce_scalar("²"), json!("²"));
    }

    #[test]
    fn py_strip_takes_the_ascii_separators_trim_leaves() {
        use super::py_strip;
        assert_eq!(py_strip("  a.md \n"), "a.md");
        assert_eq!(py_strip("\u{1f}12\u{1c}"), "12");
        assert_eq!(py_strip("\u{85}x\u{85}"), "x");
        assert_eq!(py_strip("keep me"), "keep me");
    }

    #[test]
    fn a_path_or_a_sentence_is_text() {
        assert_eq!(coerce_scalar("a.md"), json!("a.md"));
        assert_eq!(coerce_scalar("filler_2.txt"), json!("filler_2.txt"));
        assert_eq!(coerce_scalar("1.2.3"), json!("1.2.3"));
    }
}
