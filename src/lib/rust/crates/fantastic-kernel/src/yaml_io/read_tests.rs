//! The reader against PYTHON's own answers. Every expected value below is
//! python's `kernel_state.yaml_io.load` (`yaml.load(text, Loader=CSafeLoader)`)
//! on the same text — a date or a
//! datetime as its `str()`, a non-finite float as its repr, every other value as
//! the JSON python's own encoder writes.

use super::{load, load_all};
use serde_json::{json, Value};

/// `k: <text>` → what `load` makes of `<text>`.
fn k(text: &str) -> Value {
    load(&format!("k: {text}\n")).unwrap_or_else(|e| panic!("{text}: {e}"))["k"].clone()
}

fn k_err(text: &str) -> String {
    match load(&format!("k: {text}\n")) {
        Ok(v) => panic!("{text}: loaded {v}, python raises"),
        Err(e) => e,
    }
}

/// Same value AND same python type (an int is not a float).
fn same(text: &str, got: &Value, want: &Value) {
    assert_eq!(got, want, "{text}");
    if let (Value::Number(g), Value::Number(w)) = (got, want) {
        assert_eq!(g.is_f64(), w.is_f64(), "{text}: int/float kind");
    }
}

#[test]
fn plain_scalars_are_yaml_1_1_as_python_reads_them() {
    let cases: Vec<(&str, Value)> = vec![
        // bools — YAML 1.1's yes/no/on/off, any of python's three spellings
        ("yes", json!(true)),
        ("no", json!(false)),
        ("on", json!(true)),
        ("off", json!(false)),
        ("Yes", json!(true)),
        ("OFF", json!(false)),
        ("TRUE", json!(true)),
        ("y", json!("y")),
        ("n", json!("n")),
        // ints — octal by a leading 0, `_` dropped, binary, hex, sexagesimal
        ("0644", json!(420)),
        ("08", json!("08")),
        ("00", json!(0)),
        ("0_7", json!(7)),
        ("1:30", json!(90)),
        ("1:5", json!(65)),
        ("-1:30", json!(-90)),
        ("1_:30", json!(90)),
        ("12:30:45", json!(45045)),
        ("1:2:3:4", json!(223_384)),
        ("190:20:30", json!(685_230)),
        ("1:60", json!("1:60")),
        ("0:30", json!("0:30")),
        ("0o17", json!("0o17")),
        ("0O17", json!("0O17")),
        ("1_000", json!(1000)),
        ("1__0", json!(10)),
        ("0b101", json!(5)),
        ("0b_1", json!(1)),
        ("0B1", json!("0B1")),
        ("0x1F", json!(31)),
        ("-0x1f", json!(-31)),
        ("0x_1f", json!(31)),
        ("0X1F", json!("0X1F")),
        ("0b", json!("0b")),
        ("0x", json!("0x")),
        ("0o", json!("0o")),
        ("+12", json!(12)),
        ("-0", json!(0)),
        ("9223372036854775807", json!(9_223_372_036_854_775_807_i64)),
        // floats — 1.1 needs a dot (and a SIGNED exponent)
        ("1e3", json!("1e3")),
        ("1e-5", json!("1e-5")),
        ("1.0e5", json!("1.0e5")),
        ("1e400", json!("1e400")),
        ("1.0e400", json!("1.0e400")),
        ("1.5", json!(1.5)),
        ("1.", json!(1.0)),
        ("0.", json!(0.0)),
        (".5", json!(0.5)),
        ("1.e+5", json!(100_000.0)),
        ("6.8523015e+5", json!(685_230.15)),
        ("1.5e-5", json!(1.5e-5)),
        ("1.5e+3", json!(1500.0)),
        ("1_0.5", json!(10.5)),
        ("190:20:30.15", json!(685_230.15)),
        ("1:30.5", json!(90.5)),
        ("9:59.5", json!(599.5)),
        ("0:00.5", json!(0.5)),
        ("1.5:30", json!("1.5:30")),
        ("3.14159265358979323846", json!(std::f64::consts::PI)),
        // non-finite: python's float, JSON cannot hold it — its repr
        (".inf", json!("inf")),
        ("-.Inf", json!("-inf")),
        ("+.inf", json!("inf")),
        (".NaN", json!("nan")),
        ("1.0e+400", json!("inf")),
        ("-1.0e+400", json!("-inf")),
        ("-.nan", json!("-.nan")),
        (".Nan", json!(".Nan")),
        // null
        ("~", json!(null)),
        ("null", json!(null)),
        ("Null", json!(null)),
        ("", json!(null)),
        ("''", json!("")),
        // timestamps — python's `str()` of the date/datetime
        ("2001-12-14", json!("2001-12-14")),
        (
            "2001-12-14t21:59:43.10-05:00",
            json!("2001-12-14 21:59:43.100000-05:00"),
        ),
        (
            "2001-12-14 21:59:43.10 -5",
            json!("2001-12-14 21:59:43.100000-05:00"),
        ),
        (
            "2001-12-14 21:59:43.10 +05:30",
            json!("2001-12-14 21:59:43.100000+05:30"),
        ),
        (
            "2001-12-15T02:59:43.1Z",
            json!("2001-12-15 02:59:43.100000+00:00"),
        ),
        ("2001-12-14T21:59:43Z", json!("2001-12-14 21:59:43+00:00")),
        ("2002-12-14 01:02:03", json!("2002-12-14 01:02:03")),
        ("2001-1-4 1:02:03", json!("2001-01-04 01:02:03")),
        (
            "2001-12-14 21:59:43.1234567",
            json!("2001-12-14 21:59:43.123456"),
        ),
        ("2001-12-14 21:59:43.000000", json!("2001-12-14 21:59:43")),
        ("2001-12-14 01:02:03.", json!("2001-12-14 01:02:03")),
        ("2001-12-14 1:2:3", json!("2001-12-14 1:2:3")),
        ("'2001-12-14'", json!("2001-12-14")),
    ];
    for (text, want) in cases {
        same(text, &k(text), &want);
    }
}

