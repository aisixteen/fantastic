"""LlamaCppProvider — the wire it speaks and the thought channel it must not drop.

These assert the two things this bundle exists for: that `reasoning_content` is
read (not silently dropped, which is how a whole turn disappears), and that the
`enable_thinking` gate is sent PER REQUEST so toggling it is a record edit rather
than a server restart.
"""

from __future__ import annotations

import json

import httpx
import pytest
from ai_core.core import THINK_CLOSE, THINK_OPEN
from llamacpp_backend.provider import DEFAULT_MODEL, LlamaCppProvider


def _sse(*deltas: dict) -> str:
    lines = [f"data: {json.dumps({'choices': [{'delta': d}]})}" for d in deltas]
    lines.append("data: [DONE]")
    return "\n\n".join(lines) + "\n\n"


def _capture(body_sink: list, sse: str):
    def handler(request: httpx.Request) -> httpx.Response:
        body_sink.append(json.loads(request.content))
        body_sink.append(dict(request.headers))
        return httpx.Response(200, text=sse)

    return httpx.MockTransport(handler)


async def _drain(provider) -> str:
    return "".join(
        [c async for c in provider.chat([{"role": "user", "content": "hi"}])]
    )


async def test_reasoning_content_is_wrapped_not_dropped():
    """The failure this bundle exists to prevent: a pass spent reasoning read as an
    EMPTY completion. Both channels must survive, thought wrapped exactly once."""
    seen: list = []
    sse = _sse(
        {"reasoning_content": "let me "},
        {"reasoning_content": "think"},
        {"content": "answer"},
    )
    p = LlamaCppProvider(endpoint="http://llama.test/v1", transport=_capture(seen, sse))
    out = await _drain(p)
    assert out == f"{THINK_OPEN}let me think{THINK_CLOSE}answer"
    await p.aclose()


async def test_unterminated_thought_still_closes():
    """A turn that is ALL reasoning (no content) must still close its wrapper —
    otherwise the parser downstream sees an unbalanced envelope."""
    seen: list = []
    p = LlamaCppProvider(
        endpoint="http://llama.test/v1",
        transport=_capture(seen, _sse({"reasoning_content": "hm"})),
    )
    assert await _drain(p) == f"{THINK_OPEN}hm{THINK_CLOSE}"
    await p.aclose()


@pytest.mark.parametrize(
    "think,expected",
    [(True, {"enable_thinking": True}), (False, {"enable_thinking": False})],
)
async def test_think_rides_as_chat_template_kwargs(think, expected):
    """gemma-4 gates its thought channel behind this kwarg (default FALSE). Sent per
    request, so the record — not a server restart — decides."""
    seen: list = []
    p = LlamaCppProvider(
        endpoint="http://llama.test/v1",
        think=think,
        transport=_capture(seen, _sse({"content": "x"})),
    )
    await _drain(p)
    assert seen[0]["chat_template_kwargs"] == expected
    await p.aclose()


async def test_think_unset_omits_the_kwarg_entirely():
    """A model with no thinking gate REJECTS an unknown kwarg, so unset must mean
    absent — not `false`."""
    seen: list = []
    p = LlamaCppProvider(
        endpoint="http://llama.test/v1",
        transport=_capture(seen, _sse({"content": "x"})),
    )
    await _drain(p)
    assert "chat_template_kwargs" not in seen[0]
    await p.aclose()


async def test_model_is_omitted_when_unset_and_reflects_the_sentinel():
    """llama-server serves ONE model and ignores this field, so there is nothing to
    guess. The sentinel is for `reflect` only and must never reach the wire."""
    seen: list = []
    p = LlamaCppProvider(
        endpoint="http://llama.test/v1",
        transport=_capture(seen, _sse({"content": "x"})),
    )
    await _drain(p)
    assert "model" not in seen[0]
    assert p.model == DEFAULT_MODEL
    await p.aclose()


async def test_no_api_key_sends_no_authorization_header():
    """llama-server is keyless unless started with --api-key. We do not invent one."""
    seen: list = []
    p = LlamaCppProvider(
        endpoint="http://llama.test/v1",
        transport=_capture(seen, _sse({"content": "x"})),
    )
    await _drain(p)
    assert "authorization" not in {k.lower() for k in seen[1]}
    await p.aclose()


async def test_api_key_when_set_rides_as_bearer():
    seen: list = []
    p = LlamaCppProvider(
        endpoint="http://llama.test/v1",
        api_key="s3cret",
        transport=_capture(seen, _sse({"content": "x"})),
    )
    await _drain(p)
    assert seen[1]["authorization"] == "Bearer s3cret"
    await p.aclose()


