"""The ONE call grammar: `<call name="…">` with bare child tags, args inline, `bytes`
verbatim, no JSON anywhere. Parse or refuse with the fault named — never repair."""

from __future__ import annotations

from ai_core.tool_parse import (
    _defang,
    extract_tool_calls,
    malformed_calls,
    render_call,
)

WRITE = """<call name="send">
<target_id>notes</target_id>
<reply_to>self</reply_to>
<title>write the report</title>
<type>write</type>
<path>report.md</path>
<bytes>PRJ-1BCA4F

# Report
- the phrase "quick brown fox", verbatim
</bytes>
</call>"""

PATCH = """<call name="send">
<target_id>notes</target_id>
<type>patch</type>
<edits><item><path>hello.py</path><hunks>2</hunks></item></edits>
<bytes><<<<<<< SEARCH
    msg = "Hello, " + name
=======
    return f"Hello, {name}!"
>>>>>>> REPLACE
</bytes>
</call>"""


def _one(text):
    calls = extract_tool_calls(text)
    assert len(calls) == 1, (calls, malformed_calls(text))
    return calls[0]


# ── the taught shapes (the probe's fixtures, 12/12 on qwen3.8-27b) ───────────


def test_write_args_inline_routing_out_bytes_verbatim():
    c = _one(WRITE)
    assert c["name"] == "send" and c["id"].startswith("call_")
    a = c["arguments"]
    assert a["target_id"] == "notes" and a["reply_to"] == "self"
    assert a["title"] == "write the report"
    assert a["payload"] == {
        "type": "write",
        "path": "report.md",
        "bytes": b'PRJ-1BCA4F\n\n# Report\n- the phrase "quick brown fox", verbatim\n',
    }


def test_patch_nested_list_and_typed_hunks():
    p = _one(PATCH)["arguments"]["payload"]
    assert p["type"] == "patch"
    assert p["edits"] == [{"path": "hello.py", "hunks": 2}]
    assert p["bytes"].startswith(b"<<<<<<< SEARCH\n    msg = ")


def test_json_looking_content_rides_untouched():
    text = (
        '<call name="send"><target_id>notes</target_id><type>write</type>'
        '<path>c.json</path><bytes>{"name": "demo"}\n{"tags": ["a", "b"]}\n</bytes></call>'
    )
    assert _one(text)["arguments"]["payload"]["bytes"] == (
        b'{"name": "demo"}\n{"tags": ["a", "b"]}\n'
    )


def test_lists_are_item_children_any_count():
    text = (
        '<call name="send"><target_id>me</target_id><type>grep</type>'
        "<query>marker</query><ids><item>12</item><item>15</item></ids></call>"
    )
    assert _one(text)["arguments"]["payload"]["ids"] == [12, 15]
    one = text.replace("<item>15</item>", "")
    assert _one(one)["arguments"]["payload"]["ids"] == [12]
    allowed = (
        '<call name="send"><target_id>kernel</target_id><type>update_agent</type>'
        "<id>worker</id><allowed><item>python3 \\S+</item></allowed></call>"
    )
    assert _one(allowed)["arguments"]["payload"]["allowed"] == ["python3 \\S+"]


def test_object_with_one_field_is_not_a_list():
    text = (
        '<call name="send"><target_id>notes</target_id><type>patch</type>'
        "<edit><path>a.py</path></edit><bytes>x</bytes></call>"
    )
    assert _one(text)["arguments"]["payload"]["edit"] == {"path": "a.py"}


def test_scalars_typed_by_the_one_rule():
    text = (
        '<call name="send"><target_id>me</target_id><type>pull</type>'
        "<offset>0</offset><length>1.5</length><checkpoint>true</checkpoint>"
        "<why>keep</why><empty></empty></call>"
    )
    p = _one(text)["arguments"]["payload"]
    assert p == {
        "type": "pull",
        "offset": 0,
        "length": 1.5,
        "checkpoint": True,
        "why": "keep",
        "empty": "",
    }


def test_done_and_inquiry_answer():
    c = _one('<call name="done"></call>')
    assert c["name"] == "done" and c["arguments"] == {}
    c = _one('<call name="inquiry_answer"><answer>yes, "both"</answer></call>')
    assert c["name"] == "inquiry_answer" and c["arguments"] == {"answer": 'yes, "both"'}


# ── verbatim mode ─────────────────────────────────────────────────────────────


def test_a_closing_call_tag_inside_bytes_does_not_end_the_call():
    text = (
        '<call name="send"><target_id>notes</target_id><type>write</type>'
        "<path>a.md</path><bytes>the text says </call> and goes on\n</bytes>"
        "<checkpoint>true</checkpoint></call>"
    )
    p = _one(text)["arguments"]["payload"]
    assert (
        p["bytes"] == b"the text says </call> and goes on\n" and p["checkpoint"] is True
    )


