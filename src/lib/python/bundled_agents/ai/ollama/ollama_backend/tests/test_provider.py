"""ollama_backend.provider — streaming chat with mocked ollama client."""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock, patch

import httpx
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
    it, so nothing holds a connection past the turn."""
    p = OllamaProvider(endpoint="http://ollama.test", model="ollama-test")
    client = _patched_client([_mk_chunk(content="ok")])
    with patch("ollama.AsyncClient", return_value=client) as made:
        assert [x async for x in p.chat([{"role": "user", "content": "hi"}])] == ["ok"]
    made.assert_called_once_with(
        host="http://ollama.test", timeout=httpx.Timeout(60.0, connect=15.0)
    )
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


async def test_a_thinking_only_chunk_is_not_an_empty_completion():
    from ai_core.core import THINK_CLOSE, THINK_OPEN

    p = OllamaProvider(endpoint="http://ollama.test", model="m")
    out, _ = await _run(p, [{"message": {"thinking": "all of it"}, "done": True}])
    assert "".join(out) != ""
    assert "".join(out) == f"{THINK_OPEN}all of it{THINK_CLOSE}"
    assert out == [THINK_OPEN, "all of it", THINK_CLOSE]
    # explicit nulls on both channels are shape, not content
    out, _ = await _run(p, [{"message": {"thinking": None, "content": None}}])
    assert out == []


async def _serve_once(body: bytes):
    """A real local server answering POST /api/chat with the exact ndjson body."""
    import asyncio

    async def handle(reader, writer):
        try:
            head = await reader.readuntil(b"\r\n\r\n")
            n = 0
            for line in head.split(b"\r\n"):
                if line.lower().startswith(b"content-length:"):
                    n = int(line.split(b":")[1])
            if n:
                await reader.readexactly(n)
            writer.write(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n"
                b"Content-Length: " + str(len(body)).encode() + b"\r\n"
                b"Connection: close\r\n\r\n" + body
            )
            await writer.drain()
        finally:
            writer.close()

    server = await asyncio.start_server(handle, "127.0.0.1", 0)
    return server, server.sockets[0].getsockname()[1]


async def _chat_against(body: bytes):
    server, port = await _serve_once(body)
    try:
        p = OllamaProvider(endpoint=f"http://127.0.0.1:{port}", model="m")
        out = [x async for x in p.chat([{"role": "user", "content": "hi"}])]
        return p, out
    finally:
        server.close()
        await server.wait_closed()


async def test_a_final_ndjson_line_without_a_newline_still_counts():
    from ai_core.core import THINK_CLOSE, THINK_OPEN

    body = (
        b'{"message":{"role":"assistant","content":"Hello"}}\n'
        b'{"message":{"role":"assistant","content":" world"},"done":true,"done_reason":"stop"}'
    )
    p, out = await _chat_against(body)
    assert "".join(out) == "Hello world"
    assert p.last_stop_reason == "stop"

    _, out = await _chat_against(b'{"message":{"role":"assistant","thinking":"hm"}}')
    assert out == [THINK_OPEN, "hm", THINK_CLOSE]


def test_caching_none_and_cleanup_pass_and_provider_cache_is_refused_naming_ollama_backend():
    import ollama_backend.tools as ot

    rec = {"endpoint": "http://o.test", "memory_store_id": "memory"}
    for mode in ("none", "cleanup"):
        assert ot.validate_meta({"caching": mode}, rec) is None
    assert ot.validate_meta({"caching": "provider_cache"}, rec) == (
        "ollama_backend cannot do provider caching: caching=provider_cache "
        "is refused \u2014 use none or cleanup"
    )
    assert ot.validate_meta({}, {**rec, "caching": "provider_cache"}) is not None
    assert ot.validate_meta({"caching": "always"}, rec) == (
        "caching='always' is not one of the legal values: none, cleanup, provider_cache"
    )


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


# ─── link failures are transport (#198) ─────────────────────────────────────


async def _raising_stream(first: dict, exc: Exception):
    yield first
    raise exc


async def test_an_unreachable_endpoint_is_a_transport_failure():
    from ai_core.provider import ProviderError

    p = OllamaProvider(endpoint="http://127.0.0.1:1", model="test-model")
    with pytest.raises(ProviderError) as e:
        async for _ in p.chat([{"role": "user", "content": "hi"}]):
            pass
    assert e.value.transport is True
    assert str(e.value).startswith("ollama: request failed: ")


async def test_a_stream_that_breaks_mid_answer_is_a_transport_failure():
    import httpx
    from ai_core.provider import ProviderError

    p = OllamaProvider(endpoint="http://ollama.test", model="m")
    client = _patched_client([])
    client.chat = AsyncMock(
        return_value=_raising_stream(_mk_chunk("par"), httpx.ReadError("cut"))
    )
    with patch("ollama.AsyncClient", return_value=client):
        with pytest.raises(ProviderError) as e:
            async for _ in p.chat([{"role": "user", "content": "hi"}]):
                pass
    assert e.value.transport is True
    assert str(e.value) == "ollama: stream error: cut"


async def test_a_status_the_server_answered_is_raised_as_it_was():
    import ollama

    p = OllamaProvider(endpoint="http://ollama.test", model="m")
    client = _patched_client([])
    client.chat = AsyncMock(
        return_value=_raising_stream(_mk_chunk("x"), ollama.ResponseError("boom", 503))
    )
    with patch("ollama.AsyncClient", return_value=client):
        with pytest.raises(ollama.ResponseError) as e:
            async for _ in p.chat([{"role": "user", "content": "hi"}]):
                pass
    assert getattr(e.value, "transport", False) is False


# ─── the read clock (#177) ──────────────────────────────────────────────────


async def test_a_stalled_ollama_stream_is_stream_stalled():
    import httpx
    from ai_core.provider import ProviderError

    p = OllamaProvider(endpoint="http://ollama.test", model="m", stream_idle=0.2)
    client = _patched_client([])
    client.chat = AsyncMock(
        return_value=_raising_stream(_mk_chunk("par"), httpx.ReadTimeout("no byte"))
    )
    with patch("ollama.AsyncClient", return_value=client):
        with pytest.raises(ProviderError) as e:
            async for _ in p.chat([{"role": "user", "content": "hi"}]):
                pass
    assert e.value.transport is True
    assert str(e.value) == (
        "ollama_backend: stream_stalled — http://ollama.test sent no bytes for 0.2s; "
        "this turn is NOT complete"
    )


async def test_the_client_runs_on_the_read_clock():
    p = OllamaProvider(endpoint="http://ollama.test", model="m", stream_idle=4.5)
    client = _patched_client([_mk_chunk(content="ok")])
    with patch("ollama.AsyncClient", return_value=client) as ctor:
        async for _ in p.chat([{"role": "user", "content": "hi"}]):
            pass
    t = ctor.call_args.kwargs["timeout"]
    assert (t.read, t.connect) == (4.5, 15.0)
    p = OllamaProvider(endpoint="http://ollama.test", model="m")
    with patch(
        "ollama.AsyncClient", return_value=_patched_client([_mk_chunk(content="ok")])
    ) as ctor:
        async for _ in p.chat([{"role": "user", "content": "hi"}]):
            pass
    assert ctor.call_args.kwargs["timeout"].read == 60.0


def test_stream_idle_seconds_is_validated_at_the_write():
    from ollama_backend.tools import RECORD_SCHEMA, validate_meta

    rec = {"endpoint": "http://ollama.test", "memory_store_id": "memory"}
    for bad in (0, -1, "x"):
        msg = validate_meta({"stream_idle_seconds": bad}, rec)
        assert msg and msg.startswith(
            "stream_idle_seconds must be a positive number of seconds"
        )
    assert validate_meta({"stream_idle_seconds": 2.5}, rec) is None
    assert "stream_idle_seconds" in [f["name"] for f in RECORD_SCHEMA["fields"]]
