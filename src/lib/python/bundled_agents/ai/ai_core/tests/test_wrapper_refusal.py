"""A call inside any wrapper tag is refused with a fix-it report (Rust #189, part 2).

Ports the Rust tests of the same names: the scanner, the call checker and the stream
splitter all read the wrapper the same way (ai_core.grammar)."""

from ai_core.call_check import check
from ai_core.grammar import THINK_CLOSE, THINK_OPEN
from ai_core.stream_parts import PartSplitter
from ai_core.tool_parse import extract_tool_calls

SEND = '<call name="send"><target_id>files</target_id><type>reflect</type></call>'
DONE = '<call name="done"></call>'


def test_a_call_inside_a_tool_call_wrapper_is_refused_with_the_bare_rewrite():
    answer = f"I'll look at the files agent.\n<tool_call>{SEND}</tool_call>"
    reports = check(answer, ran=0)
    assert len(reports) == 1
    assert reports[0].startswith(
        "[malformed call — nothing ran] line 2, col 1:\n  <tool_call>"
    )
    assert "  ^~~~~~~~~~~ drop the wrapper" in reports[0]
    assert (
        'write it as:\n<call name="send">\n<target_id>files</target_id>\n'
        "<type>reflect</type>\n</call>"
    ) in reports[0]


def test_a_call_inside_an_arbitrary_tag_is_refused():
    reports = check(f"<foo>\n{SEND}\n</foo>", ran=0)
    assert len(reports) == 1
    assert reports[0].startswith(
        "[malformed call — nothing ran] line 1, col 1:\n  <foo>\n  ^~~~~ drop the wrapper"
    )


def test_a_top_level_call_beside_a_wrapped_one_runs_and_the_report_says_so():
    answer = f"<tool_call>{SEND}</tool_call>\n{SEND}"
    assert len(extract_tool_calls(answer)) == 1
    reports = check(answer, ran=1)
    assert len(reports) == 1
    assert "The other call of this answer ran" in reports[0]


def test_a_call_after_a_closed_tag_is_not_wrapped():
    answer = f"<tool_call>x</tool_call>\n{SEND}"
    assert len(extract_tool_calls(answer)) == 1
    assert check(answer, ran=1) == []


def test_a_call_after_think_runs_and_one_inside_think_does_not():
    answer = f"{THINK_OPEN}{SEND}{THINK_CLOSE}\n{DONE}"
    calls = extract_tool_calls(answer)
    assert [c["name"] for c in calls] == ["done"]
    reports = check(answer, ran=1)
    assert len(reports) == 1
    assert "<think>" in reports[0] and "drop the wrapper" in reports[0]
    assert "The other call of this answer ran" in reports[0]


def test_an_empty_wrapper_gets_the_wrapper_report():
    reports = check("<tool_call>\n<function_calls>\n</function_calls>", ran=0)
    assert len(reports) == 1
    assert "`<tool_call>` holds no call" in reports[0]
    assert '<call name="send">' in reports[0]


def test_an_open_wrapper_after_prose_is_named():
    reports = check("Let me look.\n<function_calls>", ran=0)
    assert len(reports) == 1 and "`<function_calls>`" in reports[0]


def test_a_closed_empty_wrapper_is_not_reported():
    assert check("<tool_call></tool_call>", ran=0) == []


def test_a_call_after_a_fenced_div_runs_with_no_report():
    answer = f"```\n<tool_call>\n```\n{SEND}"
    assert len(extract_tool_calls(answer)) == 1
    assert check(answer, ran=1) == []


def test_a_wrapped_call_is_not_dispatched_and_the_top_level_one_runs():
    answer = f"<tool_call>{SEND}</tool_call>\n{SEND}"
    assert len(extract_tool_calls(answer)) == 1


def test_an_empty_wrapper_is_reported_by_name_not_only_reminded():
    reports = check(
        "<tool_call>\n<function_calls>\n<function_calls>\n</function_calls>", ran=0
    )
    assert reports and "holds no call" in reports[0]


def test_a_wrapped_closed_call_ends_not_ok_and_is_not_matched():
    splitter = PartSplitter("t")
    events = splitter.feed(f"<tool_call>{SEND}</tool_call>")
    events += splitter.finish()
    ends = [e for e in events if e[0] == "end" and e[1]["kind"] == "call"]
    assert ends and all(e[2].get("ok") is False for e in ends)
    assert splitter.closed_calls == []


def test_a_call_inside_think_is_not_extracted_and_one_after_it_is():
    splitter = PartSplitter("t")
    events = splitter.feed(f"{THINK_OPEN}{SEND}{THINK_CLOSE}\n{DONE}")
    events += splitter.finish()
    calls = [e for e in events if e[0] == "end" and e[1]["kind"] == "call"]
    assert len(calls) == 1
    assert calls[0][1]["name"] == "done"
    assert len(splitter.closed_calls) == 1