#[test]
fn an_int_of_any_width_keeps_every_digit() {
    for (text, digits) in [
        ("12345678901234567890", "12345678901234567890"),
        ("-9223372036854775809", "-9223372036854775809"),
        (
            "123456789012345678901234567890",
            "123456789012345678901234567890",
        ),
        ("!!int 1000:99999999999999999999", "100000000000000059999"),
    ] {
        let v = k(text);
        assert!(v.is_number(), "{text}: {v}");
        assert_eq!(v.to_string(), digits, "{text}");
    }
}

#[test]
fn explicit_tags_pick_python_s_constructor() {
    let cases: Vec<(&str, Value)> = vec![
        ("!!str 123", json!("123")),
        ("!!str yes", json!("yes")),
        ("!!str", json!("")),
        ("!!int '42'", json!(42)),
        ("!!int 0o17", json!(15)),
        ("!!int -0b11", json!(-3)),
        ("!!int 1:30", json!(90)),
        ("!!int '0x_1f'", json!(31)),
        ("!!int ' 12 '", json!(12)),
        ("!!float 1", json!(1.0)),
        ("!!float '1_0'", json!(10.0)),
        ("!!float 1:30", json!(90.0)),
        ("!!float ' 1'", json!(1.0)),
        ("!!bool yes", json!(true)),
        ("!!null x", json!(null)),
        ("!!timestamp 2001-12-14", json!("2001-12-14")),
        // the non-specific `!` leaves a scalar to the resolver — quoted too
        ("! 123", json!(123)),
        ("! '123'", json!(123)),
        ("! \"yes\"", json!(true)),
        ("!!map {x: 1}", json!({"x": 1})),
        ("!!seq [1]", json!([1])),
        ("!!omap [{x: 1}, {y: 2}]", json!([["x", 1], ["y", 2]])),
        ("!!pairs [{x: 1}, {x: 2}]", json!([["x", 1], ["x", 2]])),
    ];
    for (text, want) in cases {
        same(text, &k(text), &want);
    }
}

