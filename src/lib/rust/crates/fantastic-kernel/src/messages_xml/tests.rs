//! The log format. Twin of python's `test_xml_rows_round_trip_every_shape_and_escape`
//! and `test_a_cut_last_row_is_a_loud_error_not_a_repair`.

use super::*;
use serde_json::json;

fn row(v: Value) -> Row {
    v.as_object().unwrap().clone()
}

fn round(v: Value) -> Row {
    let text = format!("{HEADER}{}", row_to_xml(&row(v)));
    let mut rows = parse_messages(&text).expect("parses");
    assert_eq!(rows.len(), 1);
    rows.remove(0)
}

#[test]
fn scalars_are_attributes_and_text_is_a_child() {
    let out = row_to_xml(&row(json!({
        "id": 412, "agent": "ai", "kind": "message", "ok": true, "ts": 1.5,
        "content": "hello",
    })));
    assert_eq!(
        out,
        "<row id=\"412\" agent=\"ai\" kind=\"message\" ok=\"true\" ts=\"1.5\">\
         <content>hello</content></row>\n"
    );
}

#[test]
fn a_body_that_contains_the_markup_cannot_break_the_log() {
    let body = "PRJ-1\n</content> & </row> <messages> done\n";
    let back = round(json!({"id": 1, "content": body}));
    assert_eq!(back["content"], json!(body));
    assert_eq!(back["id"], json!(1));
}

#[test]
fn a_quoted_title_with_a_tag_survives_the_attribute_escape() {
    let title = "read \"a.md\" <fast> & twice";
    let back = round(json!({"id": 2, "title": title}));
    assert_eq!(back["title"], json!(title));
}

#[test]
fn a_newline_bearing_string_becomes_a_child_whatever_its_name() {
    let out = row_to_xml(&row(json!({"id": 3, "note": "two\nlines"})));
    assert!(out.contains("<note>two\nlines</note>"), "{out}");
    assert_eq!(
        round(json!({"id": 3, "note": "two\nlines"}))["note"],
        "two\nlines"
    );
}

#[test]
fn lists_nested_objects_and_nulls() {
    let back = round(json!({
        "id": 4, "kind": "fold", "refs": [7, 8, 9],
        "edits": [{"path": "a.py", "hunks": 2}],
        "view": {"budget": 12000, "strategy": "fold"},
        "parent": null,
    }));
    assert_eq!(back["refs"], json!([7, 8, 9]));
    assert_eq!(back["edits"], json!([{"path": "a.py", "hunks": 2}]));
    assert_eq!(back["view"], json!({"budget": 12000, "strategy": "fold"}));
    assert!(!back.contains_key("parent"), "None is omitted: {back:?}");
}

#[test]
fn an_empty_string_and_an_empty_list_survive() {
    let back = round(json!({"id": 5, "content": "", "refs": []}));
    assert_eq!(back["content"], json!(""));
    assert_eq!(
        back["refs"],
        json!(""),
        "an empty list writes an empty element"
    );
}

#[test]
fn a_million_character_body_round_trips() {
    let big = "x".repeat(1_000_000);
    let back = round(json!({"id": 6, "content": big.clone()}));
    assert_eq!(back["content"].as_str().unwrap().len(), big.len());
}

#[test]
fn a_float_is_written_the_way_python_repr_writes_it() {
    assert!(row_to_xml(&row(json!({"ts": 1.5}))).contains("ts=\"1.5\""));
    assert!(row_to_xml(&row(json!({"ts": 3.0}))).contains("ts=\"3.0\""));
    assert!(row_to_xml(&row(json!({"ts": 1e16}))).contains("ts=\"1e+16\""));
}

#[test]
fn small_floats_use_a_two_digit_exponent() {
    // C51: python `repr(1e-05)` == "1e-05" — rust `format!("{f:e}")` writes
    // "1e-5" (one digit); only the SIGN was ever patched, not the width.
    assert!(row_to_xml(&row(json!({"ts": 1e-5}))).contains("ts=\"1e-05\""));
    assert!(row_to_xml(&row(json!({"ts": 1.5e-7}))).contains("ts=\"1.5e-07\""));
    // The positive side already had two digits at this magnitude; pin it too.
    assert!(row_to_xml(&row(json!({"ts": 1.5e17}))).contains("ts=\"1.5e+17\""));
}

#[test]
fn a_wide_integer_writes_its_exact_digits_not_a_float() {
    // C74/C51: an arbitrary-precision integer (wider than i64/u64) is still
    // an int on the python side — `str(v)`, exact digits, never routed
    // through f64 (which would lose precision and misclassify it as a float).
    let big =
        serde_json::Number::from_string_unchecked("123456789012345678901234567890".to_string());
    assert!(row_to_xml(&row(json!({"count": Value::Number(big)})))
        .contains("count=\"123456789012345678901234567890\""));
}

#[test]
fn many_rows_read_back_in_order() {
    let mut doc = String::from(HEADER);
    for i in 1..=5 {
        doc.push_str(&row_to_xml(&row(
            json!({"id": i, "content": format!("t{i}")}),
        )));
    }
    let rows = parse_messages(&doc).expect("parses");
    let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);
}

#[test]
fn a_cut_last_row_is_a_loud_error_not_a_repair() {
    let mut doc = String::from(HEADER);
    doc.push_str(&row_to_xml(&row(json!({"id": 1, "content": "whole"}))));
    let good = doc.clone();
    doc.push_str(&row_to_xml(&row(json!({"id": 2, "content": "cut here"}))));
    let cut = &doc[..doc.len() - 12];
    let err = parse_messages(cut).expect_err("a cut row must be loud");
    assert!(err.contains("row 1"), "it names the last GOOD row: {err}");
    assert!(err.contains("nothing is repaired automatically"), "{err}");
    // …and the whole prefix still reads fine, which is what makes the operator's
    // truncate-at-the-last-close a real instruction.
    assert_eq!(parse_messages(&good).unwrap().len(), 1);
}

