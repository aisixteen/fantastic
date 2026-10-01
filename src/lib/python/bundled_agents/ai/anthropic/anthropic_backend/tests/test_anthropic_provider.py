"""AnthropicProvider — the two channels it must read, and NO native tool route.

Tool-calling in this kernel is RAW: `ai_core` teaches the `<call>` grammar in the
prompt and owns the only parser. These assert the provider never breaks that: it
declares no `tools`, and a native `tool_use` block from the endpoint is a loud
error, never a translation.
"""

from __future__ import annotations

import json

import httpx
import pytest
from ai_core.core import THINK_CLOSE, THINK_OPEN
from anthropic_backend.provider import RETRIES, AnthropicProvider


@pytest.fixture(autouse=True)
def _key(monkeypatch):
    """The key is read before the request is built, so even a MockTransport run
    needs one present. Set here, never read from the developer's real `.env`."""
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-ant-test-fixture")


def _sse(*events: dict) -> str:
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events)


def _text(*chunks: str, stop: str = "end_turn") -> str:
    evts: list[dict] = [
        {"type": "content_block_start", "content_block": {"type": "text"}}
    ]
    for c in chunks:
        evts.append(
            {"type": "content_block_delta", "delta": {"type": "text_delta", "text": c}}
        )
    evts += [
        {"type": "content_block_stop"},
        {"type": "message_delta", "delta": {"stop_reason": stop}},
    ]
    return _sse(*evts)


def _capture(sink: list, sse: str, status: int = 200):
    def handler(request: httpx.Request) -> httpx.Response:
        sink.append(json.loads(request.content))
        return httpx.Response(status, text=sse)

    return httpx.MockTransport(handler)


def _provider(sink: list, sse: str, **kw) -> AnthropicProvider:
    kw.setdefault("model", "claude-test")
    kw.setdefault("endpoint", "https://anthropic.test/v1/messages")
    return AnthropicProvider(transport=_capture(sink, sse), **kw)


async def _drain(p: AnthropicProvider) -> str:
    return "".join([c async for c in p.chat([{"role": "user", "content": "hi"}])])


async def test_thinking_is_wrapped_not_dropped():
    """A reasoning model splits its output; reading `text_delta` alone returns a
    turn spent thinking as an EMPTY completion, killing the chain with no error."""
    sse = _sse(
        {"type": "content_block_start", "content_block": {"type": "thinking"}},
        {
            "type": "content_block_delta",
            "delta": {"type": "thinking_delta", "thinking": "let me "},
        },
        {
            "type": "content_block_delta",
            "delta": {"type": "thinking_delta", "thinking": "think"},
        },
        {"type": "content_block_stop"},
        {"type": "content_block_start", "content_block": {"type": "text"}},
        {
            "type": "content_block_delta",
            "delta": {"type": "text_delta", "text": "answer"},
        },
        {"type": "content_block_stop"},
        {"type": "message_delta", "delta": {"stop_reason": "end_turn"}},
    )
    assert (
        await _drain(_provider([], sse))
        == f"{THINK_OPEN}let me think{THINK_CLOSE}answer"
    )


async def test_all_thought_turn_still_closes_the_wrapper():
    """A turn that never reaches `text_delta` must still balance the marker pair —
    an unterminated block is an unparseable turn downstream."""
    sse = _sse(
        {"type": "content_block_start", "content_block": {"type": "thinking"}},
        {
            "type": "content_block_delta",
            "delta": {"type": "thinking_delta", "thinking": "hm"},
        },
        {"type": "content_block_stop"},
        {"type": "message_delta", "delta": {"stop_reason": "end_turn"}},
    )
    assert await _drain(_provider([], sse)) == f"{THINK_OPEN}hm{THINK_CLOSE}"


async def test_cut_stream_raises_instead_of_faking_a_finish():
    """`max_tokens` mid-envelope used to return as if the model had finished: the
    partial parsed as garbage, the next turn came back empty, the chain died —
    three symptoms, one silent cause. A cut turn is an error the caller can see."""
    with pytest.raises(RuntimeError, match="without end_turn"):
        await _drain(_provider([], _text("half a ", stop="max_tokens")))