#[test]
fn anchors_aliases_and_merges_resolve_as_pyyaml() {
    assert_eq!(load("b: &x 1\nc: *x\n").unwrap(), json!({"b": 1, "c": 1}));
    assert_eq!(
        load("b: &x {k: 1}\nc: *x\n").unwrap(),
        json!({"b": {"k": 1}, "c": {"k": 1}})
    );
    assert_eq!(load("a: [1, &q 2, *q]\n").unwrap(), json!({"a": [1, 2, 2]}));
    assert_eq!(load("&a a: b\n").unwrap(), json!({"a": "b"}));
    assert_eq!(load("<<: {a: 1}\nb: 2\n").unwrap(), json!({"a": 1, "b": 2}));
    assert_eq!(
        load("a: {<<: [{x: 1}, {x: 2, y: 3}], z: 0}\n").unwrap(),
        json!({"a": {"x": 1, "y": 3, "z": 0}})
    );
    assert_eq!(load("{<<: {a: 1}, a: 2}\n").unwrap(), json!({"a": 2}));
    let doc = "base: &b\n  id: a\n  mode: mem\none:\n  <<: *b\n  mode: disk\n";
    assert_eq!(
        load(doc).unwrap()["one"],
        json!({"id": "a", "mode": "disk"})
    );
}

#[test]
fn keys_are_python_dict_keys() {
    // a non-string key is written as python prints it (`True == 1`, so the
    // `1:` line lands on the `yes:` key: python `{True: 'a', None: 'b', …}`)
    assert_eq!(
        load("yes: 1\n1: a\n~: b\n1.5: c\n2001-12-14: d\n= : e\n").unwrap(),
        json!({"True": "a", "None": "b", "1.5": "c", "2001-12-14": "d", "=": "e"})
    );
    // keys python holds as EQUAL collapse onto the first, the last value wins
    assert_eq!(load("1: a\n1.0: b\ntrue: c\n").unwrap(), json!({"1": "c"}));
    assert_eq!(load("0: a\n-0.0: b\nno: c\n").unwrap(), json!({"0": "c"}));
    assert_eq!(
        load("2001-12-14 21:59:43+05:00: a\n2001-12-14 16:59:43Z: b\n").unwrap(),
        json!({"2001-12-14 21:59:43+05:00": "b"})
    );
    assert_eq!(load("a: b\na: c\n").unwrap(), json!({"a": "c"}));
    assert_eq!(load("{a: 1, a: 2}").unwrap(), json!({"a": 2}));
}

