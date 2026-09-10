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
/// - `float()` accepts `inf`, `infinity` and `nan` in any case, with a sign.
///
/// TWO values python can hold and JSON cannot, both kept as the string they were
/// written as rather than silently mangled: a non-finite float (`inf`, `nan` —
/// `serde_json::Number` has no representation for either) and an integer wider
/// than `i64` (python's ints are unbounded; going through `f64` would lose
/// digits). Neither has ever appeared on the model path; both are named here so
/// a future reader knows it is a decision, not an oversight.
#[must_use]
pub fn coerce_scalar(v: &str) -> Value {
    let lowered = v.to_ascii_lowercase();
    if lowered == "true" {
        return Value::Bool(true);
    }
    if lowered == "false" {
        return Value::Bool(false);
    }
    if let Some(text) = without_underscores(v) {
        if let Ok(n) = text.parse::<i64>() {
            return Value::Number(n.into());
        }
        // AN INTEGER TOO WIDE FOR `i64` IS NOT A FLOAT. `f64` would take it and
        // lose digits (python keeps it exact, unbounded), so the text stands.
        if is_integer_shaped(&text) {
            return Value::String(v.to_string());
        }
        // `str::parse::<f64>` accepts exactly python's float grammar — decimals,
        // exponents, `inf`/`infinity`/`nan`, an optional sign — but NOT the
        // underscores python allows, which is why the digits are cleaned first.
        if let Ok(f) = text.parse::<f64>() {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return Value::Number(n);
            }
            // Non-finite: JSON has no such number. Keep the text.
            return Value::String(v.to_string());
        }
    }
    Value::String(v.to_string())
}

/// True when the text is a plain signed run of digits — an integer python would
/// hold exactly and `f64` would round.
fn is_integer_shaped(text: &str) -> bool {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// `v` with python's legal digit-separating underscores removed, or `None` when
/// an underscore sits where python would refuse it: at either end, beside
/// another underscore, or next to anything but a digit on both sides.
fn without_underscores(v: &str) -> Option<String> {
    if !v.contains('_') {
        return Some(v.to_string());
    }
    let chars: Vec<char> = v.chars().collect();
    let mut out = String::with_capacity(chars.len());
    for (i, c) in chars.iter().enumerate() {
        if *c != '_' {
            out.push(*c);
            continue;
        }
        let before = i.checked_sub(1).and_then(|j| chars.get(j))?;
        let after = chars.get(i + 1)?;
        if !before.is_ascii_digit() || !after.is_ascii_digit() {
            return None;
        }
    }
    Some(out)
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
    fn what_json_cannot_hold_stays_text() {
        assert_eq!(coerce_scalar("inf"), json!("inf"));
        assert_eq!(coerce_scalar("NaN"), json!("NaN"));
        assert_eq!(
            coerce_scalar("123456789012345678901234567890"),
            json!("123456789012345678901234567890")
        );
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
