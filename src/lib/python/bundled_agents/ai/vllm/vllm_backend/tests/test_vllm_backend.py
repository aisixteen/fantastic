"""vllm_backend — the wire it speaks, the turns it must not lose, and the
KV-cache session table it keeps per endpoint.

Every behaviour here traces to a measured failure on a live vLLM (the production
deployment, Qwen3.8-27B-FP8, vLLM 0.27.1): a dropped thought channel, a turn
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


def _provider(transport, **kw) -> VllmProvider:
    args = dict(
        endpoint=ENDPOINT,
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
    yield
    vp._tables.clear()


# ─── the stream ──────────────────────────────────────────────────────────────


async def test_the_thought_channel_is_wrapped_once_not_dropped():
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
    await p.aclose()


async def test_a_cut_main_turn_fails_naming_the_cut():
    # the record's bound hit on a MAIN turn: it did not finish — never an answer
    p = _provider(_transport(_sse(_delta(content="half"), _delta(finish="length"))))
    with pytest.raises(RuntimeError, match=r"cut at max_tokens=8000"):
        await _drain(p)
    assert p.last_stop_reason == "max_tokens"
    await p.aclose()


async def test_a_cut_bounded_side_call_is_its_answer():
    # a fold summary passes its own bound: stopping there is what it asked for
    p = _provider(_transport(_sse(_delta(content="half"), _delta(finish="length"))))
    assert await _drain(p, max_tokens=50) == "half"
    assert p.last_stop_reason == "max_tokens"
    await p.aclose()


async def test_a_stream_that_ends_without_finish_reason_is_an_error():
    # vLLM died mid-answer: the text so far must never read as a completion
    p = _provider(_transport(_sse(_delta(content="partial"), done=False)))
    with pytest.raises(RuntimeError, match="without a finish_reason"):
        await _drain(p)
    await p.aclose()


async def test_an_http_error_names_the_endpoint_and_the_body():
    p = _provider(_transport('{"error":"bad effort"}', status=400))
    with pytest.raises(RuntimeError, match=r"answered HTTP 400: .*bad effort"):
        await _drain(p)
    await p.aclose()


@pytest.mark.parametrize(
    "exc, words",
    [
        (httpx.ConnectError("refused"), "not listening yet"),
        (httpx.ConnectTimeout("syn"), "instance is STOPPED"),
        (httpx.RemoteProtocolError("reset"), "died or restarted"),
    ],
)
async def test_each_connection_failure_says_what_it_means(exc, words):
    def handler(request):
        raise exc

    p = _provider(httpx.MockTransport(handler))
    with pytest.raises(RuntimeError, match=words):
        await _drain(p)
    await p.aclose()


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
    await p.aclose()


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
    await p.aclose()


async def test_the_default_budget_halves_on_a_smaller_call_and_none_below_the_floor():
    sink: list = []
    p = _provider(_transport(_sse(_delta(finish="stop")), sink))
    await _drain(p)  # the turn: max_tokens 8000 → min(16000, 4000)
    await _drain(p, max_tokens=2000)  # a fold summary → min(16000, 1000)
    await _drain(p, max_tokens=40)  # half is 20, under the 256 floor → none
    assert [b.get("thinking_token_budget") for b in sink] == [4000, 1000, None]
    assert p.last_thinking_budget is None
    await p.aclose()


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
    await p.aclose()


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
    for p in (p1, p2, other):
        await p.aclose()


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
    ],
)
def test_a_write_the_backend_cannot_honour_is_refused(meta, record, fragment):
    assert fragment in (validate_meta(meta, record) or "")


def test_a_record_missing_required_fields_is_refused_at_the_write_naming_all():
    # what a model spawning a sub-AI wrote (E37): the fields it could see
    meta = {"endpoint": ENDPOINT, "model": "qwen3.8-27b", "max_tokens": 65536}
    err = validate_meta(meta, {})
    assert err and "missing: kv_slots, kv_overflow" in err, err
    # an update to a complete record is judged on the MERGED state
    full = {**meta, "kv_slots": 7, "kv_overflow": "evict"}
    assert validate_meta({"seed": 3}, full) is None


def test_a_consistent_write_passes():
    meta = {
        "endpoint": ENDPOINT,
        "model": "qwen3.8-27b",
        "max_tokens": 8000,
        "kv_slots": 6,
        "kv_overflow": "evict",
        "thinking_token_budget": 4000,
        "reasoning_effort": "xhigh",
    }
    assert validate_meta(meta, {}) is None


# ─── through the kernel ──────────────────────────────────────────────────────


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
        kv_slots=7,
        kv_overflow="evict",
        seed=3,
    )
    assert r.get("id") == "v1", r
    out = (await k.send([{"target": "v1", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
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