async def test_raw_route_sends_no_tools():
    """The default route is RAW: the envelope is taught in the prompt, so the
    request must carry no `tools` key at all."""
    sink: list = []
    await _drain(_provider(sink, _text("hello")))
    assert "tools" not in sink[0]


async def test_tool_use_stop_reason_without_a_tool_use_block_is_an_anomaly():
    """`stop_reason='tool_use'` over nothing but text means the stream did not
    carry what its stop_reason claims — an error on EITHER route. Measured
    2026-08-21: a no-tools request came back `tool_use`, and a bare stop_reason
    could not say what the model had actually emitted, which is why the error
    names the block types."""
    sse = _sse(
        {"type": "content_block_start", "content_block": {"type": "text"}},
        {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "x"}},
        {"type": "content_block_stop"},
        {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
    )
    with pytest.raises(RuntimeError, match=r"blocks=\['text'\]"):
        await _drain(_provider([], sse))


async def test_a_native_tool_use_block_is_a_loud_error_not_a_silent_drop():
    """The `/cc` route converts a model's native tool syntax server-side and returns
    `tool_use` blocks with no `tools` key in the request (measured 2026-08-24). Its
    arguments are JSON this kernel does not read; dropping the block would end the
    turn looking finished with the call lost, so it raises, naming the grammar."""
    sse = _sse(
        {
            "type": "content_block_start",
            "content_block": {"type": "tool_use", "name": "send"},
        },
        {
            "type": "content_block_delta",
            "delta": {
                "type": "input_json_delta",
                "partial_json": '{"target_id": "notes", "payload": {"type": "reflect"}}',
            },
        },
        {"type": "content_block_stop"},
        {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
    )
    sink: list = []
    with pytest.raises(RuntimeError, match=r"native tool_use block.*<call name="):
        await _drain(_provider(sink, sse))
    assert "tools" not in sink[0]


# ── transport retry ──────────────────────────────────────────────────────────
# A 4xx, or any 5xx with a JSON body, is the endpoint answering deliberately and
# must stay loud. A transport code with a NON-JSON body is a CDN error page: the
# request never reached anything that could judge it.


def _sequence(responses: list, sink: list):
    """Replay `responses` in order, recording how many requests were actually made."""

    def handler(request: httpx.Request) -> httpx.Response:
        sink.append(request)
        status, body, ctype = responses[min(len(sink) - 1, len(responses) - 1)]
        return httpx.Response(status, text=body, headers={"content-type": ctype})

    return httpx.MockTransport(handler)


async def test_a_successful_call_makes_exactly_one_request():
    """The retry must not re-issue a request that already succeeded. An earlier
    draft opened the stream once to check the status and again to read it, which
    doubled every call and every token billed."""
    sink: list = []
    p = AnthropicProvider(
        endpoint="https://anthropic.test/v1/messages",
        model="claude-test",
        transport=_sequence([(200, _text("hi"), "text/event-stream")], sink),
    )
    assert await _drain(p) == "hi"
    assert len(sink) == 1


async def test_cloudflare_error_page_is_retried_then_succeeds():
    """521 with an HTML body = the edge, not the endpoint. Measured 2026-08-24: a
    1-2s reverse-proxy restart killed a 26-pass run at pass 5."""
    sink: list = []
    p = AnthropicProvider(
        endpoint="https://anthropic.test/v1/messages",
        model="claude-test",
        transport=_sequence(
            [
                (521, "<!DOCTYPE html>...", "text/html"),
                (200, _text("recovered"), "text/event-stream"),
            ],
            sink,
        ),
    )
    assert await _drain(p) == "recovered"
    assert len(sink) == 2


async def test_transport_error_gives_up_after_the_bound():
    """Bounded, not infinite — a persistently down endpoint still fails loudly."""
    sink: list = []
    p = AnthropicProvider(
        endpoint="https://anthropic.test/v1/messages",
        model="claude-test",
        transport=_sequence([(521, "<!DOCTYPE html>", "text/html")], sink),
    )
    with pytest.raises(RuntimeError, match="anthropic 521"):
        await _drain(p)
    assert len(sink) == RETRIES + 1


async def test_json_error_body_is_never_retried():
    """The proxy answered deliberately. Retrying would hide the caller's bug and
    spend the same broken request N times."""
    sink: list = []
    p = AnthropicProvider(
        endpoint="https://anthropic.test/v1/messages",
        model="claude-test",
        transport=_sequence([(503, '{"type":"error"}', "application/json")], sink),
    )
    with pytest.raises(RuntimeError, match="anthropic 503"):
        await _drain(p)
    assert len(sink) == 1


async def test_client_errors_are_never_retried():
    """A 401 is a statement about the request — the key is wrong. Measured
    2026-08-24: retrying it would have masked a harness sending the aisixteen key
    to api.anthropic.com."""
    sink: list = []
    p = AnthropicProvider(
        endpoint="https://anthropic.test/v1/messages",
        model="claude-test",
        transport=_sequence([(401, "<!DOCTYPE html>", "text/html")], sink),
    )
    with pytest.raises(RuntimeError, match="anthropic 401"):
        await _drain(p)
    assert len(sink) == 1


# ─── the wire, pinned (reduction candidate A: a shared retry POST + SSE reader)


async def _drain_msgs(p: AnthropicProvider, messages, **kw) -> str:
    return "".join([c async for c in p.chat(messages, **kw)])


async def test_reported_usage_sums_input_and_both_cache_counts():
    sse = _sse(
        {
            "type": "message_start",
            "message": {
                "usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": 200,
                    "cache_creation_input_tokens": 3000,
                    "output_tokens": 1,
                }
            },
        },
        {"type": "content_block_start", "content_block": {"type": "text"}},
        {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "x"}},
        {"type": "message_delta", "delta": {"stop_reason": "end_turn"}},
    )
    p = _provider([], sse)
    assert await _drain(p) == "x"
    assert p.last_prompt_tokens == 3210
    assert p.last_stop_reason == "end_turn"


