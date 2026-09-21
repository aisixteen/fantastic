//! Cold hydration and the loud failures. The pump's own round trip is the
//! integration test (`tests/message_log.rs`); these are the file-level rules.

use super::*;
use crate::messages_xml::row_to_xml;
use serde_json::json;

fn row(v: Value) -> Row {
    v.as_object().unwrap().clone()
}

fn write(dir: &Path, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(MESSAGES_FILE), text).unwrap();
}

fn log_of(rows: &[Value]) -> String {
    let mut doc = String::from(HEADER);
    for r in rows {
        doc.push_str(&row_to_xml(&row(r.clone())));
    }
    doc
}

#[test]
fn a_missing_log_hydrates_nothing_and_starts_the_counter_at_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (rows, next) = read_messages(tmp.path()).expect("a virgin dir is not an error");
    assert!(rows.is_empty());
    assert_eq!(next, 1);
}

#[test]
fn hydration_sets_the_counter_above_every_id_it_read() {
    let tmp = tempfile::TempDir::new().unwrap();
    write(
        tmp.path(),
        &log_of(&[
            json!({"id": 3, "agent": "ai", "role": "user", "content": "one"}),
            json!({"id": 5, "agent": "ai", "role": "assistant", "content": "two"}),
            json!({"id": 9, "agent": "other", "kind": "turn", "content": "three"}),
        ]),
    );
    let kernel = crate::Kernel::new();
    assert_eq!(hydrate_messages(&kernel, tmp.path()).unwrap(), 3);
    // THE COUNTER FIRST: every hydrated id is a pre-minted one, and the next
    // mint continues above all of them rather than colliding.
    assert_eq!(kernel.rows.next_id(), 10);
    assert_eq!(kernel.rows.mint_id(), 10);
    let mine = kernel.rows.for_agent(&AgentId::from("ai"));
    assert_eq!(mine.len(), 2);
    assert_eq!(mine[0]["content"], "one");
    assert_eq!(kernel.rows.for_agent(&AgentId::from("other")).len(), 1);
}

#[test]
fn a_second_ingestion_of_the_same_file_is_loud() {
    // It is the caller's mistake, not something to de-duplicate silently: the
    // store is the only allocator and it says so on the first duplicate id.
    let tmp = tempfile::TempDir::new().unwrap();
    write(
        tmp.path(),
        &log_of(&[json!({"id": 1, "agent": "ai", "content": "x"})]),
    );
    let kernel = crate::Kernel::new();
    assert_eq!(hydrate_messages(&kernel, tmp.path()).unwrap(), 1);
    let err = hydrate_messages(&kernel, tmp.path()).expect_err("a re-ingest must be loud");
    assert!(err.contains("pre-minted"), "{err}");
}

#[test]
fn an_unknown_log_version_refuses() {
    let tmp = tempfile::TempDir::new().unwrap();
    write(
        tmp.path(),
        "<messages v=\"9\">\n<row id=\"1\" agent=\"ai\" />\n",
    );
    let err = read_messages(tmp.path()).expect_err("a foreign version must refuse");
    // C55: python `f"{head!r}"` — python repr quoting (single quotes), not
    // rust's Debug `{:?}`.
    assert_eq!(
        err,
        "messages.xml: unknown log version — first line is '<messages v=\"9\">', \
         this loader writes '<messages v=\"1\">'"
    );
}

#[test]
fn a_cut_last_row_is_loud_and_names_the_last_good_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    let doc = log_of(&[
        json!({"id": 1, "agent": "ai", "content": "whole"}),
        json!({"id": 2, "agent": "ai", "content": "cut"}),
    ]);
    write(tmp.path(), &doc[..doc.len() - 10]);
    let err = read_messages(tmp.path()).expect_err("a cut row must be loud");
    assert!(err.contains("row 1"), "{err}");
    assert!(err.contains("nothing is repaired automatically"), "{err}");
}

#[test]
fn a_row_with_no_agent_refuses_rather_than_guessing_one() {
    let tmp = tempfile::TempDir::new().unwrap();
    write(
        tmp.path(),
        &log_of(&[json!({"id": 1, "content": "orphan"})]),
    );
    let kernel = crate::Kernel::new();
    let err = hydrate_messages(&kernel, tmp.path()).expect_err("an orphan row must refuse");
    assert!(err.contains("no `agent`"), "{err}");
}

#[test]
fn every_shape_survives_the_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    write(
        tmp.path(),
        &log_of(&[
            json!({"id": 1, "agent": "ai", "kind": "turn", "role": "user",
                   "content": "a call: <call name=\"send\"></call> & more\nline two\n"}),
            json!({"id": 2, "agent": "ai", "kind": "fold", "level": 1,
                   "refs": [1], "title": "the survey", "summary": "what it said"}),
            json!({"id": 3, "agent": "ai", "kind": "view", "budget": 12000,
                   "ids": [{"id": 1, "res": "full"}]}),
        ]),
    );
    let kernel = crate::Kernel::new();
    assert_eq!(hydrate_messages(&kernel, tmp.path()).unwrap(), 3);
    let rows = kernel.rows.for_agent(&AgentId::from("ai"));
    assert!(rows[0]["content"]
        .as_str()
        .unwrap()
        .contains("<call name=\"send\"></call>"));
    assert_eq!(rows[1]["refs"], json!([1]));
    assert_eq!(rows[1]["level"], json!(1));
    // The view row rebuilt its index on the way in, so it is CURRENT.
    let view = kernel
        .rows
        .view_get(&AgentId::from("ai"))
        .expect("the view");
    assert_eq!(view["budget"], json!(12000));
    assert_eq!(view["ids"], json!([{"id": 1, "res": "full"}]));
}
