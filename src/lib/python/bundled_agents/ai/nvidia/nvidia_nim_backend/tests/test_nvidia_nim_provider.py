"""NvidiaNimProvider — OpenAI-compatible SSE parsing, PURE RAW TEXT.

The provider streams only `str` content tokens — NO native tool-calling. Tool
calls ride the text as `<tool_call>` envelopes and are parsed by ai_core, not
here. We don't hit the real NIM endpoint; we feed a scripted SSE byte stream via
httpx.MockTransport and assert the yield shape."""

from __future__ import annotations

import json

import httpx
import pytest
from ai_core.core import THINK_CLOSE, THINK_OPEN

from nvidia_nim_backend.provider import NvidiaNimProvider


def _sse(*events: str) -> bytes:
    """Build a fake SSE body. Each event is a JSON string OR `[DONE]`."""
    out = []
    for ev in events:
        out.append(f"data: {ev}\n\n")
    return "".join(out).encode("utf-8")


def _mock_transport(body: bytes) -> httpx.MockTransport:
    """Return an httpx transport that always answers with the given body
    as text/event-stream. Asserts the request shape on the way through."""

    def handler(request: httpx.Request) -> httpx.Response:
        assert request.method == "POST"
        assert request.url.path.endswith("/chat/completions")
        assert request.headers.get("authorization", "").startswith("Bearer ")
        return httpx.Response(
            200,
            headers={"content-type": "text/event-stream"},
            content=body,
        )

    return httpx.MockTransport(handler)


async def _collect(provider, **kwargs):
    out = []
    async for x in provider.chat(
        messages=[{"role": "user", "content": "hi"}], **kwargs
    ):
        out.append(x)
    return out


# ─── text streaming ────────────────────────────────────────────


async def test_chat_yields_text_chunks_in_order():
    body = _sse(
        '{"choices":[{"delta":{"content":"Hello"}}]}',
        '{"choices":[{"delta":{"content":", "}}]}',
        '{"choices":[{"delta":{"content":"world"}}]}',
        '{"choices":[{"delta":{"content":"!"}}]}',
        "[DONE]",
    )
    p = NvidiaNimProvider(
        endpoint="https://nim.test/v1",
        api_key="nvapi-test",
        model="nvidia-test",
        transport=_mock_transport(body),
    )
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert out == ["Hello", ", ", "world", "!"]


async def test_chat_passes_tool_call_text_through_untouched():
    """A `<tool_call>` envelope in the content stream is just text to the provider —
    it does NOT interpret it (ai_core parses it downstream)."""
    body = _sse(
        '{"choices":[{"delta":{"content":"working "}}]}',
        '{"choices":[{"delta":{"content":"<tool_call>{\\"name\\":\\"send\\"}</tool_call>"}}]}',
        "[DONE]",
    )
    p = NvidiaNimProvider(
        endpoint="https://nim.test/v1",
        api_key="nvapi-test",
        model="nvidia-test",
        transport=_mock_transport(body),
    )
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert out == ["working ", '<tool_call>{"name":"send"}</tool_call>']
    assert all(isinstance(x, str) for x in out)


async def test_chat_handles_done_marker_and_stops():
    """Anything after [DONE] is ignored."""
    body = _sse(
        '{"choices":[{"delta":{"content":"first"}}]}',
        "[DONE]",
        '{"choices":[{"delta":{"content":"after-done"}}]}',
    )
    p = NvidiaNimProvider(
        endpoint="https://nim.test/v1",
        api_key="nvapi-test",
        model="nvidia-test",
        transport=_mock_transport(body),
    )
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert out == ["first"]


async def test_chat_skips_blank_and_malformed_events():
    """Blank `data:` lines and JSON parse errors must not abort the stream."""
    body = (
        b"data: \n\n"
        b"data: not-json\n\n"
        b'data: {"choices":[{"delta":{"content":"ok"}}]}\n\n'
        b"data: [DONE]\n\n"
    )
    p = NvidiaNimProvider(
        endpoint="https://nim.test/v1",
        api_key="nvapi-test",
        model="nvidia-test",
        transport=_mock_transport(body),
    )
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert out == ["ok"]


async def test_chat_ignores_empty_choices_arrays():
    body = _sse(
        '{"choices":[]}',
        '{"choices":[{"delta":{"content":"ok"}}]}',
        "[DONE]",
    )
    p = NvidiaNimProvider(
        endpoint="https://nim.test/v1",
        api_key="nvapi-test",
        model="nvidia-test",
        transport=_mock_transport(body),
    )
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert out == ["ok"]


# ─── transport / auth ──────────────────────────────────────────


async def test_chat_sends_bearer_auth_and_model_in_body_and_no_tools():
    received: dict = {}

    def handler(request: httpx.Request) -> httpx.Response:
        received["auth"] = request.headers.get("authorization")
        received["body"] = request.content.decode("utf-8")
        return httpx.Response(
            200,
            headers={"content-type": "text/event-stream"},
            content=_sse('{"choices":[{"delta":{"content":"k"}}]}', "[DONE]"),
        )

    p = NvidiaNimProvider(
        endpoint="https://nim.test/v1",
        api_key="nvapi-secret",
        model="nvidia/llama-3_1-nemotron-ultra-253b-v1",
        transport=httpx.MockTransport(handler),
    )
    try:
        await _collect(p)
    finally:
        await p.aclose()

    assert received["auth"] == "Bearer nvapi-secret"
    assert "nvidia/llama-3_1-nemotron-ultra-253b-v1" in received["body"]
    assert '"stream": true' in received["body"] or '"stream":true' in received["body"]
    # RAW: NO native tools array is ever sent.
    assert '"tools"' not in received["body"]
    assert '"tool_choice"' not in received["body"]