async def test_temperature_defaults_low_and_num_predict_maps_to_max_tokens():
    """An agentic backend must emit exact tool-call JSON, not improvise."""
    seen: list = []
    p = LlamaCppProvider(
        endpoint="http://llama.test/v1",
        num_predict=64,
        transport=_capture(seen, _sse({"content": "x"})),
    )
    await _drain(p)
    assert seen[0]["temperature"] == 0.3
    assert seen[0]["max_tokens"] == 64
    assert seen[0]["stream"] is True
    await p.aclose()


# ─── the wire, pinned (reduction candidate A: one OpenAI SSE loop) ──────────
#
# What THIS provider does today, line by line of the stream. A shared SSE helper
# must keep every one for llamacpp: blank and non-JSON `data:` lines are SKIPPED
# (vllm raises on non-JSON), `delta.reasoning` is NOT read (vllm reads it), and a
# `length` finish is recorded, never raised (vllm raises on a main turn).


def _raw(text: str, sink: list | None = None):
    def handler(request: httpx.Request) -> httpx.Response:
        if sink is not None:
            sink.append(json.loads(request.content))
        return httpx.Response(200, text=text)

    return httpx.MockTransport(handler)


async def test_blank_and_non_json_data_lines_are_skipped_not_raised():
    text = (
        "data: \n\n"
        "data: not-json\n\n"
        ": keep-alive\n\n"
        f"data: {json.dumps({'choices': [{'delta': {'content': 'ok'}}]})}\n\n"
        "data: [DONE]\n\n"
    )
    p = LlamaCppProvider(endpoint="http://llama.test/v1", transport=_raw(text))
    assert await _drain(p) == "ok"
    await p.aclose()


async def test_done_ends_the_stream_and_trailing_data_is_ignored():
    text = (
        f"data: {json.dumps({'choices': [{'delta': {'content': 'first'}}]})}\n\n"
        "data: [DONE]\n\n"
        f"data: {json.dumps({'choices': [{'delta': {'content': 'after'}}]})}\n\n"
    )
    p = LlamaCppProvider(endpoint="http://llama.test/v1", transport=_raw(text))
    assert await _drain(p) == "first"
    await p.aclose()


async def test_usage_and_a_length_finish_are_recorded_never_raised():
    text = (
        "data: "
        + json.dumps(
            {"choices": [{"delta": {"content": "x"}, "finish_reason": "length"}]}
        )
        + "\n\n"
        + "data: "
        + json.dumps({"choices": [], "usage": {"prompt_tokens": 512}})
        + "\n\ndata: [DONE]\n\n"
    )
    p = LlamaCppProvider(endpoint="http://llama.test/v1", transport=_raw(text))
    assert await _drain(p) == "x"
    assert p.last_stop_reason == "max_tokens"
    assert p.last_prompt_tokens == 512
    await p.aclose()


async def test_a_stop_finish_passes_through_verbatim():
    text = (
        "data: "
        + json.dumps(
            {"choices": [{"delta": {"content": "x"}, "finish_reason": "stop"}]}
        )
        + "\n\ndata: [DONE]\n\n"
    )
    p = LlamaCppProvider(endpoint="http://llama.test/v1", transport=_raw(text))
    await _drain(p)
    assert p.last_stop_reason == "stop"
    await p.aclose()


async def test_the_reasoning_alias_is_not_read_by_llamacpp():
    seen: list = []
    p = LlamaCppProvider(
        endpoint="http://llama.test/v1",
        transport=_capture(seen, _sse({"reasoning": "hidden"}, {"content": "ok"})),
    )
    assert await _drain(p) == "ok"
    await p.aclose()


async def test_a_named_model_rides_the_body_and_a_side_bound_beats_num_predict():
    seen: list = []
    p = LlamaCppProvider(
        endpoint="http://llama.test/v1",
        model="gemma-4-12b",
        num_predict=64,
        temperature=0.9,
        transport=_capture(seen, _sse({"content": "x"})),
    )
    out = "".join(
        [c async for c in p.chat([{"role": "user", "content": "hi"}], max_tokens=9)]
    )
    assert out == "x"
    assert seen[0] == {
        "messages": [{"role": "user", "content": "hi"}],
        "stream": True,
        "stream_options": {"include_usage": True},
        "model": "gemma-4-12b",
        "temperature": 0.9,
        "max_tokens": 9,
    }
    assert p.model == "gemma-4-12b"
    await p.aclose()
