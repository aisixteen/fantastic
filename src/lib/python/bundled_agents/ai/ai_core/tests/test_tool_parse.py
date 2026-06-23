"""tool_parse — the shared RAW tool-call parser (no native provider tool API)."""

from __future__ import annotations

import pytest

from ai_core.tool_parse import (
    extract_tool_calls,
    parse_one,
    render_tool_call,
    stream_tool_calls,
)


async def _aiter(items):
    for it in items:
        yield it


async def _drain(chunks):
    """Run the parser over a list of raw text chunks → (content_str, tool_calls)."""
    content: list[str] = []
    calls: list[dict] = []
    async for ev in stream_tool_calls(_aiter(chunks)):
        if isinstance(ev, str):
            content.append(ev)
        else:
            calls.append(ev["tool_call"])
    return "".join(content), calls


# ─── parse_one (lenient JSON → call) ────────────────────────────


def test_parse_one_canonical():
    c = parse_one(
        '{"name": "send", "arguments": {"target_id": "core", "payload": {"type": "list_agents"}}}'
    )
    assert c["name"] == "send"
    assert c["arguments"] == {"target_id": "core", "payload": {"type": "list_agents"}}
    assert c["id"].startswith("call_")


def test_parse_one_flattened_and_tool_alias():
    # tiny models drift: `tool` alias for name, flattened args
    c = parse_one(
        '{"tool": "send", "target_id": "foo", "payload": {"type": "reflect"}}'
    )
    assert c["name"] == "send"
    assert c["arguments"] == {"target_id": "foo", "payload": {"type": "reflect"}}


def test_parse_one_stringified_arguments():
    c = parse_one('{"name": "send", "arguments": "{\\"target_id\\": \\"x\\"}"}')
    assert c["arguments"] == {"target_id": "x"}


def test_parse_one_malformed_returns_none():
    assert parse_one("{not json") is None
    assert parse_one("[1,2,3]") is None


# ─── streaming ──────────────────────────────────────────────────


async def test_plain_text_no_calls():
    content, calls = await _drain(["Hello ", "world"])
    assert content == "Hello world"
    assert calls == []


async def test_single_call_clean():
    chunk = '<tool_call>{"name": "send", "arguments": {"target_id": "core", "payload": {"type": "list_agents"}}}</tool_call>'
    content, calls = await _drain([chunk])
    assert content == ""
    assert len(calls) == 1
    assert calls[0]["arguments"]["target_id"] == "core"
    assert calls[0]["arguments"]["payload"]["type"] == "list_agents"


async def test_prose_then_call():
    content, calls = await _drain(
        [
            "Let me check. ",
            '<tool_call>{"name":"send","arguments":{"target_id":"core","payload":{"type":"reflect"}}}</tool_call>',
        ]
    )
    assert content == "Let me check. "
    assert len(calls) == 1


async def test_tag_split_across_chunks():
    full = '<tool_call>{"name":"send","arguments":{"target_id":"core","payload":{"type":"list_agents"}}}</tool_call>'
    # split into single-char chunks — the worst case for buffering
    content, calls = await _drain(list(full))
    assert content == ""
    assert len(calls) == 1
    assert calls[0]["arguments"]["target_id"] == "core"


async def test_multiple_calls_one_stream():
    a = '<tool_call>{"name":"send","arguments":{"target_id":"a","payload":{"type":"reflect"}}}</tool_call>'
    b = '<tool_call>{"name":"send","arguments":{"target_id":"b","payload":{"type":"reflect"}}}</tool_call>'
    content, calls = await _drain([a, "\n", b])
    assert [c["arguments"]["target_id"] for c in calls] == ["a", "b"]
    assert content.strip() == ""


async def test_malformed_json_surfaces_as_content():
    chunk = "<tool_call>{not json}</tool_call>"
    content, calls = await _drain([chunk])
    assert calls == []
    assert content == chunk  # nothing lost


async def test_unterminated_tag_at_eof_surfaces_as_content():
    content, calls = await _drain(['<tool_call>{"name":"send"'])
    assert calls == []
    assert content.startswith("<tool_call>")


async def test_lone_angle_bracket_not_held_forever():
    # a `<` that is NOT a tool_call must still surface as content
    content, calls = await _drain(["a < b ", "and c"])
    assert calls == []
    assert content == "a < b and c"


async def test_event_dict_passthrough():
    # a pre-formed event dict (test/structured source) passes through in order
    content, calls = await _drain(
        [
            "hi ",
            {
                "tool_call": {
                    "id": "x",
                    "name": "send",
                    "arguments": {"target_id": "core"},
                }
            },
        ]
    )
    assert content == "hi "
    assert calls[0]["arguments"]["target_id"] == "core"


# ─── extract (non-streaming, for history reader) ────────────────


def test_extract_tool_calls():
    text = (
        "doing it "
        '<tool_call>{"name":"send","arguments":{"target_id":"self","payload":{"type":"recall"}}}</tool_call>'
        " and "
        '<tool_call>{"name":"send","arguments":{"target_id":"mem","payload":{"type":"set"}}}</tool_call>'
    )
    calls = extract_tool_calls(text)
    assert [c["arguments"]["payload"]["type"] for c in calls] == ["recall", "set"]


def test_render_round_trips_through_extract():
    s = render_tool_call(
        "send", {"target_id": "core", "payload": {"type": "list_agents"}}
    )
    calls = extract_tool_calls(s)
    assert calls[0]["arguments"] == {
        "target_id": "core",
        "payload": {"type": "list_agents"},
    }


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-v"]))
