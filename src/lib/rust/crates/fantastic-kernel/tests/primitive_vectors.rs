//! PINS — the primitive python-format helpers of `fantastic-bundle`'s `text.rs` /
//! `str.isspace`), every expected value CPython 3.11's (`str.strip`, `str.isspace`).

use serde_json::json;

/// `str.isspace()` is True, so `strip()` takes it.
const PY_SPACE: [char; 15] = [
    '\t', '\n', '\u{0b}', '\u{0c}', '\r', ' ', '\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}', '\u{85}',
    '\u{a0}', '\u{1680}', '\u{2028}', '\u{3000}',
];

/// `str.isspace()` is False — `strip()` leaves it.
const PY_NOT_SPACE: [char; 4] = ['\u{180e}', '\u{200b}', '\u{feff}', '\u{1}'];

#[test]
fn py_strip_takes_exactly_cpythons_whitespace() {
    for c in PY_SPACE {
        let s = format!("{c}{c}x y{c}");
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
        assert_eq!(fantastic_bundle::py_strip(&s), "x", "bundle U+{cp:04X}");
    }
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
