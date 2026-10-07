//! The CALL CHECKER — python `tests/test_call_check.py`, case for case and string
//! for string (the reports are the parity proof).

use super::{check, MAX_REPORTS};

const E59: &str = "I'll look at the files agent.\n<tool_call>\n<function=call name=\"send\">\n\
<target_id>files</target_id>\n<type>reflect</type>\n</call>\n</tool_call>";

const TAIL: &str = "A call is <call name=\"…\">, one child tag per argument, closed by </call>.";

#[test]
fn the_measured_shape_is_reported_with_the_form_to_write() {
    assert_eq!(
        check(E59, 0),
        vec![format!(
            "[malformed call — nothing ran] line 3, col 1:\n  <function=call name=\"send\">\n  \
             ^~~~~~~~~~~~~~~~~~~~~~~~~~~ not a call tag\nwrite it as:\n<call name=\"send\">\n<target_id>files</target_id>\n\
             <type>reflect</type>\n</call>\nalso: line 2 `<tool_call>` — drop the wrapper.\n{TAIL}"
        )]
    );
}

#[test]
fn a_call_name_as_a_tag_next_to_a_good_call_says_the_good_one_ran() {
    let answer = "<call name=\"send\"><target_id>a</target_id><type>reflect</type></call>\n\
                  <send><target_id>b</target_id><type>read</type></send>";
    assert_eq!(
        check(answer, 1),
        vec![format!(
            "[malformed call — nothing ran] line 2, col 1:\n  \
             <send><target_id>b</target_id><type>read</type></send>\n  ^~~~~~ not a call tag\n\
             write it as:\n<call name=\"send\">\n<target_id>b</target_id>\n<type>read</type>\n</call>\n\
             also: `</send>` — close with </call>.\n{TAIL}\nThe other call of this answer ran — its \
             reply is in this batch; do not write it again."
        )]
    );
    assert!(check(answer, 2)[0].ends_with(
        "The other 2 calls of this answer ran — their replies are in this batch; do not write \
         them again."
    ));
}

#[test]
fn a_json_call_is_shown_as_tags() {
    let r = &check(
        "{\"name\": \"send\", \"arguments\": {\"target_id\": \"files\", \"payload\": {\"type\": \
         \"list\", \"path\": \".\"}}}",
        0,
    )[0];
    assert!(
        r.contains("^~~~~~~~~~~~~~~ calls are tags, not JSON\n"),
        "{r}"
    );
    assert!(
        r.contains(
            "write it as:\n<call name=\"send\">\n<target_id>files</target_id>\n<type>list</type>\n\
             <path>.</path>\n</call>\n"
        ),
        "{r}"
    );
}

#[test]
fn qwen_coder_parameters_become_child_tags() {
    let r = &check(
        "<function=send>\n<parameter=target_id>\nfiles\n</parameter>\n\
         <parameter=type>reflect</parameter>\n</function>",
        0,
    )[0];
    assert!(
        r.starts_with(
            "[malformed call — nothing ran] line 1, col 1:\n  <function=send>\n  ^~~~~~~~~~~~~~~ not a call tag\n"
        ),
        "{r}"
    );
    assert!(r.contains("<call name=\"send\">\n<target_id>files</target_id>\n<type>reflect</type>"));
    assert!(
        r.contains(
            "also: `<parameter=target_id>` — one child tag per argument: <target_id>…</target_id>; \
             `<parameter=type>` — one child tag per argument: <type>…</type>."
        ),
        "{r}"
    );
}

#[test]
fn a_control_call_as_a_bare_tag() {
    let r = &check("finished.\n<done/>", 0)[0];
    assert!(
        r.contains("line 2, col 1:\n  <done/>\n  ^~~~~~~ not a call tag\n"),
        "{r}"
    );
    assert!(
        r.contains("write it as:\n<call name=\"done\"></call>\n"),
        "{r}"
    );
}

#[test]
fn arguments_with_no_call_open() {
    let r = &check("<target_id>notes</target_id>\n<type>read</type>", 0)[0];
    assert!(
        r.contains("^~~~~~~~~~~ an argument outside a call — open the call above it"),
        "{r}"
    );
    assert!(r.contains("<call name=\"send\">\n<target_id>notes</target_id>\n<type>read</type>"));
}

#[test]
fn a_stray_close_after_a_good_call() {
    let r = &check("<call name=\"done\"></call></call>", 0)[0];
    assert!(r.contains("line 1, col 26:"), "{r}");
    assert!(
        r.contains("\u{2039}call name=\"done\">\u{2039}/call\u{203a}\u{2039}/call\u{203a}"),
        "{r}"
    );
    assert!(
        r.split('\n')
            .nth(2)
            .unwrap_or("")
            .ends_with("^~~~~~~ a close with no call open — delete it"),
        "{r}"
    );
    assert!(!r.contains("write it as"), "{r}");
}

#[test]
fn prose_and_foreign_markup_without_our_vocabulary_are_not_calls() {
    for answer in [
        "All done, the file is written.",
        "Use <b>bold</b> and a <div class='x'>box</div>; 3 < 4 > 2.",
        "<tool_call>\n</tool_call>",
        "<call name=\"done\"></call>",
    ] {
        assert!(check(answer, 0).is_empty(), "{answer}");
    }
}

#[test]
fn what_a_call_envelope_holds_is_never_checked() {
    let good =
        "<call name=\"send\"><target_id>notes</target_id><type>write</type><path>x.md</path>\
                <bytes><send>no</send> </call> {\"name\": \"send\"}</bytes></call>";
    assert!(check(good, 1).is_empty());
    assert!(check("<call name=\"send\" target_id=\"x\"></call>", 0).is_empty());
}

#[test]
fn reports_are_capped() {
    let answer = (0..5)
        .map(|i| format!("<send><target_id>a{i}</target_id></send>"))
        .collect::<Vec<_>>()
        .join("\n");
    let reports = check(&answer, 0);
    assert_eq!(reports.len(), MAX_REPORTS);
    assert!(reports[MAX_REPORTS - 1].ends_with("(2 more malformed calls not shown.)"));
}

#[test]
fn a_long_line_is_windowed_and_the_caret_follows() {
    let answer = format!("{}<function=call name=\"send\"></call>", "x".repeat(100));
    let r = &check(&answer, 0)[0];
    let lines: Vec<&str> = r.split('\n').collect();
    let (quoted, caret) = (lines[1], lines[2]);
    assert!(quoted.starts_with("  …") && caret.trim_start().starts_with('^'));
    let q = quoted.chars().position(|c| c == '<').expect("<");
    let k = caret.chars().position(|c| c == '^').expect("^");
    assert_eq!(q, k, "{quoted}\n{caret}");
}

#[test]
fn a_multibyte_line_keeps_the_caret_under_the_token() {
    // byte offsets differ from char columns here; the report counts chars, as python
    let r = &check("é→ <send><target_id>a</target_id></send>", 0)[0];
    assert!(
        r.starts_with("[malformed call — nothing ran] line 1, col 4:\n  é→ <send>"),
        "{r}"
    );
    assert!(r.contains("\n     ^~~~~~ not a call tag"), "{r}");
}
