"""vllm_backend — the wire it speaks, the turns it must not lose, and the
KV-cache session table it keeps per endpoint.

Every behaviour here traces to a failure seen on a live vLLM: a dropped thought channel, a turn
spent thinking with no answer, a stream that died without saying so, a system
message the template refused, and a cache that only pays off while the live set
stays under the server's KV ceiling.
"""

from __future__ import annotations

import json

import httpx
import pytest
from ai_core.core import THINK_CLOSE, THINK_OPEN
from vllm_backend import provider as vp
from vllm_backend.provider import (
    KvSlotsFull,
    SessionTable,
    VllmProvider,
    conversation_key,
    one_system_message,
)
from vllm_backend.tools import validate_meta

ENDPOINT = "http://vllm.test/v1"


def _sse(*chunks: dict, done: bool = True) -> str:
    lines = [f"data: {json.dumps(c)}" for c in chunks]
    if done:
        lines.append("data: [DONE]")
    return "\n\n".join(lines) + "\n\n"


def _delta(finish: str | None = None, **delta) -> dict:
    return {"choices": [{"delta": delta, "finish_reason": finish}]}


def _transport(sse: str, sink: list | None = None, status: int = 200):
    def handler(request: httpx.Request) -> httpx.Response:
        if sink is not None:
            sink.append(json.loads(request.content))
        return httpx.Response(status, text=sse)

    return httpx.MockTransport(handler)


async def _nokey(endpoint: str) -> None:
    """The reader of a record with no `credentials`: no key, no header."""
    return None


def _provider(transport, **kw) -> VllmProvider:
    args = dict(
        agent_id="v1",
        endpoint=ENDPOINT,
        key=_nokey,
        model="qwen3.8-27b",
        max_tokens=8000,
        kv_slots=7,
        kv_overflow="evict",
        sampling=dict(vp.SAMPLING_DEFAULTS),
        transport=transport,
    )
    args.update(kw)
    return VllmProvider(**args)


async def _drain(p: VllmProvider, messages=None, **kw) -> str:
    msgs = messages or [{"role": "user", "content": "hi"}]
    return "".join([c async for c in p.chat(msgs, **kw)])


@pytest.fixture(autouse=True)
def _fresh_tables():
    vp._tables.clear()
    vp.last_budgets.clear()
    vp.usage.clear()
    yield
    vp._tables.clear()
    vp.last_budgets.clear()
    vp.usage.clear()


# ─── the stream ──────────────────────────────────────────────────────────────


async def test_wrap_opens_and_closes_once_per_turn():
    sse = _sse(
        _delta(reasoning_content="let me "),
        _delta(reasoning="think"),  # the other spelling a version may use
        _delta(content="answer"),
        _delta(finish="stop"),
        {"choices": [], "usage": {"prompt_tokens": 42}},  # the usage-only chunk
    )
    p = _provider(_transport(sse))
    assert await _drain(p) == f"{THINK_OPEN}let me think{THINK_CLOSE}answer"
    assert (p.last_stop_reason, p.last_prompt_tokens) == ("stop", 42)


class _Pieces(httpx.AsyncByteStream):
    """A body delivered as the given separate writes, like uneven TCP segments."""

    def __init__(self, pieces: list[bytes]):
        self._pieces = pieces

    async def __aiter__(self):
        for piece in self._pieces:
            yield piece


def _chunked_transport(pieces: list[bytes]):
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, stream=_Pieces(pieces))

    return httpx.MockTransport(handler)


async def test_llama_final_chunk_with_timings():
    final = {
        "choices": [{"delta": {}, "finish_reason": "stop"}],
        "timings": {"predicted_n": 1},
        "usage": {"prompt_tokens": 5, "completion_tokens": 1},
    }
    p = _provider(_transport(_sse(_delta(content="ok"), final)))
    assert await _drain(p) == "ok"
    assert p.last_stop_reason == "stop"
    assert p.last_prompt_tokens == 5


