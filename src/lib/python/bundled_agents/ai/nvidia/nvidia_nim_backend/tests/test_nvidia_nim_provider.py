"""NvidiaNimProvider — OpenAI-compatible SSE parsing, PURE RAW TEXT.

The provider streams only `str` content tokens — NO native tool-calling. Tool
calls ride the text as `<tool_call>` envelopes and are parsed by ai_core, not
here. We don't hit the real NIM endpoint; we feed a scripted SSE byte stream via
httpx.MockTransport and assert the yield shape."""

from __future__ import annotations

import httpx
import pytest

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
    p = NvidiaNimProvider(api_key="nvapi-test", transport=_mock_transport(body))
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
    p = NvidiaNimProvider(api_key="nvapi-test", transport=_mock_transport(body))
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
    p = NvidiaNimProvider(api_key="nvapi-test", transport=_mock_transport(body))
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
    p = NvidiaNimProvider(api_key="nvapi-test", transport=_mock_transport(body))
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
    p = NvidiaNimProvider(api_key="nvapi-test", transport=_mock_transport(body))
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

    p = NvidiaNimProvider(api_key="nvapi-bad", transport=httpx.MockTransport(handler))
    try:
        with pytest.raises(httpx.HTTPStatusError):
            await _collect(p)
    finally:
        await p.aclose()
