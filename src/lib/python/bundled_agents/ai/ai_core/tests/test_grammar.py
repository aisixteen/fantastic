"""The grammar helpers: envelopes, wrappers, and the code that is not markup.

Ported from Rust's `tests_grammar.rs` (#189), the grammar half. Delimiters are
built by concatenation so this file never types the opener or the document closer
literally — the checker would read them as a call.
"""

from __future__ import annotations

from ai_core.grammar import (
    CLOSE,
    OPEN,
    Envelope,
    OpenTag,
    blank_fences,
    blank_thinking,
    code_masked,
    envelopes,
    envelopes_in,
    find_close,
    open_tags,
    track,
)

BYTES_OPEN = "<" + "bytes" + ">"
BYTES_CLOSE = "</" + "bytes" + ">"
THINK_OPEN = "<" + "think" + ">"
THINK_CLOSE = "</" + "think" + ">"

WRITE = (
    OPEN + ' name="send">\n'
    "<target_id>notes</target_id>\n"
    "<reply_to>self</reply_to>\n"
    "<title>write the report</title>\n"
    "<type>write</type>\n"
    "<path>report.md</path>\n"
    + BYTES_OPEN
    + 'PRJ-1BCA4F\n\n# Report\n- the phrase "quick brown fox", verbatim\n'
    + BYTES_CLOSE
    + "\n"
    + CLOSE
)

PATCH = (
    OPEN + ' name="send">\n'
    "<target_id>notes</target_id>\n"
    "<type>patch</type>\n"
    "<edits><item><path>hello.py</path><hunks>2</hunks></item></edits>\n"
    + BYTES_OPEN
    + '<<<<<< SEARCH\n    msg = "Hello, " + name\n=======\n'
    '    return f"Hello, {name}!"\n>>>>>>>> REPLACE\n' + BYTES_CLOSE + "\n" + CLOSE
)


def test_a_mention_is_not_an_envelope():
    assert envelopes("a " + OPEN + "er> is not an envelope") == []


def test_a_mention_in_prose_is_stepped_over_not_swallowing_the_call():
    text = "I will emit a " + OPEN + " soon. " + PATCH
    spans = envelopes(text)
    assert len(spans) == 1
    assert spans[0][0] == text.index(OPEN, text.index("soon."))
    assert spans[0][2] is not None


def test_a_call_inside_a_wrapper_is_reported_with_that_wrapper():
    text = "<tool_call>" + OPEN + ' name="done">' + CLOSE
    envs = envelopes_in(text)
    assert len(envs) == 1
    assert envs[0].wrapper == OpenTag(0, 11, "tool_call")
    assert envs[0].start == 11
    assert envs[0].body == 11 + len(OPEN)
    assert envs[0].end == text.index(CLOSE)


def test_a_wrapper_tag_may_carry_attributes():
    text = '<tool_call id="7">' + WRITE
    wrapper = envelopes_in(text)[0].wrapper
    assert wrapper is not None and wrapper.name == "tool_call"


def test_a_tag_closed_before_the_call_wraps_nothing():
    text = "<b>x</b> " + WRITE
    assert envelopes_in(text)[0].wrapper is None


# twin: prose_angle_brackets_that_are_not_tags_do_not_wrap_a_call
def test_a_bare_word_tag_is_not_a_wrapper():
    text = "a<b and c>d\n" + WRITE
    assert envelopes_in(text)[0].wrapper is None


def test_the_outermost_open_tag_is_the_wrapper():
    text = "<outer><outer>" + OPEN + ' name="done">' + CLOSE
    wrapper = envelopes_in(text)[0].wrapper
    assert wrapper is not None and wrapper.start == 0


def test_a_backtick_in_thinking_does_not_hide_the_think_close():
    text = (
        "\n"
        + THINK_OPEN
        + "maybe `x"
        + THINK_CLOSE
        + "\n"
        + OPEN
        + ' name="send"><target_id>self</target_id><type>recall</type>'
        + CLOSE
        + " then `y`"
    )
    envs = envelopes_in(text)
    assert len(envs) == 1
    assert envs[0].wrapper is None


def test_code_masked_blanks_spans_and_fences_and_keeps_offsets():
    text = "a `<b>` c\n```html\n<div>\n```\nd ``x`y`` e ` f <i>"
    masked = code_masked(text)
    assert len(masked) == len(text)
    assert masked.count("\n") == text.count("\n"), "newlines are kept"
    assert "<b>" not in masked and "<div>" not in masked, masked
    assert "<i>" in masked, "an unmatched backtick hides nothing"


def test_a_code_span_does_not_cross_a_blank_line():
    text = "a ` b\n\n<tool_call> `c`"
    assert "<tool_call>" in code_masked(text)


def test_an_unclosed_fence_blanks_to_the_end():
    text = "```python\n<div>\nmore"
    masked = blank_fences(text)
    assert "<div>" not in masked
    assert len(masked) == len(text)
    assert masked.endswith("    ")


def test_blank_thinking_blanks_the_body_and_keeps_the_tags():
    text = THINK_OPEN + "maybe `x" + THINK_CLOSE + "\n" + OPEN + ' name="send">'
    masked = blank_thinking(text)
    assert masked.startswith(THINK_OPEN)
    assert THINK_CLOSE in masked
    assert "`x" not in masked
    assert len(masked) == len(text)


def test_find_close_skips_a_closer_inside_a_bytes_document():
    text = (
        OPEN + ' name="send"><target_id>n</target_id><type>write</type>'
        "<path>a.md</path>"
        + BYTES_OPEN
        + "the text says "
        + CLOSE
        + " and goes on\n"
        + BYTES_CLOSE
        + CLOSE
    )
    assert find_close(text, len(OPEN)) == text.rindex(CLOSE)


def test_an_unterminated_envelope_has_no_end():
    text = OPEN + ' name="send"><target_id>n</target_id><type>reflect</type>'
    assert envelopes(text) == [(0, len(OPEN), None)]
    assert find_close(text, len(OPEN)) is None


def test_envelopes_in_blanks_the_envelope_it_found():
    text = WRITE + "\n<tool_call>"
    envs = envelopes_in(text)
    assert len(envs) == 1 and envs[0].wrapper is None


def test_track_pushes_and_pops_to_the_last_open_of_its_name():
    text = "<a><b><c></b></a>"
    seen: list[OpenTag] = []
    track(text, 0, 9, seen)
    assert [t.name for t in seen] == ["a", "b", "c"]
    assert open_tags(text) == []


def test_a_close_pops_everything_open_after_it():
    seen: list[OpenTag] = []
    track("<a><b><c></b>", 0, 13, seen)
    assert [t.name for t in seen] == ["a"]


def test_a_stray_close_is_ignored():
    assert open_tags("</b><a>") == [OpenTag(4, 7, "a")]


def test_self_closing_does_not_open():
    assert open_tags("<br/>") == []
# twin: a_call_after_a_type_in_inline_code_runs_with_no_report
# twin: a_code_span_is_not_an_open_wrapper_in_the_report


def test_open_tags_excludes_code():
    assert open_tags("a `<b>` c") == []


def test_envelope_is_a_dataclass_of_spans():
    env = Envelope(0, 5, 10, None)
    assert env == Envelope(0, 5, 10, None)
    assert env.wrapper is None


def test_prose_mention_does_not_swallow_a_real_call():
    text = 'the <call names are listed> and <call name="send">x</call>'
    spans = envelopes(text)
    assert [s for s, _, _ in spans] == [text.index('<call name="send">')]
