//! The CALL CHECKER — python `tests/test_call_check.py`, case for case and string
//! for string (the reports are the parity proof).

use super::{check, repair, MAX_REPORTS};
use crate::think::without_thinking;
use crate::tool_parse::extract_tool_calls;

const SEND: &str = "<call name=\"send\"><target_id>files</target_id><type>reflect</type></call>";
const DROP: &str =
    "drop the wrapper: write the call at the top level, as <call name=\"…\">…</call>";

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

/// A grep scoped by `<ids>` that lost its leading `<` keeps its scope: the form
/// to write is not only target/title/type. Every argument the model wrote carries
/// over, a nested `<item>` as part of its list.
#[test]
fn the_verbs_own_arguments_carry_into_the_form_to_write() {
    let broken = "call name=\"send\">\n<target_id>ai_fold</target_id>\n\
                  <title>grep for checksum</title>\n<type>grep</type>\n\
                  <query>checksum</query>\n<ids><item>32</item></ids>\n</call>";
    let r = check(broken, 0);
    assert_eq!(r.len(), 1, "{r:?}");
    let form = r[0]
        .split_once("write it as:\n")
        .and_then(|(_, rest)| rest.split_once("\nA call is"))
        .map(|(f, _)| f)
        .unwrap_or_default();
    assert_eq!(
        form,
        "<call name=\"send\">\n<target_id>ai_fold</target_id>\n\
         <title>grep for checksum</title>\n<type>grep</type>\n\
         <query>checksum</query>\n<ids><item>32</item></ids>\n</call>",
        "{}",
        r[0]
    );
}

/// python `test_a_call_opener_that_lost_its_lt_is_named_as_that`.
#[test]
fn a_call_opener_that_lost_its_lt_is_named_as_that() {
    assert_eq!(
        check(r#"call name="await"></call>"#, 0),
        vec![format!(
            "[malformed call — nothing ran] line 1, col 1:\n  call name=\"await\">‹/call›\n  \
             ^~~~~~~~~~~~~~~~~~ the opening `<` is missing\nwrite it as:\n<call name=\"await\"></call>\n{TAIL}"
        )]
    );
}

/// python `test_repair_gives_a_dropped_lt_back_and_nothing_else`.
#[test]
fn repair_gives_a_dropped_lt_back_and_nothing_else() {
    let broken = "call name=\"send\">\n<target_id>kernel</target_id>\n<type>docs</type>\n</call>";
    assert_eq!(repair(broken), format!("<{broken}"));
    assert!(check(&repair(broken), 1).is_empty());
    for same in [
        "I will call name=\"x\"> later",
        "<call name=\"done\"></call>",
        "call name=\"send\"><target_id>a</target_id>",
        E59,
    ] {
        assert_eq!(repair(same), same);
    }
}

/// #188: a valid call inside a `<tool_call>` wrapper is NOT dispatched. The report
/// puts the caret under the wrapper and shows the call bare, the form to write.
#[test]
fn a_call_inside_a_tool_call_wrapper_is_refused_with_the_bare_rewrite() {
    let answer = format!("I'll look at the files agent.\n<tool_call>{SEND}</tool_call>");
    assert_eq!(
        check(&answer, 0),
        vec![format!(
            "[malformed call — nothing ran] line 2, col 1:\n  \
             <tool_call>‹call name=\"send\"><target_id>files</target_id><type>reflect</type>‹/call›</tool_call>\n  \
             ^~~~~~~~~~~ {DROP}\nwrite it as:\n<call name=\"send\">\n<target_id>files</target_id>\n\
             <type>reflect</type>\n</call>\n{TAIL}"
        )]
    );
}

/// The wrapper is any tag, not a fixed list of names.
#[test]
fn a_call_inside_an_arbitrary_tag_is_refused() {
    let r = &check(&format!("<foo>\n{SEND}\n</foo>"), 0)[0];
    assert!(
        r.starts_with("[malformed call — nothing ran] line 1, col 1:\n  <foo>\n  ^~~~~ "),
        "{r}"
    );
    assert!(r.contains(&format!("  ^~~~~ {DROP}\n")), "{r}");
    assert!(r.contains("write it as:\n<call name=\"send\">\n<target_id>files</target_id>"));
}

/// A wrapped call with a fault inside is refused too, and the fault is named.
#[test]
fn a_malformed_call_inside_a_wrapper_names_its_fault() {
    let answer = "<tool_call><call name=\"send\"><type>reflect</type></call></tool_call>";
    let r = &check(answer, 0)[0];
    assert!(
        r.contains("also: the call inside does not parse — a send names its target"),
        "{r}"
    );
    assert!(!r.contains("write it as:"), "{r}");
}

/// The wrapped call is refused; the top-level one is not a report, and the report
/// says the other call ran.
#[test]
fn a_top_level_call_beside_a_wrapped_one_runs_and_the_report_says_so() {
    let answer = format!("{SEND}\n<tool_call>{SEND}</tool_call>");
    assert_eq!(extract_tool_calls(&answer).len(), 1);
    let reports = check(&answer, 1);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].starts_with("[malformed call — nothing ran] line 2, col 1:"));
    assert!(reports[0].ends_with(
        "The other call of this answer ran — its reply is in this batch; do not write it \
         again."
    ));
}