async def test_chat_raises_on_http_error():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(401, json={"error": "unauthorized"})

    p = NvidiaNimProvider(
        endpoint="https://nim.test/v1",
        api_key="nvapi-bad",
        model="nvidia-test",
        transport=httpx.MockTransport(handler),
    )
    try:
        with pytest.raises(httpx.HTTPStatusError):
            await _collect(p)
    finally:
        await p.aclose()


# ─── the wire, pinned (reduction candidate A: one OpenAI SSE loop) ──────────
#
# Each behaviour below is what THIS provider does today. A shared SSE helper
# must keep every one of them for nvidia — including where nvidia differs from
# vllm (a non-JSON `data:` line is SKIPPED here, raised there; `delta.reasoning`
# is NOT read here, it is in vllm).


def _recording(body: bytes, sink: list) -> httpx.MockTransport:
    def handler(request: httpx.Request) -> httpx.Response:
        sink.append(json.loads(request.content))
        return httpx.Response(
            200, headers={"content-type": "text/event-stream"}, content=body
        )

    return httpx.MockTransport(handler)


def _nim(transport, model: str = "nvidia-test") -> NvidiaNimProvider:
    return NvidiaNimProvider(
        endpoint="https://nim.test/v1",
        api_key="nvapi-test",
        model=model,
        transport=transport,
    )


async def test_reasoning_content_is_wrapped_once_and_closed_by_content():
    body = _sse(
        '{"choices":[{"delta":{"reasoning_content":"let me "}}]}',
        '{"choices":[{"delta":{"reasoning_content":"think"}}]}',
        '{"choices":[{"delta":{"content":"answer"}}]}',
        "[DONE]",
    )
    p = _nim(_mock_transport(body))
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert out == [THINK_OPEN, "let me ", "think", THINK_CLOSE, "answer"]


async def test_a_stream_that_ends_mid_thought_still_closes_the_wrapper():
    body = _sse('{"choices":[{"delta":{"reasoning_content":"hm"}}]}', "[DONE]")
    p = _nim(_mock_transport(body))
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert out == [THINK_OPEN, "hm", THINK_CLOSE]


async def test_the_reasoning_alias_is_not_read_by_nvidia():
    # `delta.reasoning` is vllm's alternate spelling; this provider reads only
    # `reasoning_content`.
    body = _sse(
        '{"choices":[{"delta":{"reasoning":"hidden"}}]}',
        '{"choices":[{"delta":{"content":"ok"}}]}',
        "[DONE]",
    )
    p = _nim(_mock_transport(body))
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert out == ["ok"]


async def test_reported_usage_and_finish_reason_land_on_the_provider():
    body = _sse(
        '{"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}',
        '{"choices":[],"usage":{"prompt_tokens":321}}',
        "[DONE]",
    )
    p = _nim(_mock_transport(body))
    try:
        assert await _collect(p) == ["x"]
    finally:
        await p.aclose()
    # `length` is mapped to the kernel's word for a cut; a main-turn cut does
    # NOT raise in this provider (vllm's does).
    assert p.last_stop_reason == "max_tokens"
    assert p.last_prompt_tokens == 321


async def test_a_finish_reason_other_than_length_passes_through_verbatim():
    body = _sse('{"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}')
    p = _nim(_mock_transport(body))
    try:
        await _collect(p)
    finally:
        await p.aclose()
    assert p.last_stop_reason == "stop"
    # zero / absent usage leaves the class default
    assert p.last_prompt_tokens is None


async def test_a_stream_without_done_ends_cleanly():
    body = _sse('{"choices":[{"delta":{"content":"tail"}}]}')
    p = _nim(_mock_transport(body))
    try:
        assert await _collect(p) == ["tail"]
    finally:
        await p.aclose()


async def test_a_side_call_bound_rides_as_max_tokens_and_the_body_shape():
    sink: list = []
    body = _sse('{"choices":[{"delta":{"content":"k"}}]}', "[DONE]")
    p = _nim(_recording(body, sink))
    try:
        await _collect(p, max_tokens=77)
        await _collect(p)
    finally:
        await p.aclose()
    assert sink[0] == {
        "model": "nvidia-test",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": True,
        "stream_options": {"include_usage": True},
        "max_tokens": 77,
    }
    assert "max_tokens" not in sink[1]


async def test_no_model_fails_loud_before_any_request():
    sink: list = []
    p = _nim(_recording(b"", sink), model="")
    try:
        with pytest.raises(RuntimeError) as e:
            await _collect(p)
    finally:
        await p.aclose()
    assert str(e.value) == (
        "nvidia_nim_backend: no `model` set — this connector does not guess "
        "one. Name a model on the agent record (model=...)."
    )
    assert sink == []