async def test_llama_usage_chunk_with_empty_choices_after_finish():
    usage = {"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 1}}
    p = _provider(_transport(_sse(_delta(content="ok"), _delta(finish="stop"), usage)))
    assert await _drain(p) == "ok"
    assert p.last_stop_reason == "stop"


async def test_llama_long_reasoning_run():
    chunks = [_delta(reasoning_content="t")] * 12000
    chunks += [_delta(content="done"), _delta(finish="stop")]
    p = _provider(_transport(_sse(*chunks)))
    assert await _drain(p) == f"{THINK_OPEN}{'t' * 12000}{THINK_CLOSE}done"
    assert p.last_stop_reason == "stop"


async def test_llama_issue_208_synthetic_stream():
    final = {
        "choices": [{"delta": {}, "finish_reason": "stop"}],
        "timings": {"predicted_n": 8893},
        "usage": {"prompt_tokens": 120, "completion_tokens": 8893},
    }
    chunks = [_delta(reasoning_content="t")] * 8892 + [_delta(content="answer"), final]
    data = _sse(*chunks).encode()
    pieces, i, step = [], 0, 1
    while i < len(data):
        end = min(i + step, len(data))
        pieces.append(data[i:end])
        i = end
        step = step * 7 % 997 + 1
    p = _provider(_chunked_transport(pieces))
    out = await _drain(p)
    assert out.startswith(THINK_OPEN)
    assert out.endswith("answer")
    assert p.last_stop_reason == "stop"


async def test_llama_large_and_split_chunks():
    b = _sse(
        _delta(content="ab"), _delta(content="cd"), _delta(finish="stop")
    ).encode()
    split = _provider(_chunked_transport([b[:15], b[15:]]))
    assert await _drain(split) == "abcd"
    whole = _provider(_chunked_transport([b]))
    assert await _drain(whole) == "abcd"


async def test_llama_keepalive_comments():
    chunks = (_delta(content="o"), _delta(content="k"), _delta(finish="stop"))
    c = [json.dumps(x) for x in chunks]
    body = (
        f"data: {c[0]}\n\n: ping\n\ndata: {c[1]}\n\n: ping\n\n"
        f"data: {c[2]}\n\ndata: [DONE]\n\n"
    )
    p = _provider(_transport(body))
    assert await _drain(p) == "ok"
# twin: llama_clean_close_without_trailing_blank_line
# twin: a_last_data_line_without_newline_is_decoded


async def test_a_clean_close_without_a_trailing_blank_line_is_decoded():
    c1 = json.dumps(_delta(content="o"))
    c2 = json.dumps(_delta(content="k", finish="stop"))
    body = f"data: {c1}\n\ndata: {c2}\n\ndata: [DONE]"
    p = _provider(_chunked_transport([body.encode()]))
    assert await _drain(p) == "ok"
    assert p.last_stop_reason == "stop"
    # a last data line with no newline and no [DONE]
    last = f"data: {json.dumps(_delta(finish='stop', content='answer'))}"
    p = _provider(_chunked_transport([last.encode()]))
    assert await _drain(p) == "answer"


async def test_llama_crlf_line_endings():
    lf = _sse(_delta(content="ok"), _delta(finish="stop"))
    out_lf = await _drain(_provider(_transport(lf)))
    out_crlf = await _drain(_provider(_transport(lf.replace("\n", "\r\n"))))
    assert out_crlf == "ok"
    assert out_crlf == out_lf


async def test_a_cut_main_turn_fails_naming_the_cut():
    # the record's bound hit on a MAIN turn: it did not finish — never an answer
    p = _provider(_transport(_sse(_delta(content="half"), _delta(finish="length"))))
    with pytest.raises(RuntimeError, match=r"cut at max_tokens=8000"):
        await _drain(p)
    assert p.last_stop_reason == "max_tokens"


async def test_a_cut_bounded_side_call_is_its_answer():
    # a fold summary passes its own bound: stopping there is what it asked for
    p = _provider(_transport(_sse(_delta(content="half"), _delta(finish="length"))))
    assert await _drain(p, max_tokens=50) == "half"
    assert p.last_stop_reason == "max_tokens"


async def test_a_stream_cut_before_finish_is_still_dropped():
    # vLLM died mid-answer: the text so far must never read as a completion
    p = _provider(_transport(_sse(_delta(content="partial"), done=False)))
    with pytest.raises(RuntimeError, match="without a finish_reason"):
        await _drain(p)


async def test_a_refused_connection_is_a_transport_error():
    def handler(request):
        raise httpx.ConnectError("refused")

    p = _provider(httpx.MockTransport(handler))
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert e.value.transport is True


async def test_a_stream_without_a_finish_is_a_transport_error():
    p = _provider(_transport(_sse(_delta(content="partial"), done=False)))
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert e.value.transport is True


class _Broken(httpx.AsyncByteStream):
    """A body that yields the given SSE bytes, then fails the way a socket does."""

    def __init__(self, sse: str, exc: Exception):
        self._sse = sse.encode()
        self._exc = exc

    async def __aiter__(self):
        yield self._sse
        raise self._exc


def _breaking_transport(sse: str, exc: Exception):
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, stream=_Broken(sse, exc))

    return httpx.MockTransport(handler)


@pytest.mark.parametrize(
    "exc",
    [httpx.ReadError("reset"), httpx.RemoteProtocolError("reset")],
)
async def test_a_body_error_after_finish_reason_is_a_normal_answer(exc):
    # the tunnel closes the chunked body after vLLM said the answer was finished
    sse = _sse(
        _delta(content="answer"),
        _delta(finish="stop"),
        {"choices": [], "usage": {"prompt_tokens": 42}},
        done=False,
    )
    p = _provider(_breaking_transport(sse, exc))
    assert await _drain(p) == "answer"
    assert (p.last_stop_reason, p.last_prompt_tokens) == ("stop", 42)


async def test_a_main_turn_cut_still_fails_when_the_body_errors_after_length_finish():
    sse = _sse(_delta(content="half"), _delta(finish="length"), done=False)
    p = _provider(_breaking_transport(sse, httpx.ReadError("reset")))
    with pytest.raises(RuntimeError, match=r"cut at max_tokens=8000"):
        await _drain(p)
    assert p.last_stop_reason == "max_tokens"


async def test_a_bounded_side_call_cut_after_length_finish_is_still_its_answer():
    sse = _sse(_delta(content="half"), _delta(finish="length"), done=False)
    p = _provider(_breaking_transport(sse, httpx.ReadError("reset")))
    assert await _drain(p, max_tokens=50) == "half"
    assert p.last_stop_reason == "max_tokens"


async def test_a_body_error_before_finish_reason_still_drops():
    sse = _sse(_delta(content="partial"), done=False)
    p = _provider(_breaking_transport(sse, httpx.ReadError("reset")))
    with pytest.raises(RuntimeError, match="died or restarted") as e:
        await _drain(p)
    assert e.value.transport is True


