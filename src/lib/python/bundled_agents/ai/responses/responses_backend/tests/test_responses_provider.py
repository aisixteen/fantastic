"""ResponsesProvider — the wire it sends, the two channels it reads, and NO native
tool route.

Tool-calling in this kernel is RAW: `ai_core` teaches the `<call>` grammar in the
prompt and owns the only parser. These assert the provider never breaks that: it
declares no `tools`, and a native `function_call` item is a loud error, never a
translation. A cut is visible — an error on a main turn, `last_stop_reason` on a
bounded side call.
"""

from __future__ import annotations

import json

import httpx
import pytest
from ai_core.core import THINK_CLOSE, THINK_OPEN
from responses_backend.provider import (
    RETRIES,
    ResponsesProvider,
)

EP = "http://proxy.test/v1/responses"


async def _key(endpoint: str) -> str:
    """The key reader a provider awaits before each request (in the kernel, the
    record's `credentials` read from a credential store — `ai_core/credentials.py`)."""
    return "sk-test-fixture"


async def _refused(endpoint: str) -> str:
    raise RuntimeError("responses_backend: credentials creds/openai: no such ref")


def _sse(*events: dict) -> str:
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events)


def _done(status: str = "completed", reason: str | None = None, usage=None) -> dict:
    resp: dict = {"status": status, "usage": usage or {"input_tokens": 42}}
    if reason:
        resp["incomplete_details"] = {"reason": reason}
    return {
        "type": "response.completed"
        if status == "completed"
        else "response.incomplete",
        "response": resp,
    }


def _text(*chunks: str, **done) -> str:
    evts: list[dict] = [
        {"type": "response.created", "response": {"status": "in_progress"}},
        {"type": "response.output_item.added", "item": {"type": "message"}},
    ]
    evts += [{"type": "response.output_text.delta", "delta": c} for c in chunks]
    return _sse(*evts, _done(**done))


def _capture(sink: list, sse: str, status: int = 200):
    def handler(request: httpx.Request) -> httpx.Response:
        sink.append(request)
        return httpx.Response(status, text=sse)

    return httpx.MockTransport(handler)


def _provider(sink: list, sse: str, **kw) -> ResponsesProvider:
    kw.setdefault("model", "qwen-test")
    kw.setdefault("endpoint", EP)
    kw.setdefault("key", _key)
    return ResponsesProvider(transport=_capture(sink, sse), **kw)


async def _drain(p: ResponsesProvider, **kw) -> str:
    msgs = [
        {"role": "system", "content": "be terse"},
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": "hello"},
        {"role": "user", "content": "again"},
    ]
    return "".join([c async for c in p.chat(msgs, **kw)])


async def test_request_is_stateless_typed_items_and_no_tools():
    """The system block rides as `instructions`, every other turn as a typed message
    item, the whole conversation each pass (`store:false`) — and no `tools` key: the
    call grammar is taught in the prompt."""
    sink: list = []
    await _drain(_provider(sink, _text("ok")))
    body = json.loads(sink[0].content)
    assert body["instructions"] == "be terse"
    assert body["store"] is False and body["stream"] is True
    assert body["input"] == [
        {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hi"}],
        },
        {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "hello"}],
        },
        {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "again"}],
        },
    ]
    assert "tools" not in body
    assert sink[0].headers["authorization"] == "Bearer sk-test-fixture"
    assert str(sink[0].url) == EP


async def test_budget_and_sampling_ride_the_body():
    """The record's budget is `max_output_tokens`; a side call's own bound wins; the
    sampling knobs merge verbatim, None values dropped."""
    sink: list = []
    p = _provider(
        sink, _text("ok"), max_tokens=900, sampling={"temperature": 0.2, "top_p": None}
    )
    await _drain(p)
    body = json.loads(sink[0].content)
    assert body["max_output_tokens"] == 900
    assert body["temperature"] == 0.2 and "top_p" not in body
    sink.clear()
    await _drain(p, max_tokens=64)
    assert json.loads(sink[0].content)["max_output_tokens"] == 64


