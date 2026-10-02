//! STEP-4 PINS (2026-10-01) — the primitive python-format helpers that live in
//! more than one place today (the kernel's `scalar.rs` and `lifecycle.rs`, and
//! `fantastic-bundle`'s `text.rs` / `int.rs`). A reduction that gives them ONE
//! home must keep every vector here.
//!
//! Every expected value is CPython 3.11's (`str.strip`, `str.isspace`, `int()`,
//! the kernel's `coerce_scalar` in `kernel/modes/parse.py`), measured with the
//! repo venv on 2026-10-01. The two places the rust copies DIFFER from each
//! other are pinned explicitly: `fantastic_kernel::coerce_scalar` keeps an int
//! wider than i64 digit for digit, while `fantastic_bundle::py_int` saturates.

use fantastic_bundle::PyError;
use serde_json::json;

/// `str.isspace()` is True, so `strip()` takes it.
const PY_SPACE: [char; 15] = [
    '\t', '\n', '\u{0b}', '\u{0c}', '\r', ' ', '\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}', '\u{85}',
    '\u{a0}', '\u{1680}', '\u{2028}', '\u{3000}',
];

/// `str.isspace()` is False — `strip()` leaves it.
const PY_NOT_SPACE: [char; 4] = ['\u{180e}', '\u{200b}', '\u{feff}', '\u{1}'];

#[test]
fn both_py_strips_take_exactly_cpythons_whitespace() {
    for c in PY_SPACE {
        let s = format!("{c}{c}x y{c}");
        assert_eq!(
            fantastic_kernel::py_strip(&s),
            "x y",
            "kernel U+{:04X}",
            c as u32
        );
        assert_eq!(
            fantastic_bundle::py_strip(&s),
            "x y",
            "bundle U+{:04X}",
            c as u32
        );
    }
    for c in PY_NOT_SPACE {
        let s = format!("{c}x{c}");
        assert_eq!(
            fantastic_kernel::py_strip(&s),
            s,
            "kernel U+{:04X}",
            c as u32
        );
        assert_eq!(
            fantastic_bundle::py_strip(&s),
            s,
            "bundle U+{:04X}",
            c as u32
        );
    }
    // the rest of the U+2000..U+200A run and the two narrow spaces
    for cp in (0x2000u32..=0x200A).chain([0x202F, 0x205F, 0x2029]) {
        let c = char::from_u32(cp).unwrap();
        let s = format!("{c}x{c}");
        assert_eq!(fantastic_kernel::py_strip(&s), "x", "kernel U+{cp:04X}");
        assert_eq!(fantastic_bundle::py_strip(&s), "x", "bundle U+{cp:04X}");
    }
}

/// `coerce_scalar(v)` → python's `json.dumps(coerce_scalar(v))`.
#[test]
fn coerce_scalar_matches_python_digit_for_digit() {
    for (input, want) in [
        ("1_000", "1000"),
        ("١٢", "12"),
        ("1١", "11"),
        ("٣.٥", "3.5"),
        ("99999999999999999999", "99999999999999999999"),
        ("-99999999999999999999", "-99999999999999999999"),
        ("9223372036854775807", "9223372036854775807"),
        ("9223372036854775808", "9223372036854775808"),
        ("-9223372036854775809", "-9223372036854775809"),
        (" 7 ", "7"),
        ("\u{a0}7\u{3000}", "7"),
        ("+0", "0"),
        ("-0", "0"),
        ("007", "7"),
        ("1e3", "1000.0"),
        ("1E-2", "0.01"),
        ("Infinity", "Infinity"),
        ("-inf", "-Infinity"),
        ("nan", "NaN"),
        ("1_0.5", "10.5"),
        (".5", "0.5"),
        ("5.", "5.0"),
        ("True", "true"),
        ("tRuE", "true"),
        ("fAlSe", "false"),
        ("\u{85}7", "7"),
        ("\u{0b}7", "7"),
        ("\u{0c}7.5", "7.5"),
    ] {
        let got = fantastic_kernel::coerce_scalar(input);
        assert_eq!(serde_json::to_string(&got).unwrap(), want, "{input:?}");
    }
    // the shapes python's int() AND float() refuse stay the text as written —
    // `\x1c`-`\x1f` included: `str.isspace()` says yes, but CPython's
    // `int()`/`float()` strip C `isspace` only (an ASCII char is never
    // transformed), so `"\u{1c}7"` is text, not 7.0.
    for input in [
        "0x10",
        "²",
        "  ",
        "",
        "1__0",
        "_1",
        "1_",
        "\u{1c}7",
        "7\u{1f}",
        "\u{1c}7\u{a0}",
        "\u{1e}7.5",
        "\u{1d}",
    ] {
        assert_eq!(
            fantastic_kernel::coerce_scalar(input),
            json!(input),
            "{input:?}"
        );
    }
}