#[test]
fn what_python_refuses_is_refused() {
    for (doc, want) in [
        (
            "a: =\n",
            "could not determine a constructor for the tag 'tag:yaml.org,2002:value'",
        ),
        (
            "a: <<\n",
            "could not determine a constructor for the tag 'tag:yaml.org,2002:merge'",
        ),
        (
            "a: ! '&'\n",
            "could not determine a constructor for the tag 'tag:yaml.org,2002:yaml'",
        ),
        (
            "a: !foo bar\n",
            "could not determine a constructor for the tag '!foo'",
        ),
        (
            "a: {<<: 1}\n",
            "while constructing a mapping: expected a mapping or list of mappings for \
             merging, but found scalar",
        ),
        (
            "a: {<<: [1]}\n",
            "while constructing a mapping: expected a mapping for merging, but found scalar",
        ),
        ("a: *nope\n", "found undefined alias 'nope'"),
        ("a: &d 1\nb: &d 2\n", "found duplicate anchor 'd'"),
        (
            "x: &a [1, *a]\n",
            "found a recursive alias 'a' — a structure that contains itself has no JSON form",
        ),
        (
            "[1]: e\n",
            "while constructing a mapping: found unhashable key",
        ),
        (
            "? [a]\n: 1\n",
            "while constructing a mapping: found unhashable key",
        ),
        (
            "? [a]\n: 1",
            "while constructing a mapping: found unhashable key",
        ),
        (
            "? {a: 1}\n: 1\n",
            "while constructing a mapping: found unhashable key",
        ),
        (
            "{? [a] : 1}",
            "while constructing a mapping: found unhashable key",
        ),
        (
            "a: !!str [1]\n",
            "expected a scalar node, but found sequence",
        ),
        (
            "x: !!omap {a: 1}\n",
            "while constructing an ordered map: expected a sequence, but found mapping",
        ),
        (
            "x: !!omap [[1]]\n",
            "while constructing an ordered map: expected a mapping of length 1, but found \
             sequence",
        ),
        (
            "x: !!omap [{a: 1, b: 2}]\n",
            "while constructing an ordered map: expected a single mapping item, but found 2 \
             items",
        ),
        (
            "a: !!binary aGVsbG8=\n",
            "a !!binary node has no JSON form (python builds bytes)",
        ),
        (
            "a: !!set {x, y}\n",
            "a !!set node has no JSON form (python builds a set)",
        ),
        (
            "a: !!int abc\n",
            "invalid literal for int() with base 10: 'abc'",
        ),
        ("a: !!int\n", "not an int: ''"),
        (
            "a: !!float abc\n",
            "could not convert string to float: 'abc'",
        ),
        ("a: !!bool maybe\n", "not a bool: KeyError('maybe')"),
        ("a: !!timestamp bad\n", "not a timestamp: 'bad'"),
        ("a: 2001-13-14\n", "month must be in 1..12"),
        ("a: 2001-02-30\n", "day is out of range for month"),
        ("a: 0000-01-01\n", "year 0 is out of range"),
        ("a: 2001-12-14 24:00:00\n", "hour must be in 0..23"),
        (
            "a: 2001-12-14 01:02:03 +24\n",
            "offset must be a timedelta strictly between -timedelta(hours=24) and \
             timedelta(hours=24)",
        ),
        (
            "a: 1\n---\nb: 2\n",
            "expected a single document in the stream",
        ),
        (
            "a: [1, 2\n",
            "did not find expected ',' or ']' at line 2, column 1",
        ),
    ] {
        assert_eq!(load(doc).unwrap_err(), want, "{doc:?}");
    }
    // `k_err` is the single-value form of the same refusal
    assert!(k_err("!!int 12abc").starts_with("invalid literal for int()"));
}

#[test]
fn documents_and_streams_load_as_python_does() {
    assert_eq!(load("").unwrap(), json!(null));
    assert_eq!(load("~").unwrap(), json!(null));
    assert_eq!(load("---\n").unwrap(), json!(null));
    assert_eq!(load("--- 1\n").unwrap(), json!(1));
    assert_eq!(load("0").unwrap(), json!(0));
    assert_eq!(load("[]").unwrap(), json!([]));
    assert_eq!(load("{}").unwrap(), json!({}));
    assert_eq!(load("[a, b]").unwrap(), json!(["a", "b"]));
    assert_eq!(load("a: |\n  x\n  y\n").unwrap(), json!({"a": "x\ny\n"}));
    assert_eq!(load("a: 'it''s'\n").unwrap(), json!({"a": "it's"}));
    assert_eq!(load("a: \"\\u00e9\"\n").unwrap(), json!({"a": "\u{e9}"}));
    assert_eq!(load_all("").unwrap(), Vec::<Value>::new());
    assert_eq!(load_all("---\n").unwrap(), vec![json!(null)]);
    assert_eq!(
        load_all("--- 1\n--- 2\n").unwrap(),
        vec![json!(1), json!(2)]
    );
    assert_eq!(
        load_all("---\na: 1\n---\n---\nb: 2\n").unwrap(),
        vec![json!({"a": 1}), json!(null), json!({"b": 2})]
    );
    assert_eq!(load_all("a: 1\n").unwrap(), vec![json!({"a": 1})]);
    assert_eq!(
        load_all("---\n...\n---\nx\n").unwrap(),
        vec![json!(null), json!("x")]
    );
    // anchors do not cross documents
    assert_eq!(
        load_all("--- &a 1\n--- *a\n").unwrap_err(),
        "found undefined alias 'a'"
    );
}