def test_sampling_may_not_override_what_the_provider_owns():
    """`store:false` keeps the conversation ours and `model` is the record's — a
    `sampling` key naming either is refused, not merged over it."""
    for key in ("store", "model", "max_output_tokens"):
        with pytest.raises(ValueError, match=key):
            ResponsesProvider("http://x/v1/responses", "m", key=_key, sampling={key: 1})


async def test_thinking_is_wrapped_not_dropped():
    """A reasoning model splits its output; reading the output deltas alone returns a
    turn spent thinking as an EMPTY completion, killing the chain with no error."""
    sse = _sse(
        {"type": "response.output_item.added", "item": {"type": "reasoning"}},
        {"type": "response.reasoning_text.delta", "delta": "let me "},
        {"type": "response.reasoning_text.delta", "delta": "think"},
        {"type": "response.output_item.added", "item": {"type": "message"}},
        {"type": "response.output_text.delta", "delta": "answer"},
        _done(),
    )
    assert (
        await _drain(_provider([], sse))
        == f"{THINK_OPEN}let me think{THINK_CLOSE}answer"
    )


async def test_a_reasoning_summary_is_the_thought_channel_too():
    """OpenAI's own models stream a summary instead of raw reasoning — the same
    channel, wrapped, never output."""
    sse = _sse(
        {"type": "response.reasoning_summary_text.delta", "delta": "sum"},
        {"type": "response.output_text.delta", "delta": "out"},
        _done(),
    )
    assert await _drain(_provider([], sse)) == f"{THINK_OPEN}sum{THINK_CLOSE}out"


async def test_all_thought_turn_still_closes_the_wrapper():
    """A turn that never reaches output must still balance the marker pair — an
    unterminated block is an unparseable turn downstream."""
    sse = _sse({"type": "response.reasoning_text.delta", "delta": "hm"}, _done())
    assert await _drain(_provider([], sse)) == f"{THINK_OPEN}hm{THINK_CLOSE}"


async def test_reported_usage_calibrates():
    p = _provider([], _text("ok", usage={"input_tokens": 1234, "output_tokens": 2}))
    await _drain(p)
    assert p.last_prompt_tokens == 1234
    assert p.last_stop_reason == "completed"


async def test_a_cut_main_turn_raises_instead_of_faking_a_finish():
    """`max_output_tokens` mid-envelope returned as finished parses as garbage and
    the chain dies downstream with the cause hidden. A cut turn is an error."""
    p = _provider([], _text("half a ", status="incomplete", reason="max_output_tokens"))
    with pytest.raises(RuntimeError, match="without completing.*max_output_tokens"):
        await _drain(p)
    assert p.last_stop_reason == "max_tokens"


async def test_a_bounded_side_call_cut_is_an_answer():
    """A fold summary asked for at most N tokens: stopping there is the answer, and
    the caller reads `last_stop_reason` to refuse a cut summary."""
    p = _provider([], _text("partial", status="incomplete", reason="max_output_tokens"))
    assert await _drain(p, max_tokens=16) == "partial"
    assert p.last_stop_reason == "max_tokens"


async def test_a_stream_with_no_terminal_event_is_a_cut_connection():
    sse = _sse({"type": "response.output_text.delta", "delta": "x"})
    with pytest.raises(RuntimeError, match="connection cut mid-stream"):
        await _drain(_provider([], sse))


async def test_response_failed_is_loud():
    sse = _sse(
        {"type": "response.output_text.delta", "delta": "x"},
        {
            "type": "response.failed",
            "response": {"error": {"message": "upstream died"}},
        },
    )
    with pytest.raises(RuntimeError, match="upstream died"):
        await _drain(_provider([], sse))


async def test_a_native_function_call_is_a_loud_error_not_a_silent_drop():
    """Its arguments are JSON this kernel does not read; dropping the item would end
    the turn looking finished with the call lost, so it raises, naming the grammar."""
    sse = _sse(
        {
            "type": "response.output_item.added",
            "item": {"type": "function_call", "name": "send"},
        },
        {"type": "response.function_call_arguments.delta", "delta": "{}"},
        _done(),
    )
    with pytest.raises(RuntimeError, match=r"native function_call.*<call name="):
        await _drain(_provider([], sse))


