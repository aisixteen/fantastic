"""`bytes` is the one verbatim field: cut out of the call before parsing, every
occurrence, anywhere in the payload, real newlines and quotes inside, never escaped,
never repaired."""

from __future__ import annotations

from ai_core.tool_parse import extract_tool_calls, malformed_calls

CALL = (
    '<tool_call>{"name":"send","arguments":{"target_id":"notes","payload":'
    '{"type":"write","path":"a.md","bytes":"%s"}}}</tool_call>'
)


def _bytes(text):
    calls = extract_tool_calls(text)
    assert len(calls) == 1, calls
    return calls[0]["arguments"]["payload"]["bytes"]


def test_newlines_and_quotes_ride_verbatim_as_bytes():
    body = 'say "hi" now\nline 2 with {braces} and a lone " quote here\n'
    assert _bytes(CALL % body) == body.encode("utf-8")


def test_the_field_can_sit_anywhere_and_later_fields_survive():
    text = (
        '<tool_call>{"name":"send","arguments":{"target_id":"notes","payload":'
        '{"type":"write","bytes":"x = "y" + 1\n","path":"b.py","checkpoint":true},'
        '"title":"write b"}}</tool_call>'
    )
    calls = extract_tool_calls(text)
    p = calls[0]["arguments"]["payload"]
    assert (
        p["bytes"] == b'x = "y" + 1\n'
        and p["path"] == "b.py"
        and p["checkpoint"] is True
    )
    assert calls[0]["arguments"]["title"] == "write b"


def test_escapes_are_not_decoded_the_model_writes_what_lands():
    assert _bytes(CALL % "a\\nb") == b"a\\nb"


def test_a_quote_followed_by_a_delimiter_inside_the_text_is_the_one_thing_refused():
    body = 'first "}}} no\nreal end'
    text = CALL % body
    assert extract_tool_calls(text) == []
    bad = malformed_calls(text)
    assert (
        bad
        and "ENDS at the first quote followed by a JSON delimiter" in bad[0]["error"]
    )


def test_a_patch_with_two_bytes_fields_cuts_both_in_order():
    text = (
        '<tool_call>{"name":"send","arguments":{"target_id":"notes","payload":{"type":"patch","edits":['
        '{"path":"a.py","bytes":"<<<<<<< SEARCH\nx = 1\n=======\nx = 2\n>>>>>>> REPLACE\n"},'
        '{"path":"b.py","bytes":"<<<<<<< SEARCH\ny = "q"\n=======\ny = "r"\n>>>>>>> REPLACE\n"}]}}}</tool_call>'
    )
    calls = extract_tool_calls(text)
    edits = calls[0]["arguments"]["payload"]["edits"]
    assert (
        edits[0]["bytes"] == b"<<<<<<< SEARCH\nx = 1\n=======\nx = 2\n>>>>>>> REPLACE\n"
    )
    assert (
        edits[1]["bytes"]
        == b'<<<<<<< SEARCH\ny = "q"\n=======\ny = "r"\n>>>>>>> REPLACE\n'
    )
    assert edits[1]["path"] == "b.py"


def test_no_closing_quote_is_malformed_and_told_the_rule():
    text = (
        '<tool_call>{"name":"send","arguments":{"target_id":"notes","payload":'
        '{"type":"write","path":"a.md","bytes":"never closed</tool_call>'
    )
    assert extract_tool_calls(text) == []
    bad = malformed_calls(text)
    assert (
        bad and "could not be cut" in bad[0]["error"] and "verbatim" in bad[0]["error"]
    )


def test_a_call_without_bytes_is_untouched():
    text = (
        '<tool_call>{"name":"send","arguments":{"target_id":"notes","payload":'
        '{"type":"read","path":"a.md"}}}</tool_call>'
    )
    assert extract_tool_calls(text)[0]["arguments"]["payload"] == {
        "type": "read",
        "path": "a.md",
    }


def test_a_call_one_brace_short_is_told_about_its_json_not_the_cut():
    text = (
        '<tool_call>{"name":"send","arguments":{"target_id":"notes","payload":'
        '{"type":"write","path":"a.md","bytes":"fine text\n"}}</tool_call>'
    )
    assert extract_tool_calls(text) == []
    bad = malformed_calls(text)
    assert (
        bad
        and "does not parse" in bad[0]["error"]
        and "closing brace" in bad[0]["error"]
    )
    assert "could not be cut" not in bad[0]["error"]
