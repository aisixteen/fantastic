"""ollama_backend.provider — streaming chat with mocked ollama client."""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock, patch

import pytest


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
    """Build a mock for ollama.AsyncClient that returns stream_items from chat().
    The provider opens it per call as an async context manager."""
    client = MagicMock()
    client.chat = AsyncMock(return_value=_async_iter(stream_items))
    client.__aenter__ = AsyncMock(return_value=client)
    client.__aexit__ = AsyncMock(return_value=False)
    return client


async def test_chat_yields_content_tokens():
    """The provider is PURE RAW TEXT — it yields only str content tokens (a
    `<tool_call>` envelope rides the text and is parsed by ai_core, not here)."""
    p = OllamaProvider(endpoint="http://ollama.test", model="ollama-test")
    chunks = [
        _mk_chunk(content="hello "),
        _mk_chunk(content='<tool_call>{"name":"send"}</tool_call>'),
    ]
    with patch("ollama.AsyncClient", return_value=_patched_client(chunks)):
        out = []
        async for x in p.chat([{"role": "user", "content": "hi"}]):
            out.append(x)
    # Both are plain strings — the provider does NOT interpret the tool envelope.
    assert out == ["hello ", '<tool_call>{"name":"send"}</tool_call>']
    assert all(isinstance(x, str) for x in out)


async def test_chat_no_tools_arg_in_request():
    """No native `tools` array is ever sent to ollama (raw prompt-and-parse)."""
    p = OllamaProvider(endpoint="http://ollama.test", model="ollama-test")
    client = _patched_client([_mk_chunk(content="ok")])
    with patch("ollama.AsyncClient", return_value=client):
        async for _ in p.chat([{"role": "user", "content": "hi"}]):
            pass
    _, kwargs = client.chat.call_args
    assert "tools" not in kwargs
    assert kwargs["stream"] is True


async def test_chat_handles_null_content():
    """ollama returns `null` (not absent) for content sometimes — normalize to '' (skip)."""
    p = OllamaProvider(endpoint="http://ollama.test", model="ollama-test")
    chunks = [{"message": {"content": None}}, {"message": {"content": "ok"}}]
    with patch("ollama.AsyncClient", return_value=_patched_client(chunks)):
        out = []
        async for x in p.chat([{"role": "user", "content": "hi"}]):
            out.append(x)
    assert out == ["ok"]


def test_endpoint_and_model_are_required():
    # Nothing is guessed: a provider without them cannot be built.
    with pytest.raises(TypeError):
        OllamaProvider()


async def test_the_client_is_opened_and_closed_per_call():
    """The provider is built per turn: each call opens its own client and closes
    it, so nothing holds a connection past the turn (audit ARCH2-2)."""
    p = OllamaProvider(endpoint="http://ollama.test", model="ollama-test")
    client = _patched_client([_mk_chunk(content="ok")])
    with patch("ollama.AsyncClient", return_value=client) as made:
        assert [x async for x in p.chat([{"role": "user", "content": "hi"}])] == ["ok"]
    made.assert_called_once_with(host="http://ollama.test")
    client.__aenter__.assert_awaited_once()
    client.__aexit__.assert_awaited_once()


# ─── the stream, pinned (reduction candidate A: a shared ThinkWrap) ─────────


async def _run(p: OllamaProvider, chunks: list, **kw):
    client = _patched_client(chunks)
    with patch("ollama.AsyncClient", return_value=client):
        out = [x async for x in p.chat([{"role": "user", "content": "hi"}], **kw)]
    return out, client.chat.call_args.kwargs


async def test_thinking_is_wrapped_once_and_closed_by_content():
    from ai_core.core import THINK_CLOSE, THINK_OPEN

    p = OllamaProvider(endpoint="http://ollama.test", model="m")
    out, _ = await _run(
        p,
        [
            {"message": {"thinking": "let me ", "content": None}},
            {"message": {"thinking": "think"}},
            {"message": {"content": "answer"}},
        ],
    )
    assert out == [THINK_OPEN, "let me ", "think", THINK_CLOSE, "answer"]


async def test_a_stream_that_ends_mid_thought_still_closes_the_wrapper():
    from ai_core.core import THINK_CLOSE, THINK_OPEN

    p = OllamaProvider(endpoint="http://ollama.test", model="m")
    out, _ = await _run(p, [{"message": {"thinking": "hm"}}])
    assert out == [THINK_OPEN, "hm", THINK_CLOSE]


async def test_reported_usage_and_done_reason_land_on_the_provider():
    p = OllamaProvider(endpoint="http://ollama.test", model="m")
    await _run(
        p,
        [
            {"message": {"content": "x"}},
            {"message": {}, "prompt_eval_count": 777, "done_reason": "length"},
        ],
    )
    assert p.last_prompt_tokens == 777
    assert p.last_stop_reason == "max_tokens"
    q = OllamaProvider(endpoint="http://ollama.test", model="m")
    await _run(q, [{"message": {"content": "x"}, "done_reason": "stop"}])
    assert q.last_stop_reason == "stop" and q.last_prompt_tokens is None


async def test_the_request_options_from_the_knobs():
    p = OllamaProvider(
        endpoint="http://ollama.test",
        model="m",
        num_ctx=32768,
        temperature=0.7,
        num_predict=256,
        think=False,
    )
    _, kw = await _run(p, [{"message": {"content": "x"}}])
    assert kw == {
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": True,
        "options": {"temperature": 0.7, "num_ctx": 32768, "num_predict": 256},
        "think": False,
    }
    # a side call's own bound beats the record's num_predict (a fresh provider:
    # the first one holds the client it built)
    q = OllamaProvider(endpoint="http://ollama.test", model="m", num_predict=256)
    _, kw = await _run(q, [{"message": {"content": "x"}}], max_tokens=12)
    assert kw["options"]["num_predict"] == 12


async def test_the_defaults_send_only_the_low_temperature():
    p = OllamaProvider(endpoint="http://ollama.test", model="m")
    _, kw = await _run(p, [{"message": {"content": "x"}}])
    assert kw["options"] == {"temperature": 0.3}
    assert "think" not in kw


async def test_no_model_fails_loud_before_any_request():
    p = OllamaProvider(endpoint="http://ollama.test", model="")
    client = _patched_client([])
    with patch("ollama.AsyncClient", return_value=client):
        with pytest.raises(RuntimeError) as e:
            async for _ in p.chat([{"role": "user", "content": "hi"}]):
                pass
    assert str(e.value) == (
        "ollama_backend: no `model` set — this connector does not guess one. "
        "Name a model on the agent record (model=...)."
    )
    client.chat.assert_not_called()
