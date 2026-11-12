//! Every pin below was measured with CPython 3.11.

use super::*;
use serde_json::json;

#[test]
fn py_repr_matches_cpython() {
    for (input, want) in [
        ("it's", "\"it's\""),
        ("a\\b", "'a\\\\b'"),
        ("\t\r\n", "'\\t\\r\\n'"),
        ("both'\"", "'both\\'\"'"),
        ("\u{7f}", "'\\x7f'"),
        ("\u{85}", "'\\x85'"),
        ("\u{a0}", "'\\xa0'"),
        ("\u{200b}", "'\\u200b'"),
        ("é", "'é'"),
        ("\u{1F600}", "'\u{1F600}'"),
        ("\u{0}", "'\\x00'"),
        ("\u{e0001}", "'\\U000e0001'"),
        ("\u{10ffff}", "'\\U0010ffff'"),
        ("\u{378}", "'\\u0378'"),
        ("nope", "'nope'"),
        ("", "''"),
    ] {
        assert_eq!(py_repr(input), want, "repr({input:?})");
    }
}

#[test]
fn py_float_repr_matches_cpython() {
    for (f, want) in [
        (1.0, "1.0"),
        (1e16, "1e+16"),
        (1e-05, "1e-05"),
        (1e15, "1000000000000000.0"),
        (0.0001, "0.0001"),
        (1.5e16, "1.5e+16"),
        (123456789012345678.0, "1.2345678901234568e+17"),
        (0.1, "0.1"),
        (-0.0, "-0.0"),
        (1e100, "1e+100"),
        (2.5e-10, "2.5e-10"),
        (9999999999999998.0, "9999999999999998.0"),
        (1e22, "1e+22"),
        (5e-324, "5e-324"),
        (1.7976931348623157e308, "1.7976931348623157e+308"),
        (-7.25, "-7.25"),
        (123.456, "123.456"),
    ] {
        assert_eq!(py_float_repr(f), want, "repr({f})");
    }
}

#[test]
fn py_json_dumps_matches_cpython() {
    assert_eq!(
        py_json_dumps(&json!({"a": 1, "b": "\u{2014}"}), true, None),
        "{\"a\": 1, \"b\": \"\\u2014\"}"
    );
    assert_eq!(
        py_json_dumps(&json!("\u{1F600}"), true, None),
        "\"\\ud83d\\ude00\""
    );
    assert_eq!(py_json_dumps(&json!("\u{7f}"), true, None), "\"\\u007f\"");
    assert_eq!(py_json_dumps(&json!("\u{7f}"), false, None), "\"\u{7f}\"");
    assert_eq!(py_json_dumps(&json!(1e-05), true, None), "1e-05");
    assert_eq!(
        py_json_dumps(&json!({"k": null}), true, None),
        "{\"k\": null}"
    );
    assert_eq!(
        py_json_dumps(&json!("\u{0}\u{8}\u{c}\n\r\t\"\\/\u{1f}"), true, None),
        "\"\\u0000\\b\\f\\n\\r\\t\\\"\\\\/\\u001f\""
    );
    assert_eq!(
        py_json_dumps(&json!({"a": [1, {"b": []}, {}], "c": "é"}), true, Some(2)),
        "{\n  \"a\": [\n    1,\n    {\n      \"b\": []\n    },\n    {}\n  ],\n  \"c\": \"\\u00e9\"\n}"
    );
    assert_eq!(py_json_dumps(&json!([]), true, Some(2)), "[]");
    assert_eq!(py_json_dumps(&json!({}), true, Some(2)), "{}");
    assert_eq!(py_json_dumps(&json!([[]]), true, Some(2)), "[\n  []\n]");
    assert_eq!(
        py_json_dumps(&json!({"c": "é"}), false, None),
        "{\"c\": \"é\"}"
    );
}

#[test]
fn os_error_str_matches_cpython() {
    assert_eq!(
        os_error_str(2, Some("/no/such/x")),
        "[Errno 2] No such file or directory: '/no/such/x'"
    );
    assert_eq!(os_error_str(3, None), "[Errno 3] No such process");
    assert_eq!(
        os_error_str2(2, "/x/nope", "/x/b"),
        "[Errno 2] No such file or directory: '/x/nope' -> '/x/b'"
    );
    #[cfg(target_os = "macos")]
    assert_eq!(
        os_error_str(66, Some("/abs")),
        "[Errno 66] Directory not empty: '/abs'"
    );
    // The OS's own code for "not a directory": ENOTDIR on unix, ERROR_DIRECTORY
    // on Windows — both read as python's errno 20.
    let raw = if cfg!(windows) { 267 } else { 20 };
    let e = std::io::Error::from_raw_os_error(raw);
    assert_eq!(
        io_error_str(&e, std::path::Path::new("/f/x")),
        "[Errno 20] Not a directory: '/f/x'"
    );
}

