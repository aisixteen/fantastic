//! CPython's string splitters and strippers.
//!
//! All three run over CPython's OWN whitespace predicate ([`is_space`], generated
//! from CPython 3.11), which is NOT rust's `char::is_whitespace`: python counts
//! the information separators `\x1c`–`\x1f` as whitespace and rust does not. A
//! grep that used rust's predicate missed a line python found, and a `strip()`
//! that used it left a separator on a title.
//!
//! `splitlines` is its own set again — the boundaries are `\n`, `\r`, `\r\n`,
//! `\v`, `\f`, `\x1c`, `\x1d`, `\x1e`, `\x85`, U+2028 and U+2029. `\x1f` is
//! whitespace but NOT a line boundary, so the two functions genuinely differ.

use crate::unicode::{is_digit, is_space};

/// `str.strip()` — python whitespace off both ends.
#[must_use]
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `str.split()` with no separator: runs of python whitespace split, and leading
/// / trailing runs produce no empty fields.
#[must_use]
pub fn py_split_whitespace(s: &str) -> Vec<&str> {
    s.split(is_space).filter(|p| !p.is_empty()).collect()
}

/// `str.splitlines()` — the boundary is any of `\n`, `\r`, `\r\n`, `\v`, `\f`,
/// `\x1c`, `\x1d`, `\x1e`, `\x85`, U+2028, U+2029, and a trailing boundary does
/// NOT yield a final empty line.
#[must_use]
pub fn py_splitlines(s: &str) -> Vec<&str> {
    fn is_boundary(c: char) -> bool {
        matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        )
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = s.as_bytes();
    let mut it = s.char_indices();
    while let Some((i, c)) = it.next() {
        if !is_boundary(c) {
            continue;
        }
        out.push(&s[start..i]);
        let mut end = i + c.len_utf8();
        // `\r\n` is ONE boundary.
        if c == '\r' && bytes.get(end) == Some(&b'\n') {
            end += 1;
            it.next();
        }
        start = end;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `str.isdigit()` — non-empty, and EVERY character is a python digit
/// ([`is_digit`]). An empty string is False, as in python.
#[must_use]
pub fn py_isdigit(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_digit)
}