async def test_a_zero_usage_reports_none():
    sse = _sse(
        {"type": "message_start", "message": {"usage": {"input_tokens": 0}}},
        {"type": "message_delta", "delta": {"stop_reason": "end_turn"}},
    )
    p = _provider([], sse)
    await _drain(p)
    assert p.last_prompt_tokens is None


async def test_an_error_event_raises_with_its_body():
    sse = _sse({"type": "error", "error": {"type": "overloaded_error"}})
    with pytest.raises(RuntimeError) as e:
        await _drain(_provider([], sse))
    assert str(e.value) == "anthropic stream error: {'type': 'overloaded_error'}"


async def test_blank_and_non_json_data_lines_are_skipped():
    body = "data: \n\ndata: not-json\n\n" + _text("ok")
    assert await _drain(_provider([], body)) == "ok"


async def test_a_cut_connection_names_no_stop_reason():
    sse = _sse(
        {"type": "content_block_start", "content_block": {"type": "text"}},
        {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "x"}},
    )
    with pytest.raises(RuntimeError) as e:
        await _drain(_provider([], sse))
    assert str(e.value) == (
        "anthropic stream ended without end_turn: stop_reason=None "
        "blocks=['text'] (none seen — connection cut mid-stream)"
    )


async def test_a_main_turn_cut_text_and_stop_sequence_is_terminal():
    with pytest.raises(RuntimeError) as e:
        await _drain(_provider([], _text("half", stop="max_tokens")))
    assert str(e.value) == (
        "anthropic stream ended without end_turn: stop_reason='max_tokens' "
        "blocks=['text']"
    )
    p = _provider([], _text("done", stop="stop_sequence"))
    assert await _drain(p) == "done"
    assert p.last_stop_reason == "stop_sequence"


