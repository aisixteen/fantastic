"""AnthropicProvider — the two channels it must read, and the tool_use TRANSLATION.

Tool-calling in this kernel is RAW: `ai_core` teaches the `<tool_call>` envelope
in the prompt and owns the only parser. These assert the provider never breaks
that — including on the OPTIONAL `tool_use` route, whose entire job is to hand
`ai_core` the SAME text a raw-route model would have written itself.
"""

from __future__ import annotations

import json

import httpx
import pytest
from ai_core.core import THINK_CLOSE, THINK_OPEN
from ai_core.tool_parse import extract_tool_calls
from anthropic_backend.provider import RETRIES, SEND_TOOL, AnthropicProvider


@pytest.fixture(autouse=True)
def _key(monkeypatch):
    """The key is read before the request is built, so even a MockTransport run
    needs one present. Set here, never read from the developer's real `.env`."""
    monkeypatch.setenv("ANTHROPIC_KEY", "sk-ant-test-fixture")


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


async def test_tool_use_route_declares_exactly_the_one_send_tool():
    """One tool, matching what ai_core teaches in prose — never a second contract."""
    sink: list = []
    await _drain(_provider(sink, _text("hello"), tool_use=True))
    assert sink[0]["tools"] == [SEND_TOOL]
    assert SEND_TOOL["name"] == "send"


async def test_tool_use_block_becomes_the_text_envelope_ai_core_parses():
    """The translation, end to end: a native block goes in, and what comes out is
    the SAME `<tool_call>` text a raw-route model would have written — so
    `ai_core`'s one parser handles both routes identically."""
    sse = _sse(
        {
            "type": "content_block_start",
            "content_block": {"type": "tool_use", "name": "send"},
        },
        {
            "type": "content_block_delta",
            "delta": {
                "type": "input_json_delta",
                "partial_json": '{"target_id": "kernel_state",',
            },
        },
        {
            "type": "content_block_delta",
            "delta": {
                "type": "input_json_delta",
                "partial_json": ' "payload": {"type": "reflect"}}',
            },
        },
        {"type": "content_block_stop"},
        {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
    )
    out = await _drain(_provider([], sse, tool_use=True))
    calls = extract_tool_calls(out)
    assert len(calls) == 1
    assert calls[0]["name"] == "send"
    assert calls[0]["arguments"] == {
        "target_id": "kernel_state",
        "payload": {"type": "reflect"},
    }


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
    for flag in (True, False):
        with pytest.raises(RuntimeError, match=r"blocks=\['text'\]"):
            await _drain(_provider([], sse, tool_use=flag))


async def test_tool_use_blocks_are_terminal_even_when_we_declared_no_tools():
    """The `/cc` route converts the model's native tool syntax server-side and
    returns `tool_use` blocks with no `tools` key in the request. Measured
    2026-08-24: gating terminality on our own flag failed such a turn on arrival,
    killing the run at pass 1. What the stream carried decides."""
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
    out = await _drain(_provider(sink, sse))  # tool_use=False — we declare nothing
    assert "tools" not in sink[0]
    assert len(extract_tool_calls(out)) == 1


async def test_malformed_tool_args_do_not_crash_the_stream():
    """A truncated `input_json_delta` yields an empty-argument call, not a
    traceback — ai_core's malformed-call path can then teach the model."""
    sse = _sse(
        {
            "type": "content_block_start",
            "content_block": {"type": "tool_use", "name": "send"},
        },
        {
            "type": "content_block_delta",
            "delta": {"type": "input_json_delta", "partial_json": '{"target_id": "ker'},
        },
        {"type": "content_block_stop"},
        {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
    )
    calls = extract_tool_calls(await _drain(_provider([], sse, tool_use=True)))
    assert len(calls) == 1 and calls[0]["arguments"] == {}


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
        model="claude-test",
        transport=_sequence([(401, "<!DOCTYPE html>", "text/html")], sink),
    )
    with pytest.raises(RuntimeError, match="anthropic 401"):
        await _drain(p)
    assert len(sink) == 1