async def test_missing_endpoint_model_or_key_fail_loud_naming_it():
    with pytest.raises(RuntimeError, match="endpoint"):
        await _drain(ResponsesProvider(endpoint="", model="m", key=_key))
    with pytest.raises(RuntimeError, match="model"):
        await _drain(ResponsesProvider(endpoint=EP, model="", key=_key))
    with pytest.raises(RuntimeError, match="credentials"):
        await _drain(ResponsesProvider(endpoint=EP, model="m", key=_refused))


# ── transport retry ──────────────────────────────────────────────────────────
# A CDN error page (transport code, non-JSON body) is retried, bounded; anything
# the endpoint answered deliberately is not.


def _sequence(responses: list, sink: list):
    def handler(request: httpx.Request) -> httpx.Response:
        sink.append(request)
        status, body, ctype = responses[min(len(sink) - 1, len(responses) - 1)]
        return httpx.Response(status, text=body, headers={"content-type": ctype})

    return httpx.MockTransport(handler)


def _seq_provider(responses: list, sink: list) -> ResponsesProvider:
    return ResponsesProvider(
        endpoint=EP, model="m", key=_key, transport=_sequence(responses, sink)
    )


async def test_a_successful_call_makes_exactly_one_request():
    sink: list = []
    p = _seq_provider([(200, _text("hi"), "text/event-stream")], sink)
    assert await _drain(p) == "hi"
    assert len(sink) == 1


async def test_cdn_error_page_is_retried_then_succeeds():
    sink: list = []
    p = _seq_provider(
        [
            (521, "<!DOCTYPE html>", "text/html"),
            (200, _text("recovered"), "text/event-stream"),
        ],
        sink,
    )
    assert await _drain(p) == "recovered"
    assert len(sink) == 2


async def test_transport_error_gives_up_after_the_bound():
    sink: list = []
    p = _seq_provider([(521, "<!DOCTYPE html>", "text/html")], sink)
    with pytest.raises(RuntimeError, match="responses 521"):
        await _drain(p)
    assert len(sink) == RETRIES + 1


async def test_a_deliberate_answer_is_never_retried():
    """A 404 JSON body — e.g. a proxy with no /responses route — is the endpoint
    answering; retrying would spend the same broken request N times."""
    sink: list = []
    p = _seq_provider([(404, '{"error": "not found"}', "application/json")], sink)
    with pytest.raises(RuntimeError, match="responses 404"):
        await _drain(p)
    assert len(sink) == 1


# ─── the wire, pinned (reduction candidate A: a shared retry POST + SSE reader)


async def test_an_error_event_raises_with_the_whole_event():
    sse = _sse({"type": "error", "code": "rate_limit"})
    with pytest.raises(RuntimeError) as e:
        await _drain(_provider([], sse))
    assert (
        str(e.value)
        == "responses stream error: {'type': 'error', 'code': 'rate_limit'}"
    )


async def test_blank_and_non_json_data_lines_are_skipped():
    body = "data: \n\ndata: not-json\n\n" + _text("ok")
    assert await _drain(_provider([], body)) == "ok"


async def test_a_refusal_delta_is_output_and_empty_deltas_yield_nothing():
    sse = _sse(
        {"type": "response.reasoning_text.delta", "delta": ""},
        {"type": "response.output_text.delta", "delta": ""},
        {"type": "response.refusal.delta", "delta": "I can't"},
        _done(),
    )
    out = [c async for c in _provider([], sse).chat([{"role": "user", "content": "q"}])]
    assert out == ["I can't"]


async def test_the_cut_texts_are_exact():
    p = _provider([], _text("half", status="incomplete", reason="max_output_tokens"))
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert str(e.value) == (
        "responses stream ended without completing: status='incomplete' "
        "reason='max_output_tokens' items=['message']"
    )
    sse = _sse({"type": "response.output_text.delta", "delta": "x"})
    with pytest.raises(RuntimeError) as e:
        await _drain(_provider([], sse))
    assert str(e.value) == (
        "responses stream ended without completing: status=None reason=None "
        "items=[] (no terminal event — connection cut mid-stream)"
    )