#[test]
fn an_empty_log_and_a_header_only_log_read_as_nothing() {
    assert!(parse_messages("").unwrap().is_empty());
    assert!(parse_messages(HEADER).unwrap().is_empty());
}

#[test]
fn a_carriage_return_normalizes_the_way_every_xml_reader_does() {
    // NOT a round trip, and deliberately so: ElementTree writes `\r` inside
    // element text unescaped and XML 1.0 §2.11 turns it into `\n` on the way
    // back, so python's own log is lossy here. Matching that keeps the two
    // runtimes byte-identical, which matters more than the character.
    let back = round(json!({"id": 7, "content": "a\r\nb\rc"}));
    assert_eq!(back["content"], json!("a\nb\nc"));
    // A `\r`-BEARING STRING IS NEVER AN ATTRIBUTE: the writer sends any string
    // holding a newline or a return to a child element, so this is the only
    // path a stored `\r` can take.
    let back = round(json!({"id": 8, "tag": "a\rb"}));
    assert_eq!(back["tag"], json!("a\nb"));
    // An ESCAPED one, in an attribute someone else wrote, survives — because
    // normalization runs on the raw text, before unescaping, and so never
    // reaches the character the escape was there to protect.
    let rows = parse_messages("<row tag=\"a&#13;b\" />\n").expect("parses");
    assert_eq!(rows[0]["tag"], json!("a\rb"));
}

/// ONE TAPE, BYTE FOR BYTE. A `call` row carries the dispatch payload and a
/// `settle` row the whole outcome; the payload's map keys are whatever the
/// CALLER wrote (`$parent` is not an XML name → `<entry key="…">`) and a raw
/// body is its LENGTH (`<bytes len="N" />`, the ONE reduction — base64 is
/// banned). Pinned against `kernel_state.messages_xml` run on the same maps
/// (2026-09-24), because both runtimes append to the same file.
#[test]
fn a_call_and_its_settle_are_python_byte_for_byte() {
    let call = json!({
        "id": 11, "kind": "call", "verb": "write_stream", "parent": null, "origin": null,
        "payload": {
            "type": "create_agent",
            "callable_by": {"$parent": "*", "rest": ["read", "write"]},
            "bytes": {"len": 3},
        },
        "named": false, "retries": 0, "attempts": 1, "timeout": null,
        "agent": "ai", "ts": 1.5,
    });
    assert_eq!(
        row_to_xml(call.as_object().unwrap()),
        "<row id=\"11\" kind=\"call\" verb=\"write_stream\" named=\"false\" retries=\"0\" \
         attempts=\"1\" agent=\"ai\" ts=\"1.5\"><payload type=\"create_agent\"><callable_by>\
         <entry key=\"$parent\">*</entry><rest><item>read</item><item>write</item></rest>\
         </callable_by><bytes len=\"3\" /></payload></row>\n"
    );
    let settle = json!({
        "kind": "settle", "parent": 11, "settled": "ai",
        "outcome": {"status": "completed", "result": {"chunk": {"len": 9}}},
        "attempts": 2, "id": 12, "agent": "kernel_state", "ts": 1.5,
    });
    assert_eq!(
        row_to_xml(settle.as_object().unwrap()),
        "<row kind=\"settle\" parent=\"11\" settled=\"ai\" attempts=\"2\" id=\"12\" \
         agent=\"kernel_state\" ts=\"1.5\"><outcome status=\"completed\"><result>\
         <chunk len=\"9\" /></result></outcome></row>\n"
    );
    // ...and both read back as the maps python reads back: the entry key is the
    // KEY (a string, never coerced), and a raw body is `{"len": N}`.
    let doc = format!(
        "{HEADER}{}{}",
        row_to_xml(call.as_object().unwrap()),
        row_to_xml(settle.as_object().unwrap())
    );
    let back = parse_messages(&doc).expect("round trip");
    assert_eq!(
        Value::Object(back[0].clone()),
        json!({
            "id": 11, "kind": "call", "verb": "write_stream", "named": false, "retries": 0,
            "attempts": 1, "agent": "ai", "ts": 1.5,
            "payload": {
                "type": "create_agent",
                "callable_by": {"$parent": "*", "rest": ["read", "write"]},
                "bytes": {"len": 3},
            },
        })
    );
    assert_eq!(
        Value::Object(back[1].clone()),
        json!({
            "kind": "settle", "parent": 11, "settled": "ai", "attempts": 2, "id": 12,
            "agent": "kernel_state", "ts": 1.5,
            "outcome": {"status": "completed", "result": {"chunk": {"len": 9}}},
        })
    );
}

/// A KEY THAT IS NOT AN XML NAME rides `<entry>` wherever it sits — nested, in a
/// list, and as the whole value — and comes back as a STRING key.
#[test]
fn non_xml_name_keys_round_trip_through_entry() {
    let row = json!({
        "id": 1, "agent": "ai", "kind": "call",
        "payload": {
            "1": "a digit is not an XML name",
            "": "neither is empty",
            "a b": {"$deep": [1, {"$in": true}]},
            "ok.key-1": "a plain name still rides as an attribute",
        },
    });
    let text = format!("{HEADER}{}", row_to_xml(row.as_object().unwrap()));
    assert!(
        text.contains("<entry key=\"1\">") && text.contains("ok.key-1=\""),
        "{text}"
    );
    let back = parse_messages(&text).expect("round trip");
    assert_eq!(Value::Object(back[0].clone()), row);
}
