"""The ONE call grammar: `<call name="…">` with bare child tags, args inline, `bytes`
verbatim, no JSON anywhere. Parse or refuse with the fault named — never repair."""

from __future__ import annotations

from ai_core.tool_parse import (
    OPEN,
    _HINT,
    _defang,
    extract_tool_calls,
    malformed_calls,
    render_call,
    render_reply,
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


def test_a_call_written_inside_bytes_is_document_not_envelope():
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


def test_an_unterminated_call_is_not_a_call():
    """No `</call>`, no call: the model stopped generating before
    it closed — nothing dispatches, nothing is "malformed"; the cut marker names it."""
    text = '<call name="send"><target_id>notes</target_id><type>reflect</type>'
    assert extract_tool_calls(text) == [] and malformed_calls(text) == []


def test_several_calls_in_order_and_a_call_inside_think():
    text = f"<think>first {WRITE}</think>then\n{PATCH}\n"
    calls = extract_tool_calls(text)
    # A call inside a thinking block is not extracted (Rust #189); the one after it is.
    assert [c["arguments"]["payload"]["type"] for c in calls] == ["patch"]


def test_a_good_turn_reports_nothing():
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


def test_bytes_never_closed_is_a_cut_not_a_call():
    # an open `<bytes>` swallows any `</call>`, so the envelope never closes
    text = '<call name="send"><target_id>notes</target_id><type>write</type><bytes>open</call>'
    assert extract_tool_calls(text) == [] and malformed_calls(text) == []


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
    text = (
        f'<call name="send"><target_id>n</target_id><type>write</type><bytes>{big}</bytes>'
        "<path>a</path><path>b</path></call>"
    )
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


def test_a_send_may_name_several_recipients_with_to():
    text = (
        '<call name="send"><to><item>solver_2</item><item>solver_3</item></to>'
        "<re>412</re><type>send</type><text>CANDIDATE 586</text></call>"
    )
    a = _one(text)["arguments"]
    assert a["to"] == ["solver_2", "solver_3"] and a["re"] == 412
    assert "target_id" not in a and a["payload"] == {
        "type": "send",
        "text": "CANDIDATE 586",
    }
    assert _one(render_call("send", a))["arguments"] == a


def test_a_send_with_neither_target_nor_to_is_refused_naming_both():
    _refused(
        '<call name="send"><type>reflect</type></call>', "<target_id>", "<to><item>"
    )


def test_await_is_a_control_call():
    c = _one('<call name="await"></call>')
    assert c["name"] == "await" and c["arguments"] == {}


def test_item_beside_other_tags_is_a_field():
    call = _one(
        '<call name="send"><target_id>user</target_id><type>inquiry</type>'
        "<action><payload><type>buy</type><item>rocket</item></payload></action></call>"
    )
    assert call["arguments"]["payload"]["action"]["payload"] == {
        "type": "buy",
        "item": "rocket",
    }


_SEND_AND_AWAIT = (
    '<call name="send"><target_id>exec</target_id><type>tail</type>'
    "<text>go</text></call>\n"
    '<call name="await"></call>\n'
)
# twin: a_trailing_fence_after_the_calls_leaves_them_dispatchable
# twin: a_leading_invoke_fragment_leaves_the_valid_calls_after_it
# twin: a_leading_invoke_fragment_and_a_trailing_fence_together


def test_a_trailing_fence_and_a_leading_invoke_fragment_leave_calls_dispatchable():
    from ai_core import call_check

    for text in (
        _SEND_AND_AWAIT + "```\n",
        '<invoke name="x"></invoke>\n' + _SEND_AND_AWAIT.rstrip("\n"),
        '<invoke name="x"></invoke>\n' + _SEND_AND_AWAIT + "```\n",
    ):
        calls = extract_tool_calls(text)
        assert [c["name"] for c in calls] == ["send", "await"], calls
        assert malformed_calls(text) == []
        assert call_check.check(text, ran=2) == []


def test_a_reply_body_reads_as_utf8_text_at_its_slot():
    from ai_core.tool_parse import render_reply

    read = {
        "path": "a.txt",
        "offset": 0,
        "bytes": b"line one\nline \xff two",
        "eof": True,
    }
    assert render_reply(read) == (
        "<path>a.txt</path>\n<offset>0</offset>\n<bytes>\nline one\nline \ufffd two\n"
        "</bytes>\n<eof>true</eof>"
    )
    nested = {"seq": 3, "out": {"chunk": b"red \xff"}}
    assert (
        render_reply(nested) == "<seq>3</seq>\n<out>\n<chunk>red \ufffd</chunk>\n</out>"
    )
    # a body-less reply is unchanged
    assert render_reply({"seq": 3}) == "<seq>3</seq>"


# ── twins of the Rust tool_parse tests (#170) ────────────────────────────────


def test_a_dict_is_one_line_per_field():
    assert (
        render_reply({"path": "a.md", "size": 12, "ok": True})
        == "<path>a.md</path>\n<size>12</size>\n<ok>true</ok>"
    )


def test_a_nested_dict_opens_on_its_own_line():
    assert (
        render_reply({"agent": {"id": "notes", "verbs": "read"}})
        == "<agent>\n<id>notes</id>\n<verbs>read</verbs>\n</agent>"
    )


def test_a_top_level_list_is_items():
    assert render_reply(["a", "b"]) == "<items>\n<item>a</item>\n<item>b</item>\n</items>"


def test_a_list_of_structures_nests_and_an_empty_list_takes_that_branch():
    assert (
        render_reply({"lines": [{"n": 1}, {"n": 2}]})
        == "<lines>\n<item>\n<n>1</n>\n</item>\n<item>\n<n>2</n>\n</item>\n</lines>"
    )
    assert render_reply({"lines": []}) == "<lines>\n</lines>"


def test_a_multiline_scalar_spans_lines_verbatim():
    assert render_reply({"text": "one\ntwo"}) == "<text>\none\ntwo\n</text>"


def test_null_is_an_empty_tag_and_a_bare_scalar_is_its_text():
    assert render_reply({"next": None}) == "<next></next>"
    assert render_reply("just text") == "just text"
    assert render_reply(False) == "false"


def test_nothing_is_escaped():
    assert (
        render_reply({"content": 'say "hi"\nand \\ that'})
        == '<content>\nsay "hi"\nand \\ that\n</content>'
    )


def test_render_call_nests_tighter_than_render_reply():
    args = {"target_id": "notes", "payload": {"type": "read", "path": "a.md"}}
    assert render_call("send", args) == (
        '<call name="send">\n<target_id>notes</target_id>\n<type>read</type>\n'
        "<path>a.md</path>\n</call>"
    )


def test_reply_scalars_are_python_str():
    assert render_reply(1e-5) == "1e-05"
    assert render_reply(1e16) == "1e+16"
    assert render_reply(0.5) == "0.5"
    assert render_reply(3.0) == "3.0"
    assert render_reply(None) == ""
    assert render_reply(True) == "true"
    assert (
        render_reply(["a", {"a": 1}])
        == "<items>\n<item>a</item>\n<item>\n<a>1</a>\n</item>\n</items>"
    )
    assert (
        render_reply({"v": {"a": 1, "b": [True, None]}})
        == "<v>\n<a>1</a>\n<b>\n<item>true</item>\n<item></item>\n</b>\n</v>"
    )


def test_the_hint_is_verbatim():
    assert _HINT == (
        'The shape: <call name="send">\n'
        "<target_id>agent id</target_id>\n"
        "<type>verb</type>\n"
        "<path>a.md</path>\n"
        "<bytes>document, verbatim, to the first </bytes>\n"
        "</bytes>\n"
        "</call> — one child tag per argument, a list is one tag whose children "
        "are all <item>, nothing is escaped."
    )


def test_the_report_is_three_fields_and_one_composed_sentence():
    text = '<call name="send"><target_id>n</target_id><path>a</path><path>b</path></call>'
    bad = malformed_calls(text)
    assert len(bad) == 1
    assert set(bad[0]) == {"error", "envelope", "your_block"}
    assert bad[0]["envelope"] == OPEN
    assert bad[0]["error"] == (
        "call NOT dispatched: duplicate `<path>` — a list is one tag whose "
        "children are all `<item>`: `<path><item>…</item><item>…</item></path>`. "
        "Nothing ran. " + _HINT
    )
    assert (
        bad[0]["your_block"]
        == 'name="send"><target_id>n</target_id><path>a</path><path>b</path>'
    )


def test_the_body_delivered_to_the_model_is_plain_text():
    bad = malformed_calls('<call name="send"><type>reflect</type></call>')[0]
    # the body core builds for the model (core.py: error + your block)
    body = f"{bad['error']}\n\nyour block:\n{bad['your_block']}"
    assert body.startswith("call NOT dispatched: a send names its target")
    assert "\n\nyour block:\n" in body
    assert not body.startswith("{")


def test_every_broken_envelope_gets_its_own_report():
    text = (
        '<call name="send"><type>reflect</type></call>\n'
        '<call name="nope"><target_id>n</target_id></call>'
    )
    bad = malformed_calls(text)
    assert len(bad) == 2
    assert "a send names its target" in bad[0]["error"]
    assert "`nope` is not a tool" in bad[1]["error"]


def test_a_huge_numeric_target_is_refused():
    for tid in ("123456789012345678901234567890", "42"):
        _refused(
            f'<call name="send"><target_id>{tid}</target_id><type>reflect</type></call>',
            "a send names its target",
        )


def test_a_truthy_non_list_to_is_kept_raw():
    calls = extract_tool_calls('<call name="send"><to>42</to><type>reflect</type></call>')
    assert len(calls) == 1, calls
    assert calls[0]["arguments"]["to"] == 42
    assert "target_id" not in calls[0]["arguments"]
    bad = malformed_calls('<call name="send"><to>0</to><type>reflect</type></call>')
    assert len(bad) == 1, bad
    assert "a send names its target" in bad[0]["error"]
