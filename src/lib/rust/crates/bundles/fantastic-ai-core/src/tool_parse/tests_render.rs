//! The two renderers. `render_call` is the parser's inverse (its round trip is
//! pinned in `tests_grammar.rs`); `render_reply` is the notation a REPLY comes
//! back in — one notation on the whole model↔harness path, no JSON either way.
//!
//! Mirrors what python's `render_reply` produces, shape for shape
//! (`ai_core/tool_parse.py:293-333`).

use serde_json::json;

use super::{render_call, render_reply};

#[test]
fn a_dict_is_one_line_per_field() {
    assert_eq!(
        render_reply(&json!({"path": "a.md", "size": 12, "ok": true})),
        "<path>a.md</path>\n<size>12</size>\n<ok>true</ok>"
    );
}

#[test]
fn a_nested_dict_opens_on_its_own_line() {
    assert_eq!(
        render_reply(&json!({"agent": {"id": "notes", "verbs": "read"}})),
        "<agent>\n<id>notes</id>\n<verbs>read</verbs>\n</agent>"
    );
}

#[test]
fn a_top_level_list_is_items() {
    assert_eq!(
        render_reply(&json!(["a", "b"])),
        "<items>\n<item>a</item>\n<item>b</item>\n</items>"
    );
}

#[test]
fn a_list_of_structures_nests_and_an_empty_list_takes_that_branch() {
    assert_eq!(
        render_reply(&json!({"lines": [{"n": 1}, {"n": 2}]})),
        "<lines>\n<item>\n<n>1</n>\n</item>\n<item>\n<n>2</n>\n</item>\n</lines>"
    );
    assert_eq!(render_reply(&json!({"lines": []})), "<lines>\n</lines>");
}

#[test]
fn a_multiline_scalar_spans_lines_verbatim() {
    assert_eq!(
        render_reply(&json!({"text": "one\ntwo"})),
        "<text>\none\ntwo\n</text>"
    );
}

#[test]
fn null_is_an_empty_tag_and_a_bare_scalar_is_its_text() {
    assert_eq!(render_reply(&json!({"next": null})), "<next></next>");
    assert_eq!(render_reply(&json!("just text")), "just text");
    assert_eq!(render_reply(&json!(false)), "false");
}

/// NOTHING IS ESCAPED. A read's lines reach the model as lines, and a quote
/// stays a quote — the whole reason the notation replaced JSON on this path.
#[test]
fn nothing_is_escaped() {
    let rendered = render_reply(&json!({"content": "say \"hi\"\nand \\ that"}));
    assert_eq!(rendered, "<content>\nsay \"hi\"\nand \\ that\n</content>");
}

/// `render_call` nests WITHOUT the newline `render_reply` puts after an open
/// tag. Two renderers, two shapes, on purpose.
#[test]
fn render_call_nests_tighter_than_render_reply() {
    let args = json!({"target_id": "notes", "payload": {"type": "read", "path": "a.md"}});
    assert_eq!(
        render_call("send", args.as_object().unwrap()),
        "<call name=\"send\">\n<target_id>notes</target_id>\n<type>read</type>\n\
         <path>a.md</path>\n</call>"
    );
}

#[test]
fn reply_scalars_are_python_str() {
    // python `_reply_text`: `None` is empty, a bool is lower-case, and everything
    // else is `str(v)` — a float's PYTHON repr (`1e-05`, `1e+16`, `inf`, `nan`)
    // and a structure's python repr inside a mixed list.
    assert_eq!(render_reply(&json!(1e-5)), "1e-05");
    assert_eq!(render_reply(&json!(1e16)), "1e+16");
    assert_eq!(render_reply(&json!(0.5)), "0.5");
    assert_eq!(render_reply(&json!(3.0)), "3.0");
    assert_eq!(render_reply(&json!(null)), "");
    assert_eq!(render_reply(&json!(true)), "true");
    // A list that MIXES a scalar with a structure renders each element on its
    // own: the scalar as text, the structure nested (a `tree=ids` with stubs).
    assert_eq!(
        render_reply(&json!(["a", {"a": 1}])),
        "<items>\n<item>a</item>\n<item>\n<a>1</a>\n</item>\n</items>"
    );
    // Nested, a structure is still a python repr on the line.
    assert_eq!(
        render_reply(&json!({"v": {"a": 1, "b": [true, null]}})),
        "<v>\n<a>1</a>\n<b>\n<item>true</item>\n<item></item>\n</b>\n</v>"
    );
}