async def test_a_stream_dropped_mid_answer_says_vllm_died():
    sse = _sse(_delta(content="partial"), done=False)
    err = httpx.ReadError("connection reset by peer")
    p = _provider(_breaking_transport(sse, err))
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert "(cause: connection reset by peer)" in str(e.value)
    assert "; this turn is NOT complete" in str(e.value)


async def test_a_timeout_after_finish_is_still_a_stall():
    sse = _sse(_delta(content="answer"), _delta(finish="stop"), done=False)
    p = _provider(_breaking_transport(sse, httpx.ReadTimeout("slow")))
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert str(e.value) == (
        "vllm_backend: stream_stalled — http://vllm.test/v1 sent no bytes for 900s; "
        "this turn is NOT complete"
    )


# twin: a_503_is_a_transport_error
@pytest.mark.parametrize("code", [502, 503, 504])
async def test_a_gateway_status_is_transport(code):
    p = _provider(_transport('{"error":"down"}', status=code))
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert e.value.transport is True


@pytest.mark.parametrize("code", [400, 500])
async def test_an_api_400_is_not_a_transport_error(code):
    p = _provider(_transport('{"error":"bad"}', status=code))
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert e.value.transport is False


async def test_an_http_error_names_the_endpoint_and_the_body():
    p = _provider(_transport('{"error":"bad effort"}', status=400))
    with pytest.raises(RuntimeError, match=r"answered HTTP 400: .*bad effort"):
        await _drain(p)


@pytest.mark.parametrize(
    "exc, words",
    [
        (httpx.ConnectError("refused"), "not listening yet"),
        (httpx.ConnectTimeout("syn"), "instance is STOPPED"),
        (httpx.RemoteProtocolError("reset"), "died or restarted"),
    ],
)
async def test_a_refused_connection_says_vllm_is_not_listening_yet(exc, words):
    def handler(request):
        raise exc

    p = _provider(httpx.MockTransport(handler))
    with pytest.raises(RuntimeError, match=words):
        await _drain(p)


async def test_native_tool_calls_become_call_text():
    sse = _sse(
        _delta(
            tool_calls=[
                {
                    "index": 0,
                    "function": {"name": "send", "arguments": '{"target_id":"files",'},
                }
            ]
        ),
        _delta(
            tool_calls=[{"index": 0, "function": {"arguments": '"type":"reflect"}'}}]
        ),
        _delta("tool_calls"),
    )
    p = _provider(_transport(sse))
    assert (
        await _drain(p)
        == '<call name="send"><target_id>files</target_id><type>reflect</type></call>'
    )


async def test_a_native_tool_call_with_bad_arguments_is_an_error():
    sse = _sse(
        _delta(
            tool_calls=[
                {"index": 0, "function": {"name": "send", "arguments": "not json"}}
            ]
        ),
        _delta("tool_calls"),
    )
    p = _provider(_transport(sse))
    with pytest.raises(
        RuntimeError, match=r"native tool call 'send'.*not a JSON object"
    ):
        await _drain(p)


# ─── the request ─────────────────────────────────────────────────────────────


async def test_the_body_carries_the_defaults_and_only_what_the_record_set():
    sink: list = []
    sampling = dict(vp.SAMPLING_DEFAULTS, seed=7)
    p = _provider(_transport(_sse(_delta(finish="stop")), sink), sampling=sampling)
    await _drain(p)
    body = sink[0]
    assert body["model"] == "qwen3.8-27b" and body["max_tokens"] == 8000
    assert body["stream"] is True
    assert body["stream_options"] == {"include_usage": True}
    assert (body["temperature"], body["top_p"], body["top_k"]) == (0.2, 0.8, 20)
    assert body["reasoning_effort"] == "medium" and body["seed"] == 7
    # never defaulted: a penalty is sent only when the record sets one
    assert "presence_penalty" not in body and "frequency_penalty" not in body


async def test_a_cap_that_is_the_whole_window_sends_no_max_tokens():
    """vLLM counts prompt AND output against one window: `max_tokens` equal to the
    window is refused (HTTP 400). A turn capped at the window asks for the rest of
    it; the body carries no `max_tokens`, and a side call keeps its own bound."""
    sink: list = []
    p = _provider(
        _transport(_sse(_delta(finish="stop")), sink), max_tokens=262144, window=262144
    )
    await _drain(p)  # the turn
    await _drain(p, max_tokens=2000)  # a fold summary
    assert "max_tokens" not in sink[0]
    assert sink[1]["max_tokens"] == 2000
    sink.clear()
    q = _provider(
        _transport(_sse(_delta(finish="stop")), sink), max_tokens=8000, window=262144
    )
    await _drain(q)
    assert sink[0]["max_tokens"] == 8000  # a cap below the window is sent as set


