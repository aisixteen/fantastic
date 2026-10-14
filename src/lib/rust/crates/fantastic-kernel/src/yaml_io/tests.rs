//! The on-disk YAML format — twins of python's `kernel_state/tests/test_yaml_format.py`,
//! pinned BYTE FOR BYTE by the python-written fixture `fixtures/agent.yaml` (a
//! record): the same input built here must dump to the fixture bytes, and load →
//! dump must be the identity on the file. There is no message tape any more.

use super::*;
use serde_json::json;

fn fixture(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../../python/bundled_agents/loader/kernel_state/tests/fixtures/{name}"
    ));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn record() -> Value {
    let pairs: Vec<(&str, Value)> = vec![
        ("id", json!("tricky_0a1b2c")),
        ("handler_module", json!("file_bridge.tools")),
        ("parent_id", json!("kernel_state")),
        ("one", json!("1")),
        ("truthy", json!("true")),
        ("nully", json!("null")),
        ("tilde", json!("~")),
        ("empty", json!("")),
        ("yes", json!("yes")),
        ("on", json!("on")),
        ("octal12", json!("0o17")),
        ("exp", json!("1e3")),
        ("exp_neg", json!("2E-4")),
        ("dot_end", json!("5.")),
        ("version", json!("1.2.3")),
        ("date", json!("2026-09-25")),
        ("colon", json!("a: b")),
        ("hash", json!("# not a comment")),
        ("lead", json!(" leading space")),
        ("star", json!("*")),
        ("unicode", json!("héllo — 世界")),
        ("emoji", json!("ok 😀")),
        ("int", json!(42)),
        ("neg", json!(-7)),
        ("float", json!(1.5)),
        ("bool", json!(true)),
        ("none", json!(null)),
        ("multiline", json!("line one\nline two\n")),
        ("no_final_nl", json!("a\n  indented b")),
        ("trailing_blank", json!("keep\n\n")),
        ("space_before_break", json!("trail \nx")),
        ("tab", json!("a\tb")),
        ("long", json!("word ".repeat(30))),
        (
            "can_call",
            json!({"$self": "*", "$parent": "*", "kernel_store": ["read", "write"]}),
        ),
        ("callable_by", json!({"$parent": "*", "$subtree": "*"})),
        (
            "nested",
            json!({"list": [1, "two", {"three": []}], "map": {}, "deep": {"x": [[]]}}),
        ),
    ];
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

#[test]
fn record_matches_the_fixture_bytes() {
    assert_eq!(dump(&record()), fixture("agent.yaml"));
}

#[test]
fn load_then_dump_is_the_identity_on_the_record_fixture() {
    let text = fixture("agent.yaml");
    assert_eq!(dump(&load(&text).unwrap()), text);
}

#[test]
fn a_record_round_trips_type_stable_key_ordered_and_byte_stable() {
    let first = dump(&record());
    let back = load(&first).unwrap();
    assert_eq!(back, record());
    let keys: Vec<&String> = back.as_object().unwrap().keys().collect();
    let want: Vec<String> = record().as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        want.iter().collect::<Vec<_>>(),
        "key order kept, no sort"
    );
    for (k, v) in record().as_object().unwrap() {
        let b = &back[k];
        assert_eq!(
            std::mem::discriminant(b),
            std::mem::discriminant(v),
            "{k}: type changed"
        );
        if let (Value::Number(x), Value::Number(y)) = (b, v) {
            assert_eq!(x.is_f64(), y.is_f64(), "{k}: int/float changed");
        }
    }
    assert_eq!(dump(&back), first);
    assert!(
        !first.lines().any(|l| l == "..."),
        "never a document-end marker"
    );
}

#[test]
fn a_plain_string_a_rust_float_parse_would_take_stays_a_string() {
    for s in [
        "inf", "nan", "infinity", "NaN", "08", "+1", "0x1F", "1_000", "yes", "<<",
    ] {
        let v = json!({ "k": s });
        let back = load(&dump(&v)).unwrap();
        assert_eq!(back, v, "{s}: {}", dump(&v));
    }
}

