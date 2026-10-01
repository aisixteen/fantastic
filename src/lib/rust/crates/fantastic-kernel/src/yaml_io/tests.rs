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
        ("callable_by", json!({"$parent": "*", "$children": "*"})),
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
    // Every expected string is python's `yaml_io.dump` output (measured).
    // U+2028 (a YAML 1.1 break) is written raw by libyaml, as here; a 1.2 reader
    // does not treat it as a break, so that one is pinned write-side only.
    assert_eq!(dump(&json!({"a": "x\u{2028}y"})), "a: 'x\u{2028}  y'\n");
    for (v, want) in cases {
        assert_eq!(dump(&v), want, "{v}");
        assert_eq!(load(want).unwrap(), v, "{want}");
    }
}

// ─── STEP-4 PINS (2026-10-01): the READ side, as it reads today ─────────────
//
// `load`/`load_all` resolve plain scalars by YAML 1.2 CORE (see the module doc),
// where python's `yaml_io.load` (`CSafeLoader`) reads YAML 1.1. Machine-written
// files never differ (the writer quotes against both), but a HAND-EDITED record
// does — so every vector where 1.1 and 1.2 disagree is pinned here as rust reads
// it today, with python's 1.1 reading noted beside it. A reader swap (the YAML
// reduction candidate) changes these deliberately or not at all.

/// `k: <plain>` → what `load` makes of `<plain>`.
fn plain(s: &str) -> Result<Value, String> {
    load(&format!("k: {s}\n")).map(|v| v["k"].clone())
}

#[test]
fn plain_scalars_resolve_by_yaml_1_2_core_where_1_1_disagrees() {
    let cases: Vec<(&str, Value)> = vec![
        // (rust reads, python 1.1 reads)
        ("yes", json!("yes")),               // True
        ("no", json!("no")),                 // False
        ("on", json!("on")),                 // True
        ("off", json!("off")),               // False
        ("Yes", json!("Yes")),               // True
        ("OFF", json!("OFF")),               // False
        ("1:30", json!("1:30")),             // 90 (sexagesimal)
        ("0o17", json!(15)),                 // '0o17'
        ("1e3", json!(1000.0)),              // '1e3'
        ("1e-3", json!(0.001)),              // '1e-3'
        ("<<", json!("<<")),                 // a merge key: ConstructorError as a value
        ("1_000", json!("1_000")),           // 1000
        ("0b101", json!("0b101")),           // 5
        ("1:30.5", json!("1:30.5")),         // 90.5
        ("2001-12-14", json!("2001-12-14")), // datetime.date
    ];
    for (s, want) in cases {
        let got = plain(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        assert_eq!(got, want, "{s}");
        if let (Value::Number(g), Value::Number(w)) = (&got, &want) {
            assert_eq!(g.is_f64(), w.is_f64(), "{s}: int/float kind");
        }
    }
}

#[test]
fn plain_scalars_where_both_schemas_agree() {
    let cases: Vec<(&str, Value)> = vec![
        ("~", json!(null)),
        ("null", json!(null)),
        ("Null", json!(null)),
        ("NULL", json!(null)),
        ("true", json!(true)),
        ("TRUE", json!(true)),
        ("False", json!(false)),
        ("0x1F", json!(31)),
        ("+1", json!(1)),
        ("42", json!(42)),
        ("-7", json!(-7)),
        (".5", json!(0.5)),
        ("5.", json!(5.0)),
        ("1.0e+3", json!(1000.0)),
        ("1.5", json!(1.5)),
        ("0o", json!("0o")),
        ("0x", json!("0x")),
        ("inf", json!("inf")),
        ("nan", json!("nan")),
        ("hello world", json!("hello world")),
    ];
    for (s, want) in cases {
        let got = plain(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        assert_eq!(got, want, "{s}");
        if let (Value::Number(g), Value::Number(w)) = (&got, &want) {
            assert_eq!(g.is_f64(), w.is_f64(), "{s}: int/float kind");
        }
    }
    // an int wider than i64 keeps every digit (arbitrary precision, never f64)
    let big = plain("123456789012345678901234567890").unwrap();
    assert_eq!(big.to_string(), "123456789012345678901234567890");
    // an empty value is null, as in both schemas
    assert_eq!(load("k:\n").unwrap(), json!({"k": null}));
}

#[test]
fn a_non_finite_float_has_no_json_form_and_fails_the_load_naming_it() {
    for s in [".inf", ".Inf", "-.inf", "+.INF", ".nan", ".NaN"] {
        assert_eq!(
            plain(s).unwrap_err(),
            format!("the float {s} has no JSON form — a record or row cannot hold it"),
            "{s}"
        );
    }
}

#[test]
fn shapes_outside_the_format_are_refused_with_their_names() {
    for (doc, want) in [
        (
            "a: !!str 1\n",
            "a tagged node — tags are not part of this format",
        ),
        (
            "a: !foo 1\n",
            "a tagged node — tags are not part of this format",
        ),
        (
            "a: !!seq [1]\n",
            "a tagged node — tags are not part of this format",
        ),
        (
            "a: !!map {b: 1}\n",
            "a tagged node — tags are not part of this format",
        ),
        (
            "a: &x 1\nb: *x\n",
            "an alias — aliases are not part of this format",
        ),
        ("? [a]\n: 1\n", "a non-scalar map key"),
        ("? {a: 1}\n: 1\n", "a non-scalar map key"),
        (
            "---\na: 1\n---\nb: 2\n",
            "expected a single document in the stream",
        ),
    ] {
        assert_eq!(load(doc).unwrap_err(), want, "{doc:?}");
    }
}

#[test]
fn a_broken_document_names_where_it_broke() {
    let e = load("a: [1, 2\n").unwrap_err();
    assert!(
        e.ends_with("at line 2 column 1"),
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
    let bad = format!("{SEPARATOR}a: 1\n{SEPARATOR}b: .inf\n");
    assert_eq!(
        load_all(&bad).unwrap_err(),
        "the float .inf has no JSON form — a record or row cannot hold it"
    );
    // a root scalar document
    assert_eq!(
        load_all("--- 5\n--- x\n").unwrap(),
        vec![json!(5), json!("x")]
    );
}
