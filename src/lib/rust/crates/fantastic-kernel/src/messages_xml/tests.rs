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
        "id": 412, "agent": "ai", "kind": "turn", "ok": true, "ts": 1.5,
        "content": "hello",
    })));
    assert_eq!(
        out,
        "<row id=\"412\" agent=\"ai\" kind=\"turn\" ok=\"true\" ts=\"1.5\">\
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