#[test]
fn py_int_saturates_where_coerce_scalar_keeps_every_digit() {
    use fantastic_bundle::py_int;
    assert_eq!(py_int(&json!("99999999999999999999")), Ok(i64::MAX));
    assert_eq!(py_int(&json!("-99999999999999999999")), Ok(i64::MIN));
    assert_eq!(py_int(&json!("9223372036854775808")), Ok(i64::MAX));
    assert_eq!(py_int(&json!("١٢")), Ok(12));
    assert_eq!(py_int(&json!("1١")), Ok(11));
    assert_eq!(py_int(&json!("1_0")), Ok(10));
    assert_eq!(py_int(&json!(" 7 ")), Ok(7));
    assert_eq!(py_int(&json!("\u{a0}7\u{3000}")), Ok(7));
    assert_eq!(
        py_int(&json!("\u{1c}7")),
        Err(PyError::ValueError(
            "invalid literal for int() with base 10: '\\x1c7'".into()
        ))
    );
    assert_eq!(
        py_int(&json!("²")),
        Err(PyError::ValueError(
            "invalid literal for int() with base 10: '²'".into()
        ))
    );
    assert_eq!(
        py_int(&json!("1__0")),
        Err(PyError::ValueError(
            "invalid literal for int() with base 10: '1__0'".into()
        ))
    );
    // the kernel's copy, on the same wide literal, is an exact number
    let wide = fantastic_kernel::coerce_scalar("99999999999999999999");
    assert!(wide.is_number() && wide.as_i64().is_none());
    assert_eq!(wide.to_string(), "99999999999999999999");
}

fn lower_hex(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// python `uuid.uuid4().hex` — 32 lowercase hex, version nibble `4`, RFC-4122
/// variant (`8`/`9`/`a`/`b`) — and different every time.
#[test]
fn the_kernel_guid_is_a_uuid4_hex() {
    let ids: Vec<String> = (0..256)
        .map(|_| fantastic_kernel::lifecycle::mint_guid())
        .collect();
    for g in &ids {
        assert_eq!(g.len(), 32, "{g}");
        assert!(lower_hex(g), "{g}");
        assert_eq!(&g[12..13], "4", "uuid4 version nibble: {g}");
        assert!(
            matches!(&g[16..17], "8" | "9" | "a" | "b"),
            "RFC-4122 variant: {g}"
        );
    }
    let uniq: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(uniq.len(), ids.len());
    // every OTHER nibble varies (the version/variant are the only fixed bits)
    let varied = (0..32)
        .filter(|&i| i != 12)
        .filter(|&i| {
            let first = &ids[0][i..=i];
            ids.iter().any(|g| &g[i..=i] != first)
        })
        .count();
    assert_eq!(varied, 31, "no nibble is stuck: {:?}", &ids[..4]);
}

/// python `f"{bundle}_{secrets.token_hex(3)}"` — 6 lowercase hex, random.
#[test]
fn a_minted_agent_id_is_bundle_and_token_hex_3() {
    let ids: Vec<String> = (0..256)
        .map(|_| fantastic_kernel::lifecycle::mint_id("pkg.web.tools"))
        .collect();
    for id in &ids {
        let tail = id.strip_prefix("web_").unwrap_or_else(|| panic!("{id}"));
        assert_eq!(tail.len(), 6, "{id}");
        assert!(lower_hex(tail), "{id}");
    }
    let uniq: std::collections::HashSet<&String> = ids.iter().collect();
    assert!(
        uniq.len() > 250,
        "token_hex(3) repeats too often: {}",
        uniq.len()
    );
}