async def test_a_cap_the_prompt_leaves_no_room_for_sends_no_max_tokens():
    """A max_tokens near the window size plus any prompt is refused by vLLM ("a total
    of at least … tokens", HTTP 400). When the prompt's token BOUND (`prompt_bound`: its bytes + a
    per-message allowance) plus the cap overruns the window, the turn asks for the
    rest of it; a prompt that leaves the room keeps the cap."""
    sink: list = []
    p = _provider(
        _transport(_sse(_delta(finish="stop")), sink), max_tokens=64000, window=65536
    )
    big = [{"role": "user", "content": "x" * 6000}]
    await _drain(p, big)
    assert "max_tokens" not in sink[0]
    assert vp.prompt_bound(big) == len("user") + 6000 + vp.MESSAGE_OVERHEAD
    sink.clear()
    await _drain(p, [{"role": "user", "content": "hi"}])
    assert sink[0]["max_tokens"] == 64000  # 64000 + the bound fits 65536


async def test_every_system_message_becomes_one_at_index_0():
    msgs = [
        {"role": "system", "content": "A"},
        {"role": "user", "content": "q"},
        {"role": "system", "content": "B"},
    ]
    assert one_system_message(msgs) == [
        {"role": "system", "content": "A\n\nB"},
        {"role": "user", "content": "q"},
    ]
    sink: list = []
    p = _provider(_transport(_sse(_delta(finish="stop")), sink))
    await _drain(p, messages=msgs)
    assert [m["role"] for m in sink[0]["messages"]] == ["system", "user"]


async def test_the_default_budget_halves_on_a_smaller_call_and_none_below_the_floor():
    sink: list = []
    p = _provider(_transport(_sse(_delta(finish="stop")), sink))
    await _drain(p)  # the turn: max_tokens 8000 → min(16000, 4000)
    await _drain(p, max_tokens=2000)  # a fold summary → min(16000, 1000)
    await _drain(p, max_tokens=40)  # half is 20, under the 256 floor → none
    assert [b.get("thinking_token_budget") for b in sink] == [4000, 1000, None]
    assert vp.last_budgets["v1"] is None


async def test_an_explicit_budget_rules_the_turn_and_halves_on_a_side_call():
    # the proxy runs' 8000: a main turn sends it as set; a fold summary bounded
    # at 2000 gets 1000 instead of being refused; a tiny call gets none
    sink: list = []
    sampling = dict(vp.SAMPLING_DEFAULTS, thinking_token_budget=8000)
    p = _provider(
        _transport(_sse(_delta(finish="stop")), sink),
        sampling=sampling,
        explicit_budget=True,
        max_tokens=65536,
    )
    await _drain(p)
    await _drain(p, max_tokens=2000)
    await _drain(p, max_tokens=300)
    assert [b.get("thinking_token_budget") for b in sink] == [8000, 1000, None]


async def test_the_budget_goes_out_under_the_dialects_name():
    for d in vp.DIALECTS:
        sink: list = []
        p = _provider(
            _transport(_sse(_delta(finish="stop")), sink),
            sampling=dict(vp.SAMPLING_DEFAULTS, thinking_token_budget=4000),
            explicit_budget=True,
            dialect=d,
        )
        await _drain(p)
        body = sink[0]
        if d == "vllm":
            assert body["thinking_token_budget"] == 4000
            assert "reasoning_budget_tokens" not in body
        elif d == "strata":
            assert body["reasoning_budget_tokens"] == 4000
            assert "thinking_token_budget" not in body
        else:
            assert "thinking_token_budget" not in body
            assert "reasoning_budget_tokens" not in body


async def test_no_dialect_given_sends_the_vllm_name():
    sink: list = []
    p = _provider(
        _transport(_sse(_delta(finish="stop")), sink),
        sampling=dict(vp.SAMPLING_DEFAULTS, thinking_token_budget=4000),
        explicit_budget=True,
    )
    await _drain(p)
    assert sink[0]["thinking_token_budget"] == 4000
    assert "reasoning_budget_tokens" not in sink[0]


# ─── the KV-cache session table ──────────────────────────────────────────────


def test_evict_rotates_the_least_recently_used_conversation_out():
    t = SessionTable()
    for i, k in enumerate(["a", "b", "c"]):
        assert t.admit(k, 3, "evict", 1800, now=i) is None
    t.admit("a", 3, "evict", 1800, now=10)  # a is used again: b is now the idlest
    assert t.admit("d", 3, "evict", 1800, now=11) == "b"
    assert list(t.live) == ["c", "a", "d"] and t.evicted == 1


def test_reject_refuses_the_newcomer_and_names_when_to_retry():
    t = SessionTable()
    t.admit("a", 1, "reject", 1800, now=0)
    with pytest.raises(KvSlotsFull, match=r"retry in ~1700s"):
        t.admit("b", 1, "reject", 1800, now=100)
    assert list(t.live) == ["a"] and t.rejected == 1


def test_an_idle_conversation_frees_its_slot_after_the_ttl():
    t = SessionTable()
    t.admit("a", 1, "reject", 60, now=0)
    assert t.admit("b", 1, "reject", 60, now=61) is None
    assert list(t.live) == ["b"]


async def test_the_table_is_shared_per_endpoint_across_agents():
    # two agents on ONE endpoint share the count; a second endpoint does not
    ok = _transport(_sse(_delta(finish="stop")))
    p1 = _provider(ok, kv_slots=1, kv_overflow="reject")
    p2 = _provider(ok, kv_slots=1, kv_overflow="reject")
    other = _provider(
        ok, endpoint="http://other.test/v1", kv_slots=1, kv_overflow="reject"
    )
    await _drain(p1, messages=[{"role": "user", "content": "first"}])
    with pytest.raises(KvSlotsFull):
        await _drain(p2, messages=[{"role": "user", "content": "second"}])
    await _drain(other, messages=[{"role": "user", "content": "second"}])
    # the SAME conversation on the full endpoint is already live: admitted
    await _drain(p2, messages=[{"role": "user", "content": "first"}])