async def test_a_side_call_cut_is_an_answer_and_its_bound_rides_the_body():
    sink: list = []
    p = _provider(sink, _text("half", stop="max_tokens"), max_tokens=4096)
    out = await _drain_msgs(p, [{"role": "user", "content": "hi"}], max_tokens=50)
    assert out == "half"
    assert p.last_stop_reason == "max_tokens"
    assert sink[0]["max_tokens"] == 50


async def test_the_request_body_shape_system_roles_and_sampling():
    sink: list = []
    p = _provider(
        sink,
        _text("ok"),
        max_tokens=1234,
        sampling={"top_k": 20, "temperature": None},
    )
    msgs = [
        {"role": "system", "content": "first system"},
        {"role": "user", "content": "q"},
        {"role": "tool", "content": "dropped"},
        {"role": "assistant", "content": ""},
        {"role": "system", "content": "second system"},
    ]
    assert await _drain_msgs(p, msgs) == "ok"
    # the LAST system message wins (not joined); other roles are dropped; an
    # empty turn is sent as "(no content)"; a None sampling value is omitted
    assert sink[0] == {
        "model": "claude-test",
        "max_tokens": 1234,
        "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": "(no content)"},
        ],
        "stream": True,
        "system": "second system",
        "top_k": 20,
    }


async def test_no_system_message_sends_no_system_key():
    sink: list = []
    await _drain(_provider(sink, _text("ok")))
    assert "system" not in sink[0]


async def test_missing_key_and_missing_model_fail_loud(monkeypatch):
    sink: list = []
    monkeypatch.delenv("ANTHROPIC_API_KEY")
    with pytest.raises(RuntimeError) as e:
        await _drain(_provider(sink, _text("x")))
    assert str(e.value) == "ANTHROPIC_API_KEY not set in environment (.env)"
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-ant-test-fixture")
    with pytest.raises(RuntimeError) as e:
        await _drain(_provider(sink, _text("x"), model=""))
    assert str(e.value) == (
        "anthropic_backend: no `model` set — this connector does not guess "
        "one. Name a model on the agent record (model=...)."
    )
    assert sink == []


async def test_the_key_and_version_ride_the_headers():
    seen: list = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(dict(request.headers))
        return httpx.Response(200, text=_text("ok"))

    p = AnthropicProvider(
        endpoint="https://anthropic.test/v1/messages",
        model="claude-test",
        transport=httpx.MockTransport(handler),
    )
    await _drain(p)
    assert seen[0]["x-api-key"] == "sk-ant-test-fixture"
    assert seen[0]["anthropic-version"] == "2023-06-01"


async def test_the_retry_backoff_and_its_log_line(monkeypatch, capsys):
    import asyncio

    delays: list = []
    real_sleep = asyncio.sleep

    async def fake_sleep(d, *a, **kw):
        delays.append(d)
        await real_sleep(0)

    monkeypatch.setattr(asyncio, "sleep", fake_sleep)
    sink: list = []
    p = AnthropicProvider(
        endpoint="https://anthropic.test/v1/messages",
        model="claude-test",
        transport=_sequence(
            [
                (502, "bad gateway", "text/plain"),
                (521, "<!DOCTYPE html>", ""),
                (200, _text("recovered"), "text/event-stream"),
            ],
            sink,
        ),
    )
    assert await _drain(p) == "recovered"
    assert len(sink) == 3
    assert delays == [1.5, 3.0]
    out = capsys.readouterr().out
    assert "[anthropic] 502 (text/plain) — transport error, retry 1/2" in out
    assert "[anthropic] 521 (no content-type) — transport error, retry 2/2" in out


async def test_the_wire_trace_prints_what_left_and_what_came_back(monkeypatch, capsys):
    monkeypatch.setenv("FANTASTIC_AI_WIRE", "1")
    await _drain(_provider([], _text("hi")))
    out = capsys.readouterr().out
    assert "[wire] -> max_tokens=8192 tools=0 msgs=1" in out
    assert "[wire] <- stop_reason='end_turn' blocks=['text']" in out