#[test]
fn utf8_decode_error_str_matches_cpython() {
    for (bytes, want) in [
        (
            &b"\xff"[..],
            "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte",
        ),
        (
            b"a\xc3",
            "'utf-8' codec can't decode byte 0xc3 in position 1: unexpected end of data",
        ),
        (
            b"\xe4\xbd",
            "'utf-8' codec can't decode bytes in position 0-1: unexpected end of data",
        ),
        (
            b"\xe4\xbda",
            "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte",
        ),
        (
            b"\xc3a",
            "'utf-8' codec can't decode byte 0xc3 in position 0: invalid continuation byte",
        ),
        (
            b"\xe0\x80\x80",
            "'utf-8' codec can't decode byte 0xe0 in position 0: invalid continuation byte",
        ),
        (
            b"\xf0\x9f\x98",
            "'utf-8' codec can't decode bytes in position 0-2: unexpected end of data",
        ),
        (
            b"ok\x80",
            "'utf-8' codec can't decode byte 0x80 in position 2: invalid start byte",
        ),
        (
            b"\xf0\x9f\x98a",
            "'utf-8' codec can't decode bytes in position 0-2: invalid continuation byte",
        ),
        (
            b"\xc0\xaf",
            "'utf-8' codec can't decode byte 0xc0 in position 0: invalid start byte",
        ),
    ] {
        let err = std::str::from_utf8(bytes).unwrap_err();
        assert_eq!(utf8_decode_error_str(bytes, &err), want, "{bytes:?}");
    }
}

#[test]
fn unicode_predicates_match_cpython() {
    assert!(is_printable(' ') && is_printable('é') && !is_printable('\u{a0}'));
    assert!(is_space('\u{1c}') && is_space('\u{3000}') && !is_space('\u{200b}'));
    assert_eq!(decimal_value('\u{663}'), Some(3));
    assert_eq!(decimal_value('9'), Some(9));
    assert_eq!(decimal_value('\u{b2}'), None);
}

/// `os_error_class` names the `OSError` subclass CPython raises, which is what
/// a kernel's raised reply prints before the message.
#[test]
fn os_error_class_names_cpythons_subclass() {
    assert_eq!(os_error_class(libc::ENOENT), "FileNotFoundError");
    assert_eq!(os_error_class(libc::EACCES), "PermissionError");
    assert_eq!(os_error_class(libc::EPERM), "PermissionError");
    assert_eq!(os_error_class(libc::EAGAIN), "BlockingIOError");
    assert_eq!(os_error_class(libc::ECHILD), "ChildProcessError");
    assert_eq!(os_error_class(libc::ENOTDIR), "NotADirectoryError");
    assert_eq!(os_error_class(libc::EINVAL), "OSError");
}

// ── text splitters (CPython 3.11 vectors) ───────────────────────────

#[test]
fn py_splitlines_matches_cpython() {
    // The boundary set is python's, not rust's `lines()`: `\r`, `\v`, `\f` and the
    // information separators `\x1c`–`\x1e` all split, `\r\n` is ONE boundary, and
    // `\x1f` is whitespace but NOT a boundary.
    assert_eq!(
        py_splitlines("a\rneedle here\nb"),
        ["a", "needle here", "b"]
    );
    assert_eq!(py_splitlines("x\u{1c}needle"), ["x", "needle"]);
    assert_eq!(py_splitlines("a\r\nb"), ["a", "b"]);
    assert_eq!(py_splitlines("a\u{1f}b"), ["a\u{1f}b"]);
    assert_eq!(
        py_splitlines("a\u{b}b\u{c}c\u{85}d\u{2028}e"),
        ["a", "b", "c", "d", "e"]
    );
    // A trailing boundary yields no final empty line, and "" splits to nothing.
    assert_eq!(py_splitlines("a\n"), ["a"]);
    assert!(py_splitlines("").is_empty());
}