async def test_an_incomplete_reason_other_than_the_budget_is_the_stop_reason():
    p = _provider([], _text("x", status="incomplete", reason="content_filter"))
    with pytest.raises(RuntimeError):
        await _drain(p, max_tokens=10)
    assert p.last_stop_reason == "content_filter"


async def test_the_missing_config_texts_are_exact():
    with pytest.raises(RuntimeError) as e:
        await _drain(ResponsesProvider(endpoint="", model="m", key=_key))
    assert str(e.value) == (
        "responses_backend: no `endpoint` set — this connector does not "
        "guess a host. Name the full …/v1/responses URL on the agent "
        "record (endpoint=...)."
    )
    with pytest.raises(RuntimeError) as e:
        await _drain(ResponsesProvider(endpoint=EP, model="", key=_key))
    assert str(e.value) == (
        "responses_backend: no `model` set — this connector does not guess "
        "one. Name a model on the agent record (model=...)."
    )
    with pytest.raises(RuntimeError) as e:
        await _drain(ResponsesProvider(endpoint=EP, model="m", key=_refused))
    assert str(e.value) == "responses_backend: credentials creds/openai: no such ref"


def test_the_owned_key_refusal_text_is_exact():
    with pytest.raises(ValueError) as e:
        ResponsesProvider(EP, "m", key=_key, sampling={"store": True, "model": "x"})
    assert str(e.value) == (
        "responses_backend: `sampling` may not set model, store — "
        "those are the record's (`model`, `max_tokens`) or the provider's own"
    )


async def test_the_retry_backoff_and_its_log_line(monkeypatch, capsys):
    import asyncio

    delays: list = []
    real_sleep = asyncio.sleep

    async def fake_sleep(d, *a, **kw):
        delays.append(d)
        await real_sleep(0)

    monkeypatch.setattr(asyncio, "sleep", fake_sleep)
    sink: list = []
    p = _seq_provider(
        [
            (503, "unavailable", "text/plain"),
            (524, "<!DOCTYPE html>", "text/html"),
            (200, _text("recovered"), "text/event-stream"),
        ],
        sink,
    )
    assert await _drain(p) == "recovered"
    assert len(sink) == 3
    assert delays == [1.5, 3.0]
    out = capsys.readouterr().out
    assert "[responses] 503 (text/plain) — transport error, retry 1/2" in out
    assert "[responses] 524 (text/html) — transport error, retry 2/2" in out


async def test_a_json_5xx_is_never_retried():
    sink: list = []
    p = _seq_provider([(502, '{"error": "x"}', "application/json")], sink)
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert str(e.value) == 'responses 502: {"error": "x"}'
    assert len(sink) == 1


async def test_the_wire_trace_prints_what_left_and_what_came_back(monkeypatch, capsys):
    monkeypatch.setenv("FANTASTIC_AI_WIRE", "1")
    await _drain(_provider([], _text("hi")))
    out = capsys.readouterr().out
    assert "[wire] -> max_output_tokens=8192 items=3" in out
    assert "[wire] <- status='completed' reason=None items=['message']" in out


# ── the key never leaves in an error text ────────────────────────────────────


async def _listener():
    """A 127.0.0.1 listener that reads and closes — a request never gets far."""
    import asyncio

    async def handle(r, w):
        try:
            await asyncio.wait_for(r.read(4096), 1)
        except Exception:
            pass
        w.close()

    srv = await asyncio.start_server(handle, "127.0.0.1", 0)
    return srv, srv.sockets[0].getsockname()[1]


# ── link failures are transport (#198) ──────────────────────────────────────


class _BrokenStream(httpx.AsyncByteStream):
    """One delta, then the link breaks mid-answer."""

    async def __aiter__(self):
        yield b'data: {"type": "response.output_text.delta", "delta": "par"}\n\n'
        raise httpx.ReadError("the peer went away")


