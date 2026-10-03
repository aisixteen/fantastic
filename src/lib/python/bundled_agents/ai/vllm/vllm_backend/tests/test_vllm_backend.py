"""vllm_backend — the wire it speaks, the turns it must not lose, and the
KV-cache session table it keeps per endpoint.

Every behaviour here traces to a measured failure on a live vLLM (the dataroom
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
    yield
    vp._tables.clear()
    vp.last_budgets.clear()


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


async def test_a_stream_that_ends_without_finish_reason_is_an_error():
    # vLLM died mid-answer: the text so far must never read as a completion
    p = _provider(_transport(_sse(_delta(content="partial"), done=False)))
    with pytest.raises(RuntimeError, match="without a finish_reason"):
        await _drain(p)


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
async def test_each_connection_failure_says_what_it_means(exc, words):
    def handler(request):
        raise exc

    p = _provider(httpx.MockTransport(handler))
    with pytest.raises(RuntimeError, match=words):
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
    assert err and "missing: kv_slots, kv_overflow, memory_store_id" in err, err
    # each missing field is named WITH its hint, from the schema
    assert "kv_overflow = all slots live: evict" in err, err
    # an update to a complete record is judged on the MERGED state
    full = {**meta, "kv_slots": 7, "kv_overflow": "evict", "memory_store_id": "memory"}
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
        "memory_store_id": "memory",
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
        memory_store_id="memory",
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
            "vllm_backend: http://vllm.test/v1 sent nothing for read_timeout=900s",
        ),
        (
            httpx.ReadError("gone"),
            "vllm_backend: http://vllm.test/v1 dropped the stream mid-answer — "
            "vLLM died or restarted; this turn is NOT complete",
        ),
        (
            httpx.ConnectTimeout("syn"),
            "vllm_backend: no answer from http://vllm.test/v1 within "
            "connect_timeout=5s — the instance is STOPPED (its SYNs are dropped), "
            "or the tunnel/port is wrong",
        ),
    ],
)
async def test_each_transport_failure_text_is_exact(exc, text):
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


# ── the key never leaves in an error text (audit S1) ─────────────────────────


async def test_a_header_the_client_refuses_leaves_with_the_key_scrubbed():
    """h11 refuses a header holding a newline and QUOTES it in its error. The
    provider raises that text with the key replaced by `<secret>`, its chain cut
    (a traceback must not print the original)."""
    import asyncio

    async def handle(r, w):
        try:
            await asyncio.wait_for(r.read(4096), 1)
        except Exception:
            pass
        w.close()

    srv = await asyncio.start_server(handle, "127.0.0.1", 0)
    port = srv.sockets[0].getsockname()[1]

    async def key(endpoint: str) -> str:
        return "sk-test-fixture\n"

    p = _provider(None, endpoint=f"http://127.0.0.1:{port}/v1", key=key)
    try:
        with pytest.raises(RuntimeError) as e:
            await _drain(p)
    finally:
        srv.close()
    assert "sk-test-fixture" not in str(e.value), e.value
    assert str(e.value).startswith("vllm_backend: LocalProtocolError: "), e.value
    assert "<secret>" in str(e.value), e.value
    assert e.value.__suppress_context__, "the original (quoting the key) is cut"


async def test_an_error_body_echoing_the_key_is_scrubbed():
    async def key(endpoint: str) -> str:
        return "sk-test-fixture"

    p = _provider(
        _transport("invalid key: Bearer sk-test-fixture", status=401), key=key
    )
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    assert str(e.value) == (
        f"vllm_backend: {ENDPOINT} answered HTTP 401 — the door wants a key: name it "
        "in `credentials` ({store, ref}): invalid key: Bearer <secret>"
    )