async def test_side_calls_take_no_kv_slot():
    # a fold summary is one request, not a conversation kept warm: three of them
    # never fill the one slot, and the main turn after them is still admitted
    p = _provider(
        _transport(_sse(_delta(finish="stop"))),
        kv_slots=1,
        kv_overflow="reject",
    )
    for i in range(3):
        await _drain(
            p,
            messages=[{"role": "user", "content": f"summarize part {i}"}],
            max_tokens=64,
        )
    await _drain(p, messages=[{"role": "user", "content": "go"}])
    assert vp.table_for(ENDPOINT).view()["conversations"] == [
        conversation_key([{"role": "user", "content": "go"}])
    ]


async def test_a_side_call_leaves_the_slot_table_unchanged():
    # two live conversations fill the 2-slot table; a fold summary and a repeat
    # of a live conversation's text, both bounded, move no stamp and add no key
    p = _provider(
        _transport(_sse(_delta(finish="stop"))),
        kv_slots=2,
        kv_overflow="evict",
    )
    for text in ("first", "second"):
        await _drain(p, messages=[{"role": "user", "content": text}])
    table = vp.table_for(ENDPOINT)
    view_before = table.view()
    live_before = list(table.live.items())
    assert view_before["evicted"] == 0 and len(live_before) == 2

    fold = [{"role": "user", "content": "summarize part 3"}]
    await _drain(p, messages=fold, max_tokens=64)
    await _drain(
        p, messages=[{"role": "user", "content": "first"}], max_tokens=64
    )

    assert table.view() == view_before
    assert list(table.live.items()) == live_before
    assert conversation_key(fold) not in table.live


def test_a_conversation_is_its_prefix():
    base = [{"role": "system", "content": "S"}, {"role": "user", "content": "U"}]
    grown = [
        *base,
        {"role": "assistant", "content": "A"},
        {"role": "user", "content": "U2"},
    ]
    assert conversation_key(base) == conversation_key(grown)  # append-only: same
    rewritten = [{"role": "system", "content": "S2"}, *base[1:]]
    assert conversation_key(rewritten) != conversation_key(base)


# ─── the write ───────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "meta, record, fragment",
    [
        ({"reasoning_effort": "high"}, {}, "legal values: low, medium, xhigh"),
        ({"kv_overflow": "drop"}, {}, "legal values: evict"),
        ({"kv_slots": 0}, {}, "kv_slots must be a positive int"),
        ({"max_tokens": True}, {}, "max_tokens must be a positive int"),
        ({"kv_ttl": -1}, {}, "kv_ttl must be a positive number"),
        (
            {"thinking_token_budget": 8000},
            {"max_tokens": 8000},
            "not below max_tokens=8000",
        ),
        (
            {"max_tokens": 1000},
            {"thinking_token_budget": 2000},
            "not below max_tokens=1000",
        ),
        (
            {"server_dialect": "bogus"},
            {},
            "server_dialect must be one of vllm, strata, llamacpp",
        ),
        (
            {"server_dialect": "llamacpp", "thinking_token_budget": 4000},
            {},
            "llama.cpp takes the reasoning budget as a server flag",
        ),
        (
            {"thinking_token_budget": 4000},
            {"server_dialect": "llamacpp"},
            "llama.cpp takes the reasoning budget as a server flag",
        ),
    ],
)
# twin: validate_refuses_a_bad_dialect_and_a_llamacpp_budget
def test_a_write_the_backend_cannot_honour_is_refused(meta, record, fragment):
    assert fragment in (validate_meta(meta, record) or "")


def test_a_record_missing_required_fields_is_refused_at_the_write_naming_all():
    # what a model spawning a sub-AI wrote (E37): the fields it could see
    meta = {"endpoint": ENDPOINT, "model": "qwen3.8-27b", "max_tokens": 65536}
    err = validate_meta(meta, {})
    assert err and "missing: kv_overflow, memory_store_id —" in err, err
    # each missing field is named WITH its hint, from the schema
    assert "kv_overflow = all slots live: evict" in err, err
    # an update to a complete record is judged on the MERGED state
    full = {
        **meta,
        "kv_overflow": "evict",
        "memory_store_id": "memory",
    }
    assert validate_meta({"seed": 3}, full) is None


def test_a_consistent_write_passes():
    meta = {
        "endpoint": ENDPOINT,
        "model": "qwen3.8-27b",
        "max_tokens": 8000,
        "context_window": 262144,
        "kv_slots": 6,
        "kv_overflow": "evict",
        "thinking_token_budget": 4000,
        "reasoning_effort": "xhigh",
        "memory_store_id": "memory",
    }
    assert validate_meta(meta, {}) is None


def test_caching_none_and_cleanup_pass_and_provider_cache_is_refused_naming_vllm_backend():
    rec = {"endpoint": ENDPOINT, "kv_overflow": "evict", "memory_store_id": "memory"}
    for mode in ("none", "cleanup"):
        assert validate_meta({"caching": mode}, rec) is None
    refusal = validate_meta({"caching": "provider_cache"}, rec)
    assert refusal is not None
    assert refusal == (
        "vllm_backend cannot do provider caching: "
        "caching=provider_cache is refused — use none or cleanup"
    )
    assert validate_meta({}, {**rec, "caching": "provider_cache"}) is not None
    legal = validate_meta({"caching": "always"}, rec)
    assert legal is not None and "not one of the legal values" in legal