#[test]
fn scalar_styles_match_libyaml() {
    let cases = [
        (json!({"a": " a\nb"}), "a: |2-\n   a\n  b\n"),
        (json!({"a": "\n"}), "a: \"\\n\"\n"),
        (json!({"a": "it's"}), "a: it's\n"),
        (json!({"a": "'q"}), "a: '''q'\n"),
        (json!({"a": "- x"}), "a: '- x'\n"),
        (json!({"a": "a #b"}), "a: 'a #b'\n"),
        (json!({"a": "a#b"}), "a: a#b\n"),
        (json!({"a": 1e16}), "a: 1.0e+16\n"),
        (json!({"a": 1e-5}), "a: 1.0e-05\n"),
        (
            json!({"a": [{"b": 1, "c": [1, [2, 3]]}]}),
            "a:\n- b: 1\n  c:\n  - 1\n  - - 2\n    - 3\n",
        ),
        (json!([{"id": "x", "n": 1}, "y"]), "- id: x\n  n: 1\n- y\n"),
        (json!({}), "{}\n"),
        (json!({"pid": 123}), "pid: 123\n"),
        (
            json!({"k".repeat(130): "v"}),
            &format!("? {}\n: v\n", "k".repeat(130)),
        ),
        (json!({"a\nb": 1}), "? |-\n  a\n  b\n: 1\n"),
        (json!({"k": "x\u{85}y"}), "k: \"x\\Ny\"\n"),
        (json!({"k": "a\u{a0}b\u{feff}"}), "k: \"a\u{a0}b\\uFEFF\"\n"),
        (json!({"k": "a:"}), "k: 'a:'\n"),
        (json!({"k": "?x"}), "k: ?x\n"),
        (json!({"k": "a  \n b"}), "k: \"a  \\n b\"\n"),
    ];
    // Every expected string is python's `yaml_io.dump` output.
    // U+2028 (a YAML 1.1 break) is written raw by libyaml, as here; a 1.2 reader
    // does not treat it as a break, so that one is pinned write-side only.
    assert_eq!(dump(&json!({"a": "x\u{2028}y"})), "a: 'x\u{2028}  y'\n");
    for (v, want) in cases {
        assert_eq!(dump(&v), want, "{v}");
        assert_eq!(load(want).unwrap(), v, "{want}");
    }
}

// The READ side's python vectors (YAML 1.1, tags, aliases, merges, keys) live
// in `read_tests.rs`; this file pins what the WRITER makes and that it reads
// back unchanged.

#[test]
fn a_broken_document_names_where_it_broke() {
    let e = load("a: [1, 2\n").unwrap_err();
    assert!(
        e.ends_with("at line 2, column 1"),
        "a scan error carries its line/column: {e}"
    );
    let e = load("a: b\n  c: d\n").unwrap_err();
    assert!(e.contains(" at line "), "{e}");
}

#[test]
fn load_all_reads_every_document_of_a_stream_in_order() {
    // the scheduler's `schedule_events.yaml` — one `---` document per event
    let text = format!(
        "{SEPARATOR}{}{SEPARATOR}{}{SEPARATOR}{}",
        dump(&json!({"id": "e1", "n": 1})),
        dump(&json!({"id": "e2", "tags": ["x", "y"]})),
        dump(&json!({"id": "e3", "nested": {"k": null}})),
    );
    assert_eq!(
        load_all(&text).unwrap(),
        vec![
            json!({"id": "e1", "n": 1}),
            json!({"id": "e2", "tags": ["x", "y"]}),
            json!({"id": "e3", "nested": {"k": null}}),
        ]
    );
    assert_eq!(
        load_all("").unwrap(),
        Vec::<Value>::new(),
        "an empty stream"
    );
    assert_eq!(load("").unwrap(), Value::Null, "load of nothing is null");
    assert_eq!(
        load_all("a: 1\n").unwrap(),
        vec![json!({"a": 1})],
        "no marker"
    );
    // one bad document fails the whole stream
    let bad = format!("{SEPARATOR}a: 1\n{SEPARATOR}b: !!int x\n");
    assert_eq!(
        load_all(&bad).unwrap_err(),
        "invalid literal for int() with base 10: 'x'"
    );
    // a root scalar document
    assert_eq!(
        load_all("--- 5\n--- x\n").unwrap(),
        vec![json!(5), json!("x")]
    );
}
