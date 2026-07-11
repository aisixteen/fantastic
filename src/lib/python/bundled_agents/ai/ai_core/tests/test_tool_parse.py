"""tool_parse — the retained non-streaming parser for the context-reaction
read-model (`core._derive_reaction`). The streaming parser + the agentic loop
are gone: tool dispatch is now the emergent mind's job, not the backend's."""

from __future__ import annotations

import pytest

from ai_core.tool_parse import extract_tool_calls, parse_one


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


# ─── extract (non-streaming, for the history reader) ────────────


def test_extract_tool_calls():
    text = (
        "doing it "
        '<tool_call>{"name":"send","arguments":{"target_id":"self","payload":{"type":"recall"}}}</tool_call>'
        " and "
        '<tool_call>{"name":"send","arguments":{"target_id":"mem","payload":{"type":"set"}}}</tool_call>'
    )
    calls = extract_tool_calls(text)
    assert [c["arguments"]["payload"]["type"] for c in calls] == ["recall", "set"]


def test_extract_ignores_unterminated_and_malformed():
    assert extract_tool_calls("<tool_call>{unterminated") == []
    assert extract_tool_calls("<tool_call>{bad}</tool_call>") == []


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-v"]))