async def test_the_final_usage_chunk_adds_one_request_and_its_tokens():
    sse = _sse(
        _delta(content="hi"),
        _delta(finish="stop"),
        {"choices": [], "usage": {"prompt_tokens": 120, "completion_tokens": 30}},
    )
    p = _provider(_transport(sse))
    await _drain(p)
    assert vp.usage["v1"] == {"requests": 1, "prompt_tokens": 120, "output_tokens": 30}


def test_a_strata_budget_write_passes():
    meta = {
        "endpoint": ENDPOINT,
        "model": "qwen3.8-27b",
        "max_tokens": 8000,
        "context_window": 262144,
        "kv_slots": 6,
        "kv_overflow": "evict",
        "thinking_token_budget": 4000,
        "reasoning_effort": "xhigh",
        "memory_store_id": "memory",
        "server_dialect": "strata",
    }
    assert validate_meta(meta, {}) is None


# ─── through the kernel ──────────────────────────────────────────────────────


async def test_the_usage_verb_shows_the_counts_reset_zeroes_them_and_delete_drops_them(
    seeded_kernel,
):
    k = seeded_kernel
    r = await _create(
        k,
        id="v1",
        endpoint=ENDPOINT,
        model="qwen3.8-27b",
        max_tokens=8000,
        context_window=262144,
        kv_slots=7,
        kv_overflow="evict",
        memory_store_id="memory",
    )
    assert r.get("id") == "v1", r
    vp.usage["v1"] = {"requests": 2, "prompt_tokens": 10, "output_tokens": 5}

    def call(payload):
        return k.send([{"target": "v1", "payload": payload}])

    got = (await call({"type": "usage"}))[0]["result"]
    assert got == {"requests": 2, "prompt_tokens": 10, "output_tokens": 5}, got

    out = (await call({"type": "reflect"}))[0]["result"]
    assert out["usage"] == {"requests": 2, "prompt_tokens": 10, "output_tokens": 5}, out

    got = (await call({"type": "usage", "reset": True}))[0]["result"]
    assert got == {"requests": 2, "prompt_tokens": 10, "output_tokens": 5}, got
    assert vp.usage["v1"] == {"requests": 0, "prompt_tokens": 0, "output_tokens": 0}

    bad = (await call({"type": "usage", "reset": "yes"}))[0]["result"]
    assert bad.get("reason") == "bad_arg" and bad.get("arg") == "reset", bad

    await k.send([{"target": k.id, "payload": {"type": "delete_agent", "id": "v1"}}])
    assert "v1" not in vp.usage


async def _create(kernel, **meta):
    payload = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "handler_module": "vllm_backend.tools",
        **meta,
    }
    return (await kernel.send([{"target": kernel.id, "payload": payload}]))[0]["result"]


async def test_the_kernel_refuses_a_bad_record_at_the_write(seeded_kernel):
    r = await _create(
        seeded_kernel, id="bad", max_tokens=4000, thinking_token_budget=4000
    )
    assert r.get("reason") == "invalid_meta", r
    assert "not below max_tokens=4000" in r["error"], r