def _raising(exc: Exception):
    def handler(req: httpx.Request) -> httpx.Response:
        raise exc

    return httpx.MockTransport(handler)


@pytest.mark.parametrize(
    "exc", [httpx.ConnectError("refused"), httpx.ConnectTimeout("slow")]
)
async def test_a_connect_failure_is_transport(exc):
    from ai_core.provider import ProviderError

    p = ResponsesProvider(endpoint=EP, model="m", key=_key, transport=_raising(exc))
    with pytest.raises(ProviderError) as e:
        await _drain(p)
    assert e.value.transport is True
    assert str(e.value) == f"http: {exc}"


async def test_a_stream_dropped_mid_answer_is_transport():
    from ai_core.provider import ProviderError

    t = httpx.MockTransport(lambda r: httpx.Response(200, stream=_BrokenStream()))
    p = ResponsesProvider(endpoint=EP, model="m", key=_key, transport=t)
    with pytest.raises(ProviderError) as e:
        await _drain(p)
    assert e.value.transport is True
    assert str(e.value) == "http: the peer went away"


@pytest.mark.parametrize("code", [502, 503, 504])
async def test_a_gateway_status_is_transport(code):
    from ai_core.provider import ProviderError

    sink: list = []
    p = _seq_provider([(code, '{"error": "x"}', "application/json")], sink)
    with pytest.raises(ProviderError) as e:
        await _drain(p)
    assert e.value.transport is True
    assert str(e.value) == f'responses {code}: {{"error": "x"}}'


@pytest.mark.parametrize("code", [400, 401, 404, 500])
async def test_a_status_the_endpoint_meant_is_not_transport(code):
    from ai_core.provider import ProviderError

    sink: list = []
    p = _seq_provider([(code, "no", "text/plain")], sink)
    with pytest.raises(ProviderError) as e:
        await _drain(p)
    assert e.value.transport is False
    assert str(e.value) == f"responses {code}: no"


# ── the read clock (#177) ───────────────────────────────────────────────────


async def test_a_stalled_responses_stream_is_stream_stalled():
    from ai_core.provider import ProviderError

    class _Silent(httpx.AsyncByteStream):
        async def __aiter__(self):
            yield b'data: {"type": "response.output_text.delta", "delta": "par"}\n\n'
            raise httpx.ReadTimeout("no byte")

    t = httpx.MockTransport(lambda r: httpx.Response(200, stream=_Silent()))
    p = ResponsesProvider(
        endpoint=EP, model="m", key=_key, transport=t, stream_idle=0.2
    )
    with pytest.raises(ProviderError) as e:
        await _drain(p)
    assert e.value.transport is True
    assert str(e.value) == (
        f"responses_backend: stream_stalled — {EP} sent no bytes for 0.2s; "
        "this turn is NOT complete"
    )


async def test_the_stream_runs_on_the_read_clock():
    seen: list = []

    def handler(req: httpx.Request) -> httpx.Response:
        seen.append(req.extensions["timeout"])
        return httpx.Response(200, text=_text("ok"))

    t = httpx.MockTransport(handler)
    await _drain(ResponsesProvider(endpoint=EP, model="m", key=_key, transport=t))
    await _drain(
        ResponsesProvider(
            endpoint=EP, model="m", key=_key, transport=t, stream_idle=4.5
        )
    )
    assert seen[0]["read"] == 60.0 and seen[1]["read"] == 4.5
    assert seen[1]["connect"] == 15.0


def test_stream_idle_seconds_is_validated_at_the_write():
    from responses_backend.tools import RECORD_SCHEMA, validate_meta

    rec = {
        "endpoint": EP,
        "model": "m",
        "memory_store_id": "memory",
        "credentials": {"store": "s", "ref": "r"},
    }
    for bad in (0, -1, "x"):
        msg = validate_meta({"stream_idle_seconds": bad}, rec)
        assert msg and msg.startswith(
            "stream_idle_seconds must be a positive number of seconds"
        )
    assert validate_meta({"stream_idle_seconds": 2.5}, rec) is None
    assert "stream_idle_seconds" in [f["name"] for f in RECORD_SCHEMA["fields"]]


