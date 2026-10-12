//! CPython's per-character predicates, from tables generated out of CPython
//! itself (`unicode_tables.rs`), so a character class is python's and not the
//! Unicode version this Rust toolchain happens to ship.
//!
//! Regenerate (CPython 3.11, the version the python kernel runs):
//!
//! ```text
//! import unicodedata
//! def ranges(pred):
//!     out, start = [], None
//!     for c in range(0x110000):
//!         p = pred(c)
//!         if p and start is None: start = c
//!         if not p and start is not None: out.append((start, c - 1)); start = None
//!     if start is not None: out.append((start, 0x10FFFF))
//!     return out
//! NON_PRINTABLE = ranges(lambda c: not chr(c).isprintable())
//! SPACE = [c for c in range(0x110000) if chr(c).isspace()]
//! DECIMAL = runs of unicodedata.decimal(chr(c), -1) >= 0 as (first, last, value_of_first)
//! ALNUM = ranges(lambda c: chr(c).isalnum())
//! DIGIT_NON_DECIMAL = ranges(lambda c: chr(c).isdigit() and not chr(c).isdecimal())
//! ```

use crate::unicode_tables::{ALNUM, DECIMAL, DIGIT_NON_DECIMAL, NON_PRINTABLE, SPACE};

/// CPython `str.isprintable()` for one character.
#[must_use]
pub fn is_printable(c: char) -> bool {
    let cp = c as u32;
    NON_PRINTABLE
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_err()
}

/// CPython `str.isspace()` for one character.
#[must_use]
pub fn is_space(c: char) -> bool {
    SPACE.binary_search(&(c as u32)).is_ok()
}

/// CPython `unicodedata.decimal(c)`: the decimal digit value of a Unicode `Nd`
/// character, or `None`.
#[must_use]
pub fn decimal_value(c: char) -> Option<u32> {
    let cp = c as u32;
    let i = DECIMAL.partition_point(|&(_, hi, _)| hi < cp);
    match DECIMAL.get(i) {
        Some(&(lo, _, first)) if lo <= cp => Some(first + (cp - lo)),
        _ => None,
    }
}

/// CPython `str.isdigit()` for one character: `Numeric_Type=Digit`, which is
/// the decimal digits ([`decimal_value`]) PLUS the compatibility digit forms
/// (`²`, `①`, `⑽`, `➀`).
///
/// NOT rust's `char::is_numeric`, which is the wider `Numeric_Type=Numeric` and
/// admits `½` and `Ⅷ`; NOT `char::is_ascii_digit`, which is narrower than
/// python by every non-Latin script.
#[must_use]
pub fn is_digit(c: char) -> bool {
    if decimal_value(c).is_some() {
        return true;
    }
    let cp = c as u32;
    DIGIT_NON_DECIMAL
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// CPython `str.isalnum()` for one character — the class python's `re` calls
/// `\w` (with `_`).
///
/// NOT rust's `char::is_alphanumeric()`, which also admits Other_Alphabetic
/// (U+093E DEVANAGARI VOWEL SIGN AA, U+0345 COMBINING GREEK YPOGEGRAMMENI,
/// U+24B6 CIRCLED LATIN CAPITAL LETTER A): python's is Lu/Ll/Lt/Lm/Lo plus the
/// numeric categories, so `<k_नाम>` and `<xⒶ>` are NOT tag names to python.
#[must_use]
pub fn is_alnum(c: char) -> bool {
    let cp = c as u32;
    ALNUM
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// CPython `_PyUnicode_TransformDecimalAndSpaceToASCII`: the FIRST step of both
/// `int(s)` and `float(s)` on a string — every Unicode space becomes ASCII ` `,
/// every Unicode `Nd` digit ([`decimal_value`]) its ASCII digit, and mixed
/// scripts are allowed in one literal (`int('1١')` == 11). Anything else
/// (a letter, a non-decimal digit form) passes through unchanged, so the
/// ASCII-only grammar that runs next refuses it exactly as CPython does.
#[must_use]
pub fn transform_decimal_and_space_to_ascii(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii() {
                c
            } else if is_space(c) {
                ' '
            } else if let Some(d) = decimal_value(c) {
                char::from_digit(d, 10).expect("decimal_value is 0..=9")
            } else {
                c
            }
        })
        .collect()
}