async def test_reflect_shows_the_effective_posture_and_the_shared_table(seeded_kernel):
    k = seeded_kernel
    r = await _create(
        k,
        id="v1",
        endpoint=ENDPOINT,
        model="qwen3.8-27b",
        max_tokens=8000,
        context_window=262144,
        kv_slots=7,
        kv_overflow="evict",
        memory_store_id="memory",
        seed=3,
    )
    assert r.get("id") == "v1", r
    out = (await k.send([{"target": "v1", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    # WHERE its key is read: absent here, shown as null (a pointer, never a secret).
    assert "credentials" in out and out["credentials"] is None, out
    assert out["sampling"]["temperature"] == 0.2 and out["sampling"]["seed"] == 3
    assert out["thinking_token_budget_source"] == "default"
    assert out["timeouts"] == {"connect": 5.0, "read": 900.0}
    assert out["kv"] == {
        "slots": 7,
        "overflow": "evict",
        "ttl": 1800.0,
        "live": 0,
        "conversations": [],
        "evicted": 0,
        "rejected": 0,
    }, out["kv"]
    assert out["server_dialect"] == "vllm"


# ─── the key (an HTTPS door in front of the GPU) ─────────────────────────────


def _recording(seen: list, status: int = 200):
    def handler(req: httpx.Request) -> httpx.Response:
        seen.append(req.headers.get("authorization"))
        if status != 200:
            return httpx.Response(status, text="unauthorized")
        return httpx.Response(
            200,
            text=_sse(
                {"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]}
            ),
            headers={"content-type": "text/event-stream"},
        )

    return httpx.MockTransport(handler)


async def test_the_key_rides_as_a_bearer_header_only_when_set():
    seen: list = []
    asked: list = []
    keys = iter(["k-123", "k-rotated"])

    async def key(endpoint):
        asked.append(endpoint)
        return next(keys)

    assert await _drain(_provider(_recording(seen))) == "ok"
    p = _provider(_recording(seen), key=key)
    assert await _drain(p) == "ok"
    assert await _drain(p) == "ok"
    # no credentials: NO header at all (a keyless local vLLM); with them: Bearer,
    # read before EACH request — a rotated key is used by the next one
    assert seen == [None, "Bearer k-123", "Bearer k-rotated"], seen
    assert asked == [ENDPOINT, ENDPOINT]


async def test_a_refused_key_says_to_name_credentials():
    with pytest.raises(RuntimeError) as e:
        await _drain(_provider(_recording([], status=401)))
    assert "HTTP 401" in str(e.value) and "`credentials`" in str(e.value), e.value


def test_the_schema_takes_optional_credentials():
    from vllm_backend.tools import RECORD_SCHEMA

    assert "key_var" not in RECORD_SCHEMA
    f = next(f for f in RECORD_SCHEMA["fields"] if f["name"] == "credentials")
    assert f["type"] == "credentials" and f["required"] is False


# ─── the wire, pinned (reduction candidate A: one OpenAI SSE loop) ──────────
#
# A non-JSON `data:` line RAISES (it is not skipped), and a stream may end
# inside the thought.


async def test_a_non_json_data_line_raises_not_skipped():
    sse = "data: not-json\n\n" + _sse(_delta(content="x"), _delta(finish="stop"))
    p = _provider(_transport(sse))
    with pytest.raises(json.JSONDecodeError):
        await _drain(p)


async def test_blank_data_lines_are_skipped_and_trailing_data_after_done_ignored():
    sse = (
        "data: \n\n"
        + _sse(_delta(content="first"), _delta(finish="stop"))
        + f"data: {json.dumps(_delta(content='after'))}\n\n"
    )
    p = _provider(_transport(sse))
    assert await _drain(p) == "first"


async def test_a_turn_that_finishes_inside_the_thought_closes_the_wrapper():
    sse = _sse(_delta(reasoning_content="hm"), _delta(finish="stop"))
    p = _provider(_transport(sse))
    assert await _drain(p) == f"{THINK_OPEN}hm{THINK_CLOSE}"


@pytest.mark.parametrize(
    "exc, text",
    [
        (
            httpx.ReadTimeout("slow"),
            "vllm_backend: stream_stalled — http://vllm.test/v1 sent no bytes for "
            "900s; this turn is NOT complete",
        ),
        (
            httpx.ReadError("gone"),
            "vllm_backend: http://vllm.test/v1 dropped the stream mid-answer — "
            "vLLM died or restarted (cause: gone); this turn is NOT complete",
        ),
        (
            httpx.ConnectTimeout("syn"),
            "vllm_backend: no answer from http://vllm.test/v1 within "
            "connect_timeout=5s — the instance is STOPPED (its SYNs are dropped), "
            "or the tunnel/port is wrong",
        ),
    ],
)
async def test_a_server_that_never_answers_stalls_on_the_read_clock(exc, text):
    def handler(request):
        raise exc

    p = _provider(httpx.MockTransport(handler))
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert str(e.value) == text


async def test_no_finish_reason_and_main_turn_cut_texts_are_exact():
    p = _provider(_transport(_sse(_delta(content="partial"), done=False)))
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert str(e.value) == (
        "vllm_backend: the stream from http://vllm.test/v1 ended without a "
        "finish_reason — vLLM died mid-answer; this turn is NOT complete"
    )
    p2 = _provider(_transport(_sse(_delta(content="half"), _delta(finish="length"))))
    with pytest.raises(RuntimeError) as e2:
        await _drain(p2)
    assert str(e2.value) == (
        "vllm_backend: the turn was cut at max_tokens=8000 "
        "(finish_reason=length) — it did not finish; raise max_tokens on "
        "the record, or lower thinking_token_budget / reasoning_effort"
    )


async def test_no_budget_in_sampling_sends_none():
    sink: list = []
    sampling = {k: v for k, v in vp.SAMPLING_DEFAULTS.items()}
    sampling.pop("thinking_token_budget")
    p = _provider(_transport(_sse(_delta(finish="stop")), sink), sampling=sampling)
    await _drain(p)
    assert "thinking_token_budget" not in sink[0]
    assert vp.last_budgets["v1"] is None


async def test_an_evicted_conversation_is_logged_by_its_key(caplog):
    ok = _transport(_sse(_delta(finish="stop")))
    p = _provider(ok, kv_slots=1, kv_overflow="evict")
    first = [{"role": "user", "content": "first"}]
    await _drain(p, messages=first)
    with caplog.at_level("INFO", logger="vllm_backend.provider"):
        await _drain(p, messages=[{"role": "user", "content": "second"}])
    assert (
        f"vllm_backend: KV slot rotated — conversation {conversation_key(first)} out"
        in caplog.text
    )


# ── the key never leaves in an error text ────────────────────────────────────


# ─── the read clock (#177) ───────────────────────────────────────────────────


def _timeout_probe(sink: list):
    def handler(request: httpx.Request) -> httpx.Response:
        sink.append(request.extensions["timeout"])
        return httpx.Response(
            200, text=_sse(_delta(content="ok"), _delta(finish="stop"))
        )

    return httpx.MockTransport(handler)


async def test_stream_idle_seconds_defaults_to_read_timeout():
    seen: list = []
    await _drain(_provider(_timeout_probe(seen), read_timeout=30))
    assert seen[0]["read"] == 30.0
    await _drain(_provider(_timeout_probe(seen)))
    assert seen[1]["read"] == 900.0


async def test_a_stream_that_goes_quiet_stalls_on_the_read_clock():
    seen: list = []
    p = _provider(_timeout_probe(seen), read_timeout=30, stream_idle=0.2)
    await _drain(p)
    assert seen[0]["read"] == 0.2
    sse = _sse(_delta(content="part"), done=False)
    p = _provider(
        _breaking_transport(sse, httpx.ReadTimeout("slow")),
        read_timeout=30,
        stream_idle=0.2,
    )
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert "sent no bytes for 0.2s" in str(e.value) and e.value.transport is True
# twin: stream_idle_seconds_is_validated_and_read_clock_defaults_to_read_timeout


def test_stream_idle_seconds_is_validated_at_the_write():
    from vllm_backend.tools import RECORD_SCHEMA, validate_meta

    for bad in (0, -1, "x"):
        msg = validate_meta({"stream_idle_seconds": bad}, {})
        assert msg == "stream_idle_seconds must be a positive number of seconds"
    assert "stream_idle_seconds" in [f["name"] for f in RECORD_SCHEMA["fields"]]


async def test_openai_usage_reports_prompt_tokens_and_a_cut_as_max_tokens():
    # a chunk with no finish and no usage leaves the report empty
    p = _provider(_transport(_sse(_delta(content="x"), done=False)))
    with pytest.raises(Exception, match="without a finish_reason"):
        await _drain(p, max_tokens=50)
    assert (p.last_stop_reason, p.last_prompt_tokens) == (None, None)
    # a `length` finish plus usage: the prompt size and a cut as max_tokens
    cut = {"choices": [{"finish_reason": "length"}], "usage": {"prompt_tokens": 1234}}
    p = _provider(_transport(_sse(_delta(content="x"), cut)))
    await _drain(p, max_tokens=50)
    assert p.last_prompt_tokens == 1234
    assert p.last_stop_reason == "max_tokens"
    # any other finish is reported as it came
    p = _provider(_transport(_sse(_delta(content="x"), _delta(finish="stop"))))
    await _drain(p, max_tokens=50)
    assert p.last_stop_reason == "stop"


# ─── twins of the rust vllm_backend tests (#170) ────────────────────────────


async def test_unknown_type_wire_shape_is_byte_exact(seeded_kernel):
    k = seeded_kernel
    r = await _create(
        k,
        id="v1",
        endpoint=ENDPOINT,
        model="m",
        max_tokens=8000,
        context_window=262144,
        kv_slots=2,
        kv_overflow="evict",
        memory_store_id="memory",
    )
    assert r.get("id") == "v1", r
    got = (await k.send([{"target": "v1", "payload": {"type": "frob"}}]))[0]["result"]
    assert list(got) == ["error", "reason", "verb"]
    assert json.dumps(got, separators=(",", ":")) == (
        '{"error":"vllm_backend: unknown type \'frob\'","reason":"unhandled",'
        '"verb":"frob"}'
    )


async def test_the_last_budget_is_per_agent_and_freed_on_delete(
    seeded_kernel, store_agent, monkeypatch
):
    import asyncio

    from _testkit import ensure_memory

    k = seeded_kernel
    store = ensure_memory(k)
    transport = _transport(
        _sse(_delta("stop", content="ok"))  # a call-less answer
    )
    real_init = VllmProvider.__init__

    def init(self, *a, **kw):
        kw.setdefault("transport", transport)
        real_init(self, *a, **kw)

    monkeypatch.setattr(VllmProvider, "__init__", init)
    meta = dict(
        endpoint=ENDPOINT,
        model="m",
        max_tokens=8000,
        context_window=262144,
        kv_slots=4,
        kv_overflow="evict",
        memory_store_id=store,
        can_call={"cli": "*", store: "*"},
    )

    def call(who, payload):
        return k.send([{"target": who, "payload": payload}])

    async def last(who):
        return (await call(who, {"type": "reflect"}))[0]["result"][
            "last_thinking_budget"
        ]

    async def make(aid):
        assert (await _create(k, id=aid, **meta)).get("id") == aid
        # two-sided deny-all: open each callee's reciprocal `callable_by`
        for target in meta["can_call"]:
            cb = dict(k.ctx.agents[target]._meta.get("callable_by") or {})
            cb[aid] = "*"
            await call(
                "kernel_state",
                {"type": "update_agent", "id": target, "callable_by": cb},
            )

    await make("v1")
    await make("v1_sib")
    out = (await call("v1", {"type": "send", "text": "hi"}))[0]["result"]
    assert out.get("final") == "ok", out
    # the call-less answer is rerolled in the background; let it settle so a late
    # reroll cannot record a budget for the re-created id
    quiet = 0
    for _ in range(500):
        st = (await call("v1", {"type": "status"}))[0]["result"]
        idle = st["generating"] is False and st["current"] is None
        quiet = quiet + 1 if idle else 0
        if quiet >= 5:
            break
        await asyncio.sleep(0.02)
    assert quiet >= 5, "the chain never settled"
    mine = await last("v1")
    assert isinstance(mine, int) and not isinstance(mine, bool), mine
    assert await last("v1_sib") is None  # a sibling shares nothing
    gone = (await call(k.id, {"type": "delete_agent", "id": "v1"}))[0]["result"]
    assert "error" not in gone, gone
    await make("v1")
    assert await last("v1") is None  # freed on delete