#[test]
fn py_strip_and_split_use_pythons_whitespace() {
    assert_eq!(py_strip(" \u{1f} a \u{1c} "), "a");
    assert_eq!(py_split_whitespace("a\u{1f}b\u{1c}c"), ["a", "b", "c"]);
    assert_eq!(py_split_whitespace("  a   b  "), ["a", "b"]);
    assert!(py_split_whitespace("  \u{1d} ").is_empty());
}

#[test]
fn py_set_order_matches_cpython() {
    // A python vector: `list(set([1116,1065,1066,1080]))`.
    assert_eq!(
        py_set_order(&[1116, 1065, 1066, 1080]),
        vec![1080, 1065, 1066, 1116]
    );
    // Duplicates drop keeping the first insert's slot.
    assert_eq!(py_set_order(&[3, 3, 1, 2, 1]), vec![1, 2, 3]);
    // A resize past 8 slots re-inserts the old table in slot order:
    // `list(set(range(1, 12)))` is 1..=11 (small ints land in their own slots).
    let many: Vec<i64> = (1..=11).collect();
    assert_eq!(py_set_order(&many), many);
    // 0 is a key like any other.
    assert_eq!(py_set_order(&[8, 0]), vec![8, 0]);
    assert!(py_set_order(&[]).is_empty());
}

#[test]
fn py_isdigit_matches_cpython() {
    // `str.isdigit()` — the class `guard_agent_id` refuses an id by, so an
    // all-digit name cannot read as a message id in prompt text.
    assert!(py_isdigit("123"));
    assert!(py_isdigit("0"));
    // Nd beyond ASCII: ARABIC-INDIC and DEVANAGARI digits are digits.
    assert!(py_isdigit("\u{0663}"));
    assert!(py_isdigit("\u{0966}\u{0967}"));
    // Numeric_Type=Digit OUTSIDE Nd — python says yes, `is_ascii_digit` says no.
    assert!(py_isdigit("\u{00b2}")); // SUPERSCRIPT TWO
    assert!(py_isdigit("\u{2460}")); // CIRCLED DIGIT ONE
    assert!(py_isdigit("\u{2081}")); // SUBSCRIPT ONE
                                     // Numeric_Type=Numeric, NOT Digit — python says no, rust's `is_numeric` yes.
    assert!(!py_isdigit("\u{00bd}")); // VULGAR FRACTION ONE HALF
    assert!(!py_isdigit("\u{2167}")); // ROMAN NUMERAL EIGHT
                                      // Mixed and empty.
    assert!(!py_isdigit("1a"));
    assert!(!py_isdigit("1 2"));
    assert!(!py_isdigit(""));
}

#[test]
fn a_non_finite_and_a_wide_int_keep_their_python_spelling() {
    // An arbitrary-precision number keeps the TEXT it was built from,
    // and `as_f64()` answers `None` for exactly the two shapes an `f64` cannot
    // hold. Routing them through `unwrap_or(0.0)` would print `0.0` for
    // `Infinity` and `1.2345678901234568e+29` for a 30-digit int, where python's
    // `kernel create_agent … big2=… inf=inf nan=nan` keeps the text.
    let raw = |s: &str| {
        serde_json::Value::Number(serde_json::Number::from_string_unchecked(s.to_string()))
    };
    let wide = "123456789012345678901234567890";
    for (v, dumps, repr) in [
        (raw("Infinity"), "Infinity", "inf"),
        (raw("-Infinity"), "-Infinity", "-inf"),
        (raw("NaN"), "NaN", "nan"),
        (raw(wide), wide, wide),
    ] {
        // `json.dumps(..., allow_nan=True)` — the kernel's default.
        assert_eq!(py_json_dumps(&v, true, None), dumps, "dumps {v}");
        // `repr()` spells a non-finite float differently.
        assert_eq!(py_repr_value(&v), repr, "repr {v}");
    }
    assert_eq!(
        py_json_dumps(&json!({"inf": f64::INFINITY}), true, None),
        "{\"inf\": null}",
        "an f64 infinity never reaches Number::from_f64 — it is null there"
    );
}