# ── Rust twins (#170) ────────────────────────────────────────────────────────

_REC = {
    "endpoint": EP,
    "model": "m",
    "memory_store_id": "memory",
    "credentials": {"store": "s", "ref": "r"},
}


async def test_a_503_is_a_transport_failure_and_a_401_is_not():
    from ai_core.provider import ProviderError

    with pytest.raises(ProviderError) as gw:
        await _drain(_seq_provider([(503, "overloaded", "text/plain")], []))
    assert gw.value.transport is True
    assert str(gw.value).startswith("responses 503")
    with pytest.raises(ProviderError) as deliberate:
        await _drain(_seq_provider([(401, "no", "text/plain")], []))
    assert deliberate.value.transport is False
    assert str(deliberate.value) == "responses 401: no"


async def test_a_rotated_key_rides_the_next_request_and_nothing_pools(monkeypatch):
    made: list = []
    real_init = httpx.AsyncClient.__init__

    def counting(self, *a, **kw):
        made.append(1)
        real_init(self, *a, **kw)

    monkeypatch.setattr(httpx.AsyncClient, "__init__", counting)
    current = {"key": "sk-pad\tinner"}
    asked: list = []

    async def reader(endpoint: str) -> str:
        asked.append(endpoint)
        return current["key"]

    sink: list = []
    p = _provider(sink, _text("ok"), key=reader)
    await _drain(p)
    assert sink[0].headers["authorization"] == "Bearer sk-pad\tinner"
    assert sink[0].headers["content-type"] == "application/json"
    current["key"] = "sk-next"
    await _drain(p)
    assert sink[1].headers["authorization"] == "Bearer sk-next"
    assert asked == [EP, EP]
    assert len(made) == 2


async def test_transport_retry_is_bounded_and_deliberate_answers_are_not_retried():
    sink: list = []
    p = _seq_provider(
        [
            (521, "<!DOCTYPE html>", "text/html"),
            (200, _text("recovered"), "text/event-stream"),
        ],
        sink,
    )
    assert await _drain(p) == "recovered"
    assert len(sink) == 2
    sink = []
    p = _seq_provider([(521, "<!DOCTYPE html>", "text/html")], sink)
    with pytest.raises(RuntimeError, match="responses 521"):
        await _drain(p)
    assert len(sink) == RETRIES + 1
    sink = []
    p = _seq_provider([(404, '{"error": "not found"}', "application/json")], sink)
    with pytest.raises(RuntimeError, match="responses 404"):
        await _drain(p)
    assert len(sink) == 1


def test_a_record_without_an_endpoint_is_refused_at_the_write_naming_it():
    from responses_backend.tools import validate_meta

    assert validate_meta({}, _REC) is None
    assert validate_meta({"endpoint": ""}, _REC) == (
        "responses_backend: required on the record and missing: endpoint — "
        "nothing is guessed. endpoint = the full Responses API URL (…/v1/responses)"
    )


def test_caching_none_and_cleanup_pass_and_provider_cache_is_refused_naming_responses_backend():
    from responses_backend.tools import validate_meta

    for mode in ("none", "cleanup"):
        assert validate_meta({"caching": mode}, _REC) is None
    assert validate_meta({"caching": "provider_cache"}, _REC) == (
        "responses_backend cannot do provider caching: "
        "caching=provider_cache is refused — use none or cleanup"
    )
    assert validate_meta({}, {**_REC, "caching": "provider_cache"}) is not None
    assert validate_meta({"caching": "always"}, _REC) == (
        "caching='always' is not one of the legal values: "
        "none, cleanup, provider_cache"
    )


# ── twins of the rust responses_backend tests (#170) ─────────────────────────


def test_to_responses_matches_python():
    from responses_backend.provider import _to_responses

    ins, items = _to_responses(
        [
            {"role": "system", "content": "first"},
            {"role": "system", "content": "last wins"},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": ""},
            {"role": "tool", "content": "dropped"},
        ]
    )
    assert ins == "last wins"
    assert items == [
        {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hi"}],
        },
        {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "(no content)"}],
        },
    ]
    ins, _ = _to_responses([{"role": "system", "content": ""}])
    assert ins is None  # a falsy system block means no `instructions` key