/// A closed tag before a call wraps nothing: the call runs and no report comes.
#[test]
fn a_call_after_a_closed_tag_is_not_wrapped() {
    let answer = format!("<b>x</b> {SEND}");
    assert_eq!(extract_tool_calls(&answer).len(), 1);
    assert!(check(&answer, 1).is_empty());
}

/// Thinking is stripped before the answer is read: a call after `</think>` runs,
/// and one inside `<think>` is not an answer call at all.
#[test]
fn a_call_after_think_runs_and_one_inside_think_does_not() {
    let answer = format!("<think>plan {SEND}</think>\n{SEND}");
    let stripped = without_thinking(&answer);
    assert_eq!(extract_tool_calls(&stripped).len(), 1);
    assert!(check(&stripped, 1).is_empty());
}

/// An answer with no call at all whose tag is left open names the wrapper and shows
/// the form to write — not only the generic no-call reminder.
#[test]
fn an_empty_wrapper_gets_the_wrapper_report() {
    let answer = "<tool_call>\n<function_calls>\n<function_calls>\n</function_calls>";
    assert_eq!(
        check(answer, 0),
        vec![format!(
            "[malformed call — nothing ran] line 1, col 1:\n  <tool_call>\n  ^~~~~~~~~~~ \
             `<tool_call>` holds no call — write one at the top level, as \
             <call name=\"…\">…</call>\nwrite it as:\n<call name=\"send\">\n\
             <target_id>agent id</target_id>\n<type>verb</type>\n</call>\n\
             If it is finished, end the chain explicitly: <call name=\"done\"></call>\n{TAIL}"
        )]
    );
}

/// Prose around an open wrapper is still an open wrapper with no call in it.
#[test]
fn an_open_wrapper_after_prose_is_named() {
    let r = &check("I'll look at the files.\n<tool_call>", 0)[0];
    assert!(
        r.starts_with("[malformed call — nothing ran] line 2, col 1:\n  <tool_call>"),
        "{r}"
    );
}

/// A wrapper is only a wrapper while it is open: a closed one, with no call in it, is
/// not reported.
#[test]
fn a_closed_empty_wrapper_is_not_reported() {
    assert!(check("<tool_call></tool_call>", 0).is_empty());
}

/// #188 review: a tag-shaped token in INLINE CODE is code, not a wrapper. The call
/// after it runs, and no report comes.
#[test]
fn a_call_after_a_type_in_inline_code_runs_with_no_report() {
    let answer = format!("The helper returns `Vec<String>`.\n{SEND}");
    assert_eq!(extract_tool_calls(&answer).len(), 1);
    assert!(check(&answer, 1).is_empty(), "{:?}", check(&answer, 1));
}

/// A tag-shaped token in a CODE FENCE is code, not a wrapper: the call after the
/// fence runs with no report.
#[test]
fn a_call_after_a_fenced_div_runs_with_no_report() {
    let answer = format!("```html\n<div>\n```\n{SEND}");
    assert_eq!(extract_tool_calls(&answer).len(), 1);
    assert!(check(&answer, 1).is_empty(), "{:?}", check(&answer, 1));
}

/// A `<br>` or `<b and c>` in plain prose is not an open wrapper that would take the
/// call after it: `<b and c>` has bare words where an attribute would be, and a
/// `<br>` inside a code span is code.
#[test]
fn prose_angle_brackets_that_are_not_tags_do_not_wrap_a_call() {
    for prose in ["if a<b and c>d then", "line `line<br>` ends"] {
        let answer = format!("{prose}\n{SEND}");
        assert_eq!(extract_tool_calls(&answer).len(), 1, "{prose}");
        assert!(
            check(&answer, 1).is_empty(),
            "{prose}: {:?}",
            check(&answer, 1)
        );
    }
}

/// An open tag in code is no wrapper for an empty-answer report either: a code span
/// holding `<String>` is not a wrapper that holds no call.
#[test]
fn a_code_span_is_not_an_open_wrapper_in_the_report() {
    assert!(check("returns `Vec<String>` and nothing else", 0).is_empty());
}

/// #188 review: a backtick inside a call's argument must not open a code span that
/// runs on past the call and hides the wrapper after it. Code is masked over the
/// prose only (as the stream splitter reads it), so the wrapped call is refused.
#[test]
fn a_backtick_inside_a_call_argument_does_not_hide_a_wrapper_after_it() {
    let answer = format!(
        "<call name=\"send\"><target_id>a</target_id><type>run</type><cmd>echo `x</cmd></call>\n\
         <tool_call>`y` {SEND}</tool_call>"
    );
    assert_eq!(extract_tool_calls(&answer).len(), 1);
    let r = check(&answer, 1);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(
        r[0].starts_with("[malformed call — nothing ran] line 2, col 1:\n  <tool_call>"),
        "{}",
        r[0]
    );
    assert!(
        r[0].contains(&format!("  ^~~~~~~~~~~ {DROP}\n")),
        "{}",
        r[0]
    );
    assert!(r[0].ends_with("The other call of this answer ran — its reply is in this batch; do not write it again."), "{}", r[0]);
}
