"""ollama_backend.provider — streaming chat with mocked ollama client."""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock, patch


from ollama_backend.provider import OllamaProvider


def _mk_chunk(content: str = "", tool_calls: list | None = None) -> dict:
    msg = {"content": content}
    if tool_calls is not None:
        msg["tool_calls"] = tool_calls
    return {"message": msg}


async def _async_iter(items):
    for it in items:
        yield it


def _patched_client(stream_items):
    """Build a mock for ollama.AsyncClient that returns stream_items from chat()."""
    client = MagicMock()
    client.chat = AsyncMock(return_value=_async_iter(stream_items))
    return client


async def test_chat_yields_content_tokens():
    p = OllamaProvider()
    chunks = [_mk_chunk(content="hello "), _mk_chunk(content="world")]
    with patch("ollama.AsyncClient", return_value=_patched_client(chunks)):
        out = []
        async for x in p.chat([{"role": "user", "content": "hi"}], tools=[]):
            out.append(x)
    assert out == ["hello ", "world"]


async def test_chat_yields_tool_call_dict():
    p = OllamaProvider()
    chunks = [
        _mk_chunk(content="working… "),
        _mk_chunk(
            tool_calls=[
                {
                    "id": "call_abc",
                    "function": {"name": "send", "arguments": {"target_id": "x"}},
                }
            ]
        ),
    ]
    with patch("ollama.AsyncClient", return_value=_patched_client(chunks)):
        out = []
        async for x in p.chat([{"role": "user", "content": "go"}], tools=[]):
            out.append(x)
    # First chunk is text; second yields a tool_call dict.
    assert out[0] == "working… "
    assert isinstance(out[1], dict)
    assert out[1]["tool_call"]["name"] == "send"
    assert out[1]["tool_call"]["arguments"] == {"target_id": "x"}


async def test_chat_handles_null_tool_calls():
    """ollama returns `null` (not absent) for tool_calls when none — must normalize to []."""
    p = OllamaProvider()
    chunks = [{"message": {"content": "ok", "tool_calls": None}}]
    with patch("ollama.AsyncClient", return_value=_patched_client(chunks)):
        out = []
        async for x in p.chat([{"role": "user", "content": "hi"}], tools=[]):
            out.append(x)
    assert out == ["ok"]


def test_default_model_and_endpoint():
    from ollama_backend.provider import DEFAULT_ENDPOINT, DEFAULT_MODEL

    p = OllamaProvider()
    assert p.model == DEFAULT_MODEL
    assert p._endpoint == DEFAULT_ENDPOINT


def test_stop_resets_client():
    p = OllamaProvider()
    p._client = "fake"
    p.stop()
    assert p._client is None