def test_bytes_is_never_trimmed_and_other_values_are():
    text = (
        '<call name="send"><target_id>notes</target_id><type>write</type>'
        "<path>  a.md  </path><bytes>  two spaces \n</bytes></call>"
    )
    p = _one(text)["arguments"]["payload"]
    assert p["path"] == "a.md" and p["bytes"] == b"  two spaces \n"


def test_unterminated_call_at_the_end_is_still_an_attempt():
    text = '<call name="send"><target_id>notes</target_id><type>reflect</type>'
    assert _one(text)["arguments"]["payload"] == {"type": "reflect"}


def test_several_calls_in_order_and_a_call_inside_think():
    text = f"<think>first {WRITE}</think>then\n{PATCH}\n"
    calls = extract_tool_calls(text)
    assert [c["arguments"]["payload"]["type"] for c in calls] == ["write", "patch"]


def test_a_mention_in_prose_is_stepped_over_not_reported():
    text = f"I will emit a <call soon. {PATCH}"
    assert len(extract_tool_calls(text)) == 1 and malformed_calls(text) == []
    assert extract_tool_calls("a <caller> is not an envelope") == []


# ── refusals: fault named, shape shown, nothing repaired ─────────────────────


def _refused(text, *needles):
    assert extract_tool_calls(text) == []
    bad = malformed_calls(text)
    assert len(bad) == 1, bad
    err = bad[0]["error"]
    for n in needles:
        assert n in err, (n, err)
    assert err.startswith("call NOT dispatched: ") and "The shape:" in err
    assert "<call" not in bad[0]["your_block"], "the echoed block is defanged"
    return bad[0]


def test_bytes_closer_inside_a_document_ends_the_value_early_and_is_refused():
    text = (
        '<call name="send"><target_id>notes</target_id><type>write</type>'
        "<path>a.md</path><bytes>first </bytes> no\nreal end</bytes></call>"
    )
    _refused(text, "text after the last tag", "no")


def test_bytes_never_closed():
    _refused(
        '<call name="send"><target_id>notes</target_id><type>write</type><bytes>open',
        "`<bytes>` opened and never closed",
    )


def test_element_never_closed():
    _refused(
        '<call name="send"><target_id>notes</target_id><type>write<path>a</path></call>',
        "`<type>` opened and never closed",
    )


def test_duplicate_scalar_tag():
    _refused(
        '<call name="send"><target_id>n</target_id><path>a</path><path>b</path></call>',
        "duplicate `<path>`",
        "<path><item>",
    )


def test_text_between_tags():
    _refused(
        '<call name="send"><target_id>n</target_id> stray words <type>x</type></call>',
        "text outside a tag",
    )


def test_send_without_target():
    _refused('<call name="send"><type>reflect</type></call>', "a send names its target")


def test_head_with_extra_attribute():
    _refused(
        '<call name="send" id="7"><target_id>n</target_id></call>',
        'the call head must be exactly `name="<tool>"`',
    )


def test_defang_breaks_every_delimiter():
    d = _defang('<call name="x"><bytes>a</bytes></call>')
    assert "<call" not in d and "</call>" not in d and "<bytes>" not in d
    assert "‹call" in d


def test_the_broken_block_is_shown_whole():
    big = "x" * 40_000
    text = f'<call name="send"><target_id>n</target_id><type>write</type><bytes>{big}'
    bad = malformed_calls(text)
    assert len(bad) == 1 and big in bad[0]["your_block"]


def test_render_call_round_trips_the_parser_shape():
    args = {
        "target_id": "notes",
        "reply_to": "self",
        "title": "t",
        "payload": {
            "type": "patch",
            "edits": [{"path": "a.py", "hunks": 2}, {"path": "b.py", "hunks": 1}],
            "ids": [3],
            "dry_run": True,
            "bytes": b'x = "1"\n</call>\n',
        },
    }
    text = render_call("send", args)
    assert _one(text)["arguments"] == args
    assert _one(render_call("done"))["arguments"] == {}
    assert _one(render_call("inquiry_answer", {"answer": "ok"}))["arguments"] == {
        "answer": "ok"
    }


def test_grant_maps_keyed_by_ids_and_relative_tokens_are_tags():
    text = (
        '<call name="send"><target_id>kernel</target_id><type>update_agent</type>'
        "<id>vault</id><callable_by><$self>*</$self><$parent>*</$parent>"
        "<ai-rev.2>*</ai-rev.2><notes><item>read</item><item>write</item></notes>"
        "</callable_by></call>"
    )
    p = _one(text)["arguments"]["payload"]
    assert p["callable_by"] == {
        "$self": "*",
        "$parent": "*",
        "ai-rev.2": "*",
        "notes": ["read", "write"],
    }
    assert (
        _one(render_call("send", {"target_id": "k", "payload": p}))["arguments"][
            "payload"
        ]
        == p
    )