async def _make_responses(kernel, transport):
    """A responses agent on the real provider, its transport and key stubbed."""
    from _testkit import ensure_memory

    store = ensure_memory(kernel)
    can_call = {"cli": "*", store: "*"}
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "responses_backend.tools",
                        "endpoint": EP,
                        "model": "test-model",
                        "can_call": can_call,
                        "memory_store_id": store,
                        "credentials": {"store": "s", "ref": "r"},
                        "stream_idle_seconds": 0.2,
                    },
                }
            ]
        )
    )[0]["result"]
    aid = rec["id"]
    for target in can_call:
        tgt = kernel.ctx.agents.get(target)
        cb = dict(tgt._meta.get("callable_by") or {}) if tgt is not None else {}
        cb[aid] = "*"
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "update_agent", "id": target, "callable_by": cb},
                }
            ]
        )
    return aid


@pytest.fixture
def stubbed_provider(monkeypatch):
    """Hand every kernel-built ResponsesProvider a transport and a key."""
    from responses_backend import provider as rp
    from responses_backend import tools as rt

    box: dict = {}
    real_init = rp.ResponsesProvider.__init__

    def init(self, *a, **kw):
        kw["transport"] = box["transport"]
        real_init(self, *a, **kw)

    monkeypatch.setattr(rp.ResponsesProvider, "__init__", init)
    monkeypatch.setattr(rt, "key_reader", lambda *a, **kw: _key)
    return box


async def test_reflect_sentence_is_python_verbatim(
    seeded_kernel, store_agent, stubbed_provider
):
    stubbed_provider["transport"] = None
    aid = await _make_responses(seeded_kernel, None)
    r = (await seeded_kernel.send([{"target": aid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert r["sentence"] == (
        "OpenAI Responses-API LLM agent (raw prompt-and-parse tool-calling)."
    )
    assert r["model"] == "test-model"
    assert r["endpoint"] == EP


async def test_send_streams_tokens_raw_on_both_channels(
    seeded_kernel, store_agent, stubbed_provider
):
    sse = _sse(
        {"type": "response.created", "response": {"status": "in_progress"}},
        {"type": "response.reasoning_text.delta", "delta": "pondering"},
        {"type": "response.output_text.delta", "delta": "hel"},
        {"type": "response.output_text.delta", "delta": "lo"},
        _done(),
    )
    stubbed_provider["transport"] = httpx.MockTransport(
        lambda req: httpx.Response(200, text=sse)
    )
    aid = await _make_responses(seeded_kernel, None)
    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(events.append)
    try:
        r = (
            await seeded_kernel.send(
                [{"target": aid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
    finally:
        unsub()
    tokens = "".join(
        e["payload"]["text"]
        for e in events
        if e.get("kind") == "send"
        and e["agent_id"] == "cli"
        and e["payload"].get("type") == "token"
    )
    assert tokens == "<think>pondering</think>hello", "streamed raw, both channels"
    # the reply is the RAW text; only the stored assistant row is stripped
    assert r.get("final") == "<think>pondering</think>hello", r


async def test_an_endpoint_that_never_answers_fails_after_the_idle_window():
    """No response headers ever arrive: the handler raises ReadTimeout before any
    response, and the turn fails naming the stall (#177)."""
    from ai_core.provider import ProviderError

    def never(req: httpx.Request) -> httpx.Response:
        raise httpx.ReadTimeout("no headers", request=req)

    p = ResponsesProvider(
        endpoint=EP,
        model="m",
        key=_key,
        transport=httpx.MockTransport(never),
        stream_idle=0.2,
    )
    with pytest.raises(ProviderError) as e:
        await _drain(p)
    assert e.value.transport is True
    assert "stream_stalled" in str(e.value)
    assert "sent no bytes for 0.2s" in str(e.value)