/// python `io_bridge._codec.set_path` over a `_binary_path` LIST OF STEPS: a
/// string step is an object key taken literally, an int step a list index; the
/// last object key is set in place or appended; the typed steps come back.
#[test]
fn set_path_places_at_literal_keys_and_list_indices() {
    use path::{set_path, Step};
    let mut env = json!({"type": "call", "payload": {"a": {"b": "kept"}, "a.b": null}});
    let got = set_path(&mut env, &json!(["payload", "a.b"]), json!("BODY")).unwrap();
    assert_eq!(got, vec![Step::from("payload"), Step::from("a.b")]);
    assert_eq!(
        env["payload"],
        json!({"a": {"b": "kept"}, "a.b": "BODY"}),
        "the literal key, never the nested twin"
    );
    let mut env = json!({"data": {"items": [{"n": 1}, {"data": null}]}});
    let got = set_path(&mut env, &json!(["data", "items", 1, "data"]), json!(2)).unwrap();
    assert_eq!(got[2], Step::Index(1));
    assert_eq!(env["data"]["items"][1]["data"], 2);
    // a key that looks like an index is a key; empty and dotted keys are keys
    let mut env = json!({"payload": {}});
    set_path(&mut env, &json!(["payload", "0"]), json!(4)).unwrap();
    set_path(&mut env, &json!(["payload", ""]), json!(5)).unwrap();
    set_path(&mut env, &json!(["payload", "a..b."]), json!({})).unwrap();
    set_path(&mut env, &json!(["payload", "a..b.", "c."]), json!(6)).unwrap();
    assert_eq!(
        serde_json::to_string(&env).unwrap(),
        r#"{"payload":{"0":4,"":5,"a..b.":{"c.":6}}}"#
    );
    // the last object key keeps its place when present, is appended when absent
    let mut env = json!({"a": 1, "b": 2});
    set_path(&mut env, &json!(["a"]), json!(null)).unwrap();
    set_path(&mut env, &json!(["c"]), json!(3)).unwrap();
    assert_eq!(
        serde_json::to_string(&env).unwrap(),
        r#"{"a":null,"b":2,"c":3}"#
    );
}

/// Every malformed `_binary_path` is ONE refusal, python's `ValueError` text
/// word for word (`test_binary_path_steps.py`).
#[test]
fn set_path_refuses_a_malformed_path_with_python_s_text() {
    let cases: [(serde_json::Value, &str); 13] = [
        (json!("payload.bytes"), "not a list of steps"),
        (json!([]), "an empty path names no slot"),
        (json!({"payload": "bytes"}), "not a list of steps"),
        (json!([0]), "step 0 at an object is not a string key"),
        (
            json!(["items", "0"]),
            "step '0' at a list is not a non-negative integer index",
        ),
        (
            json!(["items", -1]),
            "step -1 at a list is not a non-negative integer index",
        ),
        (
            json!(["items", true]),
            "step True at a list is not a non-negative integer index",
        ),
        (
            serde_json::from_str(r#"["items", 0.0]"#).unwrap(),
            "step 0.0 at a list is not a non-negative integer index",
        ),
        (
            json!(["items", 2]),
            "index 2 is past the end of a list of 2",
        ),
        (json!(["nope", "x"]), "no key 'nope'"),
        (
            json!(["type", "x"]),
            "step 'x' goes into a string, not a container",
        ),
        (
            json!(["items", 0, "x"]),
            "step 'x' goes into a null, not a container",
        ),
        (
            json!(["n", "x"]),
            "step 'x' goes into a number, not a container",
        ),
    ];
    for (path, why) in cases {
        let mut head = json!({"type": "reply", "items": [null, null], "n": 1});
        assert_eq!(
            path::set_path(&mut head, &path, json!(null)).unwrap_err(),
            why,
            "{path}"
        );
    }
}

/// `take`: an object key is removed (order of the rest kept), a list slot
/// nulled, an unresolvable path refused; `get` follows the same steps.
#[test]
fn take_removes_a_key_and_nulls_a_slot() {
    use path::{get, take, Step};
    let mut env = json!({"a": 1, "data": null, "b": {"c": [1, 2]}, "x.y": 3});
    take(&mut env, &[Step::from("data")]).unwrap();
    take(
        &mut env,
        &[Step::from("b"), Step::from("c"), Step::Index(0)],
    )
    .unwrap();
    assert_eq!(get(&env, &[Step::from("x.y")]), Some(&json!(3)));
    take(&mut env, &[Step::from("x.y")]).unwrap();
    assert_eq!(
        serde_json::to_string(&env).unwrap(),
        r#"{"a":1,"b":{"c":[null,2]}}"#
    );
    assert!(take(&mut env, &[Step::from("nope")]).is_err());
    assert!(take(&mut env, &[Step::from("a"), Step::from("b")]).is_err());
    assert!(take(&mut env, &[]).is_err());
}
