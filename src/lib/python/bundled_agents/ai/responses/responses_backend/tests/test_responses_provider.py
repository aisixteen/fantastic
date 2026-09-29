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
    KEY_VAR,
    RETRIES,
    ResponsesProvider,
)

EP = "http://proxy.test/v1/responses"


@pytest.fixture(autouse=True)
def _key(monkeypatch):
    """The key is read before the request is built, so even a MockTransport run
    needs one present. Set here, never read from the developer's real `.env`."""
    monkeypatch.setenv(KEY_VAR, "sk-test-fixture")


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
    `sampling` key naming either is refused, not merged over it (audit d8)."""
    for key in ("store", "model", "max_output_tokens"):
        with pytest.raises(ValueError, match=key):
            ResponsesProvider("http://x/v1/responses", "m", sampling={key: 1})


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


async def test_missing_endpoint_model_or_key_fail_loud_naming_it(monkeypatch):
    with pytest.raises(RuntimeError, match="endpoint"):
        await _drain(ResponsesProvider(endpoint="", model="m"))
    with pytest.raises(RuntimeError, match="model"):
        await _drain(ResponsesProvider(endpoint=EP, model=""))
    monkeypatch.delenv(KEY_VAR)
    with pytest.raises(RuntimeError, match=KEY_VAR):
        await _drain(ResponsesProvider(endpoint=EP, model="m"))


# ── transport retry ──────────────────────────────────────────────────────────
# Same rule as the anthropic provider: a CDN error page (transport code, non-JSON
# body) is retried, bounded; anything the endpoint answered deliberately is not.


def _sequence(responses: list, sink: list):
    def handler(request: httpx.Request) -> httpx.Response:
        sink.append(request)
        status, body, ctype = responses[min(len(sink) - 1, len(responses) - 1)]
        return httpx.Response(status, text=body, headers={"content-type": ctype})

    return httpx.MockTransport(handler)


def _seq_provider(responses: list, sink: list) -> ResponsesProvider:
    return ResponsesProvider(
        endpoint=EP, model="m", transport=_sequence(responses, sink)
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
    """A 404 JSON body — measured 2026-09-27 on a proxy with no /responses route —
    is the endpoint answering; retrying would spend the same broken request N times."""
    sink: list = []
    p = _seq_provider([(404, '{"error": "not found"}', "application/json")], sink)
    with pytest.raises(RuntimeError, match="responses 404"):
        await _drain(p)
    assert len(sink) == 1
