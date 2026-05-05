"""NvidiaNimProvider — OpenAI-compatible SSE parsing + tool_call argument
aggregation across delta chunks (provider-level, not agent-level).

We don't hit the real NIM endpoint; we feed the provider a scripted
SSE byte stream via httpx.MockTransport and assert the yield shape."""

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


# ─── tool_call aggregation ─────────────────────────────────────


async def test_chat_aggregates_streamed_tool_call_args():
    """OpenAI streams `function.arguments` split across N deltas under
    the same `index`. Provider must accumulate and emit ONE tool_call
    dict with the full parsed arguments."""
    body = _sse(
        # First delta carries id + name + first arg fragment.
        '{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","function":{"name":"send","arguments":"{\\"tar"}}]}}]}',
        # Second carries another fragment.
        '{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"get_id\\":\\"core\\",\\"payload\\":{\\"type\\":\\""}}]}}]}',
        # Third closes it out.
        '{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"list_agents\\"}}"}}]}}]}',
        "[DONE]",
    )
    p = NvidiaNimProvider(api_key="nvapi-test", transport=_mock_transport(body))
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert len(out) == 1
    assert isinstance(out[0], dict)
    tc = out[0]["tool_call"]
    assert tc["id"] == "call_x"
    assert tc["name"] == "send"
    assert tc["arguments"] == {
        "target_id": "core",
        "payload": {"type": "list_agents"},
    }


async def test_chat_yields_text_then_tool_call():
    """Text deltas appear in-stream; tool_calls are yielded after the
    stream ends (provider aggregates them per index)."""
    body = _sse(
        '{"choices":[{"delta":{"content":"thinking..."}}]}',
        '{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"send","arguments":"{\\"target_id\\":\\"core\\",\\"payload\\":{}}"}}]}}]}',
        "[DONE]",
    )
    p = NvidiaNimProvider(api_key="nvapi-test", transport=_mock_transport(body))
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    # Text chunk first, then the dict tool_call.
    assert out[0] == "thinking..."
    assert isinstance(out[1], dict)
    tc = out[1]["tool_call"]
    assert tc["name"] == "send"
    assert tc["arguments"] == {"target_id": "core", "payload": {}}


async def test_chat_aggregates_two_independent_tool_calls():
    """Two indices in parallel → two tool_call yields, in index order
    isn't guaranteed by the provider but both must show up."""
    body = _sse(
        '{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"send","arguments":"{\\"target_id\\":\\"core\\",\\"payload\\":{}}"}}]}}]}',
        '{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"send","arguments":"{\\"target_id\\":\\"cli\\",\\"payload\\":{}}"}}]}}]}',
        "[DONE]",
    )
    p = NvidiaNimProvider(api_key="nvapi-test", transport=_mock_transport(body))
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert len(out) == 2
    targets = sorted(o["tool_call"]["arguments"]["target_id"] for o in out)
    assert targets == ["cli", "core"]


async def test_chat_skips_tool_call_without_name():
    """Defensive: stream that never delivered a `function.name` shouldn't
    yield a malformed tool_call (we can't dispatch nameless calls)."""
    body = _sse(
        '{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","function":{"arguments":"{}"}}]}}]}',
        "[DONE]",
    )
    p = NvidiaNimProvider(api_key="nvapi-test", transport=_mock_transport(body))
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert out == []


async def test_chat_recovers_from_unparseable_tool_call_args():
    """Malformed argument JSON falls back to {} so the agent loop can
    still observe the call and decide what to do."""
    body = _sse(
        '{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","function":{"name":"send","arguments":"{not-json"}}]}}]}',
        "[DONE]",
    )
    p = NvidiaNimProvider(api_key="nvapi-test", transport=_mock_transport(body))
    try:
        out = await _collect(p)
    finally:
        await p.aclose()
    assert len(out) == 1
    assert out[0]["tool_call"]["arguments"] == {}


# ─── transport / auth ──────────────────────────────────────────


async def test_chat_sends_bearer_auth_and_model_in_body():
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
        await _collect(p, tools=[{"type": "function", "function": {"name": "send"}}])
    finally:
        await p.aclose()

    assert received["auth"] == "Bearer nvapi-secret"
    assert "nvidia/llama-3_1-nemotron-ultra-253b-v1" in received["body"]
    assert '"stream": true' in received["body"] or '"stream":true' in received["body"]
    # tools were forwarded.
    assert '"send"' in received["body"]


async def test_chat_raises_on_http_error():
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(401, json={"error": "unauthorized"})

    p = NvidiaNimProvider(api_key="nvapi-bad", transport=httpx.MockTransport(handler))
    try:
        with pytest.raises(httpx.HTTPStatusError):
            await _collect(p)
    finally:
        await p.aclose()
