"""anthropic_backend — the Messages wire it speaks, what it reads from the API when
the record leaves it out, and the optional key."""

from __future__ import annotations

import json
from pathlib import Path

import httpx
import pytest
from _testkit import boot_root, persist
from ai_core.core import THINK_CLOSE, THINK_OPEN
from anthropic_backend import server
from anthropic_backend.provider import AnthropicProvider, to_messages
from anthropic_backend.tools import RECORD_SCHEMA, validate_meta

ENDPOINT = "https://api.test/v1/messages"
MODEL = "claude-test-5"


def _sse(*events: dict) -> str:
    return "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events)


def _text(t: str) -> dict:
    return {
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": t},
    }


def _think(t: str) -> dict:
    return {
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "thinking_delta", "thinking": t},
    }


START = {
    "type": "message_start",
    "message": {"usage": {"input_tokens": 100, "cache_read_input_tokens": 20}},
}


def _stop(reason: str = "end_turn") -> list[dict]:
    return [
        {
            "type": "message_delta",
            "delta": {"stop_reason": reason},
            "usage": {"output_tokens": 5},
        },
        {"type": "message_stop"},
    ]


def _transport(body: str, sink: list | None = None, status: int = 200):
    def handler(req: httpx.Request) -> httpx.Response:
        if sink is not None:
            sink.append(req)
        return httpx.Response(status, text=body)

    return httpx.MockTransport(handler)


async def _nokey(endpoint):
    return None


async def _key(endpoint):
    return "sk-test"


def _provider(transport, **kw) -> AnthropicProvider:
    args = dict(
        endpoint=ENDPOINT, model=MODEL, key=_nokey, max_tokens=8000, transport=transport
    )
    args.update(kw)
    return AnthropicProvider(**args)


async def _drain(p, messages=None, **kw) -> str:
    msgs = messages or [{"role": "user", "content": "hi"}]
    return "".join([c async for c in p.chat(msgs, **kw)])


# ─── the request ─────────────────────────────────────────────────────────────


def test_system_is_one_string_and_turns_of_one_role_merge():
    system, turns = to_messages(
        [
            {"role": "system", "content": "a"},
            {"role": "user", "content": "u1"},
            {"role": "system", "content": "b"},
            {"role": "user", "content": "u2"},
            {"role": "assistant", "content": "x"},
        ]
    )
    assert system == "a\n\nb"
    assert turns == [
        {"role": "user", "content": "u1\n\nu2"},
        {"role": "assistant", "content": "x"},
    ]


def test_the_body_sends_no_tools_and_only_what_the_record_set():
    p = _provider(None)
    b = p.body(
        [{"role": "system", "content": "s"}, {"role": "user", "content": "hi"}], None
    )
    assert b == {
        "model": MODEL,
        "max_tokens": 8000,
        "messages": [{"role": "user", "content": "hi"}],
        "stream": True,
        "system": "s",
    }
    p = _provider(None, temperature=0.2, thinking_budget=2048)
    b = p.body([{"role": "user", "content": "hi"}], None)
    assert b["temperature"] == 0.2
    assert b["thinking"] == {"type": "enabled", "budget_tokens": 2048}
    # a side call's own bound, too small for the thinking budget: no thinking
    b = p.body([{"role": "user", "content": "hi"}], 1500)
    assert b["max_tokens"] == 1500 and "thinking" not in b


async def test_the_key_rides_as_x_api_key_only_when_named():
    seen: list = []
    body = _sse(START, _text("ok"), *_stop())
    await _drain(_provider(_transport(body, seen), key=_key))
    await _drain(_provider(_transport(body, seen)))
    assert seen[0].headers["x-api-key"] == "sk-test"
    assert seen[0].headers["anthropic-version"] == "2023-06-01"
    assert "x-api-key" not in seen[1].headers
    assert seen[1].headers["anthropic-version"] == "2023-06-01"
    assert "tools" not in json.loads(seen[0].content)


# ─── the stream ──────────────────────────────────────────────────────────────


async def test_the_thought_is_wrapped_once_and_the_usage_reported():
    p = _provider(
        _transport(
            _sse(START, _think("let me "), _think("see"), _text("done"), *_stop())
        )
    )
    assert await _drain(p) == f"{THINK_OPEN}let me see{THINK_CLOSE}done"
    assert p.last_prompt_tokens == 120 and p.last_stop_reason == "end_turn"


async def test_a_stream_without_message_stop_did_not_finish():
    p = _provider(_transport(_sse(START, _text("half"))))
    with pytest.raises(RuntimeError, match="ended without message_stop"):
        await _drain(p)


async def test_an_error_event_carries_the_apis_own_message():
    err = {
        "type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"},
    }
    p = _provider(_transport(_sse(START, err)))
    with pytest.raises(
        RuntimeError, match="stream error: overloaded_error: Overloaded"
    ):
        await _drain(p)


async def test_a_refused_key_says_to_name_credentials():
    p = _provider(_transport('{"type":"error"}', status=401))
    with pytest.raises(RuntimeError, match="name it in `credentials`"):
        await _drain(p)


async def test_a_cut_main_turn_fails_and_a_cut_side_call_is_its_answer():
    body = _sse(START, _text("half"), *_stop("max_tokens"))
    with pytest.raises(RuntimeError, match="cut at max_tokens=8000"):
        await _drain(_provider(_transport(body)))
    p = _provider(_transport(body))
    assert await _drain(p, max_tokens=500) == "half"
    assert p.last_stop_reason == "max_tokens"


# ─── link failures are transport (#198) ──────────────────────────────────────


class _BrokenStream(httpx.AsyncByteStream):
    """A body that sends one event, then the link breaks mid-answer."""

    def __init__(self, first: bytes):
        self._first = first

    async def __aiter__(self):
        yield self._first
        raise httpx.ReadError("the peer went away")


def _raising(exc: Exception):
    def handler(req: httpx.Request) -> httpx.Response:
        raise exc

    return httpx.MockTransport(handler)


async def _turn_error(p) -> Exception:
    with pytest.raises(RuntimeError) as e:
        await _drain(p)
    return e.value


async def test_a_connect_failure_is_transport():
    from ai_core.provider import ProviderError

    err = await _turn_error(_provider(_raising(httpx.ConnectError("refused"))))
    assert isinstance(err, ProviderError) and err.transport is True
    assert str(err) == f"anthropic_backend: could not connect to {ENDPOINT}"
    err = await _turn_error(_provider(_raising(httpx.ConnectTimeout("slow"))))
    assert isinstance(err, ProviderError) and err.transport is True
    assert str(err) == f"anthropic_backend: no answer from {ENDPOINT} within 15s"


async def test_a_stream_dropped_mid_answer_is_transport():
    from ai_core.provider import ProviderError

    first = _sse(START, _text("par")).encode()
    t = httpx.MockTransport(
        lambda req: httpx.Response(200, stream=_BrokenStream(first))
    )
    err = await _turn_error(_provider(t))
    assert isinstance(err, ProviderError) and err.transport is True
    assert "dropped the stream mid-answer" in str(err)


@pytest.mark.parametrize("code", [502, 503, 504, 529])
async def test_a_gateway_or_overloaded_status_is_transport(code):
    from ai_core.provider import ProviderError

    err = await _turn_error(_provider(_transport("busy", status=code)))
    assert isinstance(err, ProviderError) and err.transport is True
    assert f"answered HTTP {code}: busy" in str(err)


@pytest.mark.parametrize("code", [400, 401, 500])
async def test_a_status_the_api_meant_is_not_transport(code):
    from ai_core.provider import ProviderError

    err = await _turn_error(_provider(_transport("bad request", status=code)))
    assert isinstance(err, ProviderError) and err.transport is False


async def test_a_stream_without_message_stop_is_not_transport():
    from ai_core.provider import ProviderError

    err = await _turn_error(_provider(_transport(_sse(START, _text("x")))))
    assert not isinstance(err, ProviderError) or err.transport is False


# ─── the write ───────────────────────────────────────────────────────────────


def _rec(**kw) -> dict:
    return {"endpoint": ENDPOINT, "model": MODEL, "memory_store_id": "memory", **kw}


def test_endpoint_model_and_a_store_are_all_a_record_needs():
    required = [f["name"] for f in RECORD_SCHEMA["fields"] if f["required"]]
    assert required == ["endpoint", "model", "memory_store_id"]
    assert validate_meta(_rec(), {}) is None
    cred = next(f for f in RECORD_SCHEMA["fields"] if f["name"] == "credentials")
    assert cred["required"] is False


@pytest.mark.parametrize(
    "meta, fragment",
    [
        ({"max_tokens": 0}, "max_tokens must be a positive int"),
        ({"thinking_budget": 512}, "below the API's 1024"),
        ({"thinking_budget": 4000, "max_tokens": 4000}, "not below max_tokens=4000"),
        ({"temperature": "hot"}, "temperature must be a number"),
        (
            {"endpoint": "https://api.test/v1/chat"},
            "max_tokens is left to the API, but the endpoint",
        ),
    ],
)
def test_a_write_the_backend_cannot_honour_is_refused(meta, fragment):
    assert fragment in (validate_meta(meta, _rec()) or ""), validate_meta(meta, _rec())


# ─── the read ────────────────────────────────────────────────────────────────


def _api(seen: list, entry: dict | None = None, status: int = 200):
    entry = entry or {"id": MODEL, "max_input_tokens": 200000, "max_tokens": 64000}

    def handler(req: httpx.Request) -> httpx.Response:
        seen.append((str(req.url), req.headers.get("x-api-key")))
        return httpx.Response(status, text=json.dumps(entry))

    return httpx.MockTransport(handler)


class _Kernel:
    def __init__(self, rec: dict):
        self.rec = {"id": "a1", **rec}
        self.updates: list[dict] = []

    def get(self, id):
        return self.rec if id == "a1" else None

    def update(self, id, **meta):
        self.updates.append(meta)
        self.rec.update(meta)


async def _resolve(k, transport, key=_nokey):
    return await server.resolve(
        "a1", k, key=key, validate=validate_meta, transport=transport
    )


async def test_what_the_record_leaves_out_is_read_from_the_model_entry():
    seen: list = []
    k = _Kernel(_rec())
    assert await _resolve(k, _api(seen), key=_key) is None
    assert k.updates == [{"max_tokens": 64000, "context_window": 200000}]
    assert seen == [(f"https://api.test/v1/models/{MODEL}", "sk-test")]
    seen.clear()
    assert await _resolve(k, _api(seen)) is None
    assert seen == []


async def test_a_field_the_record_sets_is_never_read():
    seen: list = []
    k = _Kernel(_rec(max_tokens=4000))
    assert await _resolve(k, _api(seen)) is None
    assert k.updates == [{"context_window": 200000}]


async def test_an_api_that_does_not_answer_refuses_naming_the_field():
    k = _Kernel(_rec())
    r = await _resolve(k, _api([], status=404))
    assert r == {
        "error": f"max_tokens: GET https://api.test/v1/models/{MODEL} answered HTTP 404; "
        "set max_tokens on the record",
        "reason": "bad_arg",
        "arg": "max_tokens",
    }, r
    r = await _resolve(_Kernel(_rec()), _api([], status=401))
    assert "name it in `credentials`" in r["error"], r
    r = await _resolve(_Kernel(_rec(max_tokens=10)), _api([], entry={"id": MODEL}))
    assert r["arg"] == "context_window" and "names no max_input_tokens" in r["error"], r


async def test_the_apis_values_pass_the_write_check():
    k = _Kernel(_rec(thinking_budget=70000))
    r = await _resolve(k, _api([]))
    assert r["reason"] == "bad_arg", r
    assert "the API's values do not fit the record: thinking_budget=70000" in r["error"]
    assert k.updates == []


# ─── through the kernel ──────────────────────────────────────────────────────


@pytest.fixture
def mock_http(monkeypatch):
    real = httpx.AsyncClient

    def use(transport):
        monkeypatch.setattr(
            httpx, "AsyncClient", lambda **kw: real(**{**kw, "transport": transport})
        )

    return use


async def _call(kernel, target, payload):
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def _create(kernel, **meta):
    return await _call(
        kernel,
        kernel.id,
        {
            "type": "create_agent",
            "parent_id": kernel.id,
            "handler_module": "anthropic_backend.tools",
            "id": "a1",
            **_rec(**meta),
        },
    )


async def test_a_send_reads_the_api_first(seeded_kernel, mock_http, fake_providers):
    seen: list = []
    mock_http(_api(seen))
    r = await _create(seeded_kernel)
    assert r.get("id") == "a1", r
    await _call(seeded_kernel, "a1", {"type": "send", "text": "hi", "client_id": "t"})
    rec = seeded_kernel.get("a1")
    assert (rec["max_tokens"], rec["context_window"]) == (64000, 200000), rec


async def test_a_verb_that_runs_no_turn_reads_nothing(seeded_kernel, mock_http):
    seen: list = []
    mock_http(_api(seen))
    await _create(seeded_kernel)
    for verb in ("reflect", "status", "context_status", "rows"):
        await _call(seeded_kernel, "a1", {"type": verb})
    assert seen == []


# ─── prompt caching ──────────────────────────────────────────────────────────

CACHED = {"type": "ephemeral"}
PLAIN = [
    {"role": "system", "content": "s"},
    {"role": "user", "content": "hi"},
]


def _markers(body: dict) -> int:
    """The number of `cache_control` markers a body sends."""
    return json.dumps(body).count('"cache_control"')


def _flagged(rows) -> list[dict]:
    """The conversation's messages as ai_core flags them, a system row first.
    `rows` = [(role, text, flag-or-None)]."""
    out = [{"role": "system", "content": "S"}]
    for role, text, flag in rows:
        m = {"role": role, "content": text}
        if flag:
            m[flag] = True
        out.append(m)
    return out


def test_cleanup_and_none_send_no_marker_even_when_a_message_is_flagged():
    assert _provider(None).body(PLAIN, None) == {
        "model": MODEL,
        "max_tokens": 8000,
        "messages": [{"role": "user", "content": "hi"}],
        "stream": True,
        "system": "s",
    }
    flags = _flagged([("user", "a", "cache_primer"), ("assistant", "b", "cache_fold")])
    for mode in ("cleanup", "none"):
        b = _provider(None, caching=mode).body(flags, None)
        assert _markers(b) == 0, b
        assert b["system"] == "S"
        assert b["messages"] == [
            {"role": "user", "content": "a"},
            {"role": "assistant", "content": "b"},
        ]


def test_provider_cache_splits_the_merged_turns_at_the_flagged_rows():
    # [primer rows][fold][fold][task rows]: the primer's row is the assistant turn
    # "p2"; the fold rows and the task rows are one merged user turn, and the marker
    # lands at the end of the last fold's text, not at the end of that merged turn.
    msgs = _flagged(
        [
            ("user", "p1", None),
            ("assistant", "p2", "cache_primer"),
            ("user", "f1", None),
            ("user", "f2", "cache_fold"),
            ("user", "t1", None),
            ("assistant", "t2", None),
            ("user", "live", None),
        ]
    )
    b = _provider(None, caching="provider_cache").body(msgs, None)
    marked = {"type": "ephemeral"}
    assert b["system"][0]["cache_control"] == marked, b
    assert b["messages"] == [
        {"role": "user", "content": "p1"},
        {
            "role": "assistant",
            "content": [
                {"type": "text", "text": "p2", "cache_control": marked},
            ],
        },
        {
            "role": "user",
            "content": [
                {"type": "text", "text": "f1\n\nf2", "cache_control": marked},
                {"type": "text", "text": "\n\nt1"},
            ],
        },
        {"role": "assistant", "content": "t2"},
        {"role": "user", "content": "live"},
    ], b
    assert b["cache_control"] == marked, b
    assert _markers(b) == 4, b


_SPLIT_CASES = [
    (["one", "", "three"], ["cache_primer", None, None]),
    (["one", "", "three"], [None, "cache_fold", None]),
    (["one", "", "three"], [None, "cache_primer", "cache_fold"]),
    (["one", "", "three"], ["cache_fold", "cache_fold", None]),
    (["one", "", "three"], [None, None, "cache_primer"]),
    (["one", ""], ["cache_primer", None]),
    (["one", "", ""], ["cache_fold", None, None]),
    (["", "one", ""], [None, "cache_fold", None]),
    (["one", ""], ["cache_primer", "cache_fold"]),
    # a flagged row that is only whitespace, first in its turn, opens no block of its
    # own (the API refuses a blank block): it joins the next text
    ([" ", "two"], ["cache_primer", None]),
    ([" ", "two", "three"], ["cache_fold", "cache_primer", None]),
    # ...and after text, it closes the block it is part of, bytes unchanged
    (["one", " "], [None, "cache_fold"]),
]


@pytest.mark.parametrize("texts, shape", _SPLIT_CASES)
def test_the_split_turns_concatenate_to_exactly_the_merged_string(texts, shape):
    # The block texts of a merged turn, joined, are the string the turn was before
    # any split: no byte added or lost, wherever a flag falls. And no block is blank.
    msgs = _flagged([("user", t, f) for t, f in zip(texts, shape)])
    b = _provider(None, caching="provider_cache").body(msgs, None)
    merged = to_messages(msgs)[1][0]["content"]
    content = b["messages"][0]["content"]
    if isinstance(content, list):
        for block in content:
            assert block["text"].strip(), f"a blank block is refused by the API: {b}"
        joined = "".join(x["text"] for x in content)
    else:
        joined = content
    assert joined == merged, b
    assert _markers(b) <= 4, b


def test_a_flagged_row_followed_by_empty_rows_keeps_its_marker_and_no_blank_block():
    msgs = _flagged([("user", "one", "cache_primer"), ("user", "", None)])
    b = _provider(None, caching="provider_cache").body(msgs, None)
    assert b["messages"] == [
        {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "one\n\n",
                    "cache_control": {"type": "ephemeral"},
                },
            ],
        }
    ], b
    assert _markers(b) == 3, b


def test_a_flagged_row_with_no_text_carries_no_marker():
    msgs = _flagged([("user", "a", None), ("user", "", "cache_primer")])
    b = _provider(None, caching="provider_cache").body(msgs, None)
    assert b["messages"] == [
        {"role": "user", "content": [{"type": "text", "text": "a\n\n"}]}
    ], b
    assert _markers(b) == 2, b


def test_a_primer_and_a_fold_on_one_message_send_one_marker():
    msgs = _flagged([("user", "only", None), ("assistant", "both", "cache_primer")])
    msgs[2]["cache_fold"] = True
    b = _provider(None, caching="provider_cache").body(msgs, None)
    assert b["messages"][1]["content"] == [
        {"type": "text", "text": "both", "cache_control": {"type": "ephemeral"}},
    ], b
    assert _markers(b) == 3, b


def test_provider_cache_with_no_primer_and_no_fold_marks_only_the_system_and_the_tail():
    b = _provider(None, caching="provider_cache").body(
        _flagged([("user", "a", None), ("assistant", "b", None)]), None
    )
    assert b["messages"] == [
        {"role": "user", "content": "a"},
        {"role": "assistant", "content": "b"},
    ]
    assert _markers(b) == 2, b


def test_provider_cache_marks_the_system_block_and_the_tail():
    p = _provider(None, caching="provider_cache", temperature=0.2, thinking_budget=2048)
    assert p.body(PLAIN, None) == {
        "model": MODEL,
        "max_tokens": 8000,
        "messages": [{"role": "user", "content": "hi"}],
        "stream": True,
        "system": [{"type": "text", "text": "s", "cache_control": CACHED}],
        "cache_control": CACHED,
        "temperature": 0.2,
        "thinking": {"type": "enabled", "budget_tokens": 2048},
    }
    # a side call's own bound is marked the same way
    b = p.body(PLAIN, 1500)
    assert b["max_tokens"] == 1500 and b["cache_control"] == CACHED


def test_provider_cache_sends_no_system_block_when_the_system_is_empty():
    p = _provider(None, caching="provider_cache")
    b = p.body([{"role": "user", "content": "hi"}], None)
    assert "system" not in b
    assert b["cache_control"] == CACHED


async def test_provider_cache_marks_every_request_a_provider_sends():
    seen: list = []
    p = _provider(
        _transport(_sse(START, _text("ok"), *_stop()), seen), caching="provider_cache"
    )
    await _drain(p, PLAIN)
    assert json.loads(seen[0].content)["cache_control"] == CACHED
    assert json.loads(seen[0].content)["system"][0]["cache_control"] == CACHED


def test_the_flags_reach_the_wire_only_through_body_marks():
    assert to_messages(_flagged([("user", "a", "cache_primer")]))[1] == [
        {"role": "user", "content": "a"}
    ]


# ─── usage ───────────────────────────────────────────────────────────────────


@pytest.fixture
def _fresh_usage():
    from anthropic_backend import tools

    tools._USAGE.clear()
    yield
    tools._USAGE.clear()


def _usage_start(inp: int, cw: int, cr: int) -> dict:
    return {
        "type": "message_start",
        "message": {
            "usage": {
                "input_tokens": inp,
                "cache_creation_input_tokens": cw,
                "cache_read_input_tokens": cr,
            }
        },
    }


def _usage_stop(out: int) -> list[dict]:
    return [
        {
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": out},
        },
        {"type": "message_stop"},
    ]


async def test_the_stream_sets_last_prompt_tokens_and_the_counts():
    from anthropic_backend.provider import Usage

    u = Usage()
    body = _sse(_usage_start(100, 400, 1000), _text("ok"), *_usage_stop(35))
    p = _provider(_transport(body), usage=u)
    await _drain(p)
    assert p.last_prompt_tokens == 1500 and p.last_completion_tokens == 35
    assert (u.requests, u.input_tokens, u.cache_write_tokens) == (1, 100, 400)
    assert (u.cache_read_tokens, u.output_tokens) == (1000, 35)


async def test_two_passes_accumulate_and_a_side_call_counts_too():
    from anthropic_backend.provider import Usage

    u = Usage()
    first = _sse(_usage_start(100, 400, 0), _text("a"), *_usage_stop(10))
    second = _sse(_usage_start(50, 0, 900), _text("b"), *_usage_stop(20))
    await _drain(_provider(_transport(first), usage=u))
    await _drain(_provider(_transport(second), usage=u), max_tokens=500)
    assert (u.requests, u.input_tokens, u.cache_write_tokens) == (2, 150, 400)
    assert (u.cache_read_tokens, u.output_tokens) == (900, 30)


def test_hit_rate_and_saved_tokens_follow_the_formulas():
    from anthropic_backend.provider import Usage, saved_tokens

    u = Usage()
    u.add_request(100, 400, 1000)
    s = u.snapshot()
    assert s["prompt_tokens"] == 1500
    assert s["hit_rate"] == pytest.approx(1000 / 1500)
    assert s["saved_tokens"] == 800  # 0.9*1000 - 0.25*400
    assert "cost_usd" not in s and "saved_usd" not in s
    # a write that is never read is a loss: negative, reported, never clamped
    w = Usage()
    w.add_request(10, 400, 0)
    assert w.snapshot()["saved_tokens"] == -100
    assert w.snapshot()["hit_rate"] == 0.0
    # nothing counted: no prompt, no hit rate
    assert Usage().snapshot()["hit_rate"] == 0
    # halves round to even
    assert saved_tokens(5, 0) == 4 and saved_tokens(0, 2) == 0
    assert saved_tokens(0, 3) == -1 and saved_tokens(1, 0) == 1


def test_prices_on_the_record_add_cost_and_saving_in_usd():
    from anthropic_backend.provider import Usage

    u = Usage()
    u.add_request(100, 400, 1000)
    u.output_tokens = 50
    s = u.snapshot(price_in=3.0, price_out=15.0)
    cost = (100 + 1.25 * 400 + 0.1 * 1000) * 3.0 / 1e6 + 50 * 15.0 / 1e6
    assert s["cost_usd"] == pytest.approx(cost)
    assert s["saved_usd"] == pytest.approx(800 * 3.0 / 1e6)
    # one price without the other: neither field
    s = u.snapshot(price_in=3.0)
    assert "cost_usd" not in s and "saved_usd" not in s


async def test_the_usage_verb_returns_the_counts_and_reset_zeroes_them(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    from anthropic_backend import tools

    mock_http(_api([]))
    await _create(seeded_kernel, price_input_per_mtok=3.0, price_output_per_mtok=15.0)
    tools.usage_of("a1").add_request(100, 400, 1000)
    got = await _call(seeded_kernel, "a1", {"type": "usage"})
    assert got["cache_read_tokens"] == 1000 and got["saved_tokens"] == 800
    assert "cost_usd" in got and "saved_usd" in got
    assert await _call(seeded_kernel, "a1", {"type": "usage", "reset": True}) == got
    zero = await _call(seeded_kernel, "a1", {"type": "usage"})
    assert zero["requests"] == 0 and zero["hit_rate"] == 0 and zero["saved_tokens"] == 0
    bad = await _call(seeded_kernel, "a1", {"type": "usage", "reset": "yes"})
    assert bad["reason"] == "bad_arg" and bad["arg"] == "reset"


async def test_reflect_carries_the_usage_and_the_caching_mode(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    mock_http(_api([]))
    await _create(seeded_kernel, caching="provider_cache")
    r = await _call(seeded_kernel, "a1", {"type": "reflect"})
    assert r["caching"] == "provider_cache"
    assert "prompt_cache" not in r
    assert r["usage"]["requests"] == 0 and r["usage"]["hit_rate"] == 0


async def test_reflect_shows_cache_primer_until_only_when_set(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    mock_http(_api([]))
    await _create(seeded_kernel, caching="provider_cache", cache_primer_until=3)
    r = await _call(seeded_kernel, "a1", {"type": "reflect"})
    assert r["cache_primer_until"] == 3
    await _call(seeded_kernel, seeded_kernel.id, {"type": "delete_agent", "id": "a1"})
    await _create(seeded_kernel, caching="provider_cache")
    r = await _call(seeded_kernel, "a1", {"type": "reflect"})
    assert "cache_primer_until" not in r


async def test_reflect_shows_cleanup_when_caching_is_unset(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    mock_http(_api([]))
    await _create(seeded_kernel)
    r = await _call(seeded_kernel, "a1", {"type": "reflect"})
    assert r["caching"] == "cleanup"
    assert "cache_primer_until" not in r


def _models_api(status: int, body: str, seen: list | None = None):
    def handler(req: httpx.Request) -> httpx.Response:
        if seen is not None:
            seen.append(str(req.url))
        return httpx.Response(status, text=body)

    return httpx.MockTransport(handler)


async def test_models_lists_the_keys_models(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    seen: list = []
    body = json.dumps(
        {
            "data": [
                {
                    "id": "m1",
                    "display_name": "M1",
                    "max_input_tokens": 200000,
                    "max_tokens": 64000,
                },
                {"id": "m2", "display_name": "M2", "max_input_tokens": 100000},
            ]
        }
    )
    mock_http(_models_api(200, body, seen))
    await _create(seeded_kernel)
    got = await _call(seeded_kernel, "a1", {"type": "models"})
    assert [m["id"] for m in got["models"]] == ["m1", "m2"]
    assert got["models"][1]["max_tokens"] is None
    assert "limit=1000" in seen[0]


async def test_models_names_a_missing_key_on_401(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    mock_http(_models_api(401, "{}"))
    await _create(seeded_kernel)
    got = await _call(seeded_kernel, "a1", {"type": "models"})
    assert "401" in got["error"] and "credentials" in got["error"]


async def test_models_refuses_a_body_that_is_not_a_model_list(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    mock_http(_models_api(200, "<html>"))
    await _create(seeded_kernel)
    got = await _call(seeded_kernel, "a1", {"type": "models"})
    assert "not a model list" in got["error"]

    mock_http(_models_api(200, "{}"))
    got = await _call(seeded_kernel, "a1", {"type": "models"})
    assert "not a model list" in got["error"]


async def test_reflect_lists_the_models_verb(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    mock_http(_api([]))
    await _create(seeded_kernel)
    got = await _call(seeded_kernel, "a1", {"type": "models"})
    assert "not a model list" in got["error"]


async def test_deleting_an_agent_drops_its_counts(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    from anthropic_backend import tools

    mock_http(_api([]))
    await _create(seeded_kernel)
    tools.usage_of("a1").add_request(100, 400, 1000)
    r = await _call(
        seeded_kernel, seeded_kernel.id, {"type": "delete_agent", "id": "a1"}
    )
    assert r["deleted"] is True, r
    assert "a1" not in tools._USAGE
    await _create(seeded_kernel)
    zero = await _call(seeded_kernel, "a1", {"type": "usage"})
    assert zero["requests"] == 0 and zero["cache_read_tokens"] == 0, zero


def test_the_record_refuses_a_bad_caching_mode_or_primer_or_price():
    check = lambda meta: validate_meta(meta, _rec())  # noqa: E731
    assert check({"caching": "provider_cache"}) is None
    assert check({"caching": "none"}) is None
    assert check({"cache_primer_until": 12}) is None
    assert check({"price_input_per_mtok": 3}) is None
    assert check({"caching": "always"}) == (
        "caching='always' is not one of the legal values: none, cleanup, provider_cache"
    )
    assert check({"caching": True}) == (
        "caching=True is not one of the legal values: none, cleanup, provider_cache"
    )
    assert check({"cache_primer_until": 0}) == (
        "cache_primer_until=0 must be a row id (a positive int)"
    )
    # retired: an unknown key is not this backend's to refuse
    assert check({"prompt_cache": True}) is None
    assert "must be a number" in check({"price_output_per_mtok": "x"})
    assert "must be a number" in check({"price_input_per_mtok": True})
    assert "zero or more" in check({"price_input_per_mtok": -1})
    names = {f["name"] for f in RECORD_SCHEMA["fields"]}
    assert {
        "caching",
        "cache_primer_until",
        "price_input_per_mtok",
        "price_output_per_mtok",
    } <= names
    assert "prompt_cache" not in names


def test_make_provider_reads_the_caching_mode(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    mock_http(_api([]))
    import asyncio

    asyncio.run(_create(seeded_kernel, caching="provider_cache", max_tokens=8000))
    from anthropic_backend import tools

    p = tools.make_provider("a1", seeded_kernel)
    assert p.body(PLAIN, None)["cache_control"] == CACHED


def test_the_backend_declares_provider_cache(monkeypatch):
    from anthropic_backend import tools

    seen = {}
    real = tools.build
    monkeypatch.setattr(tools, "build", lambda **kw: (seen.update(kw), real(**kw))[1])
    tools._build()
    assert seen["provider_cache"] is True


# ─── usage priced per request, logged per request (#236) ─────────────────────


async def test_a_request_over_the_long_context_line_is_billed_long():
    from anthropic_backend.provider import Usage

    u = Usage()
    body = _sse(_usage_start(50, 50, 50), _text("ok"), *_usage_stop(10))
    await _drain(_provider(_transport(body), usage=u, long_context_tokens=100))
    assert (u.long_requests, u.requests) == (1, 0)
    s = u.snapshot(1.0, 5.0, 2.0, 10.0)
    assert s["long_context_requests"] == 1 and s["requests"] == 1
    cost = (50 + 1.25 * 50 + 0.1 * 50) * 2.0 / 1e6 + 10 * 10.0 / 1e6
    assert s["cost_usd"] == pytest.approx(cost, abs=1e-12)


async def test_a_request_under_the_line_is_billed_base():
    from anthropic_backend.provider import Usage

    u = Usage()
    body = _sse(_usage_start(50, 40, 0), _text("ok"), *_usage_stop(10))
    await _drain(_provider(_transport(body), usage=u, long_context_tokens=100))
    assert (u.long_requests, u.requests) == (0, 1)
    s = u.snapshot(1.0, 5.0, 2.0, 10.0)
    assert s["long_context_requests"] == 0
    cost = (50 + 1.25 * 40) * 1.0 / 1e6 + 10 * 5.0 / 1e6
    assert s["cost_usd"] == pytest.approx(cost, abs=1e-12)


async def test_without_long_prices_the_long_bucket_uses_the_base_prices():
    from anthropic_backend.provider import Usage

    u = Usage()
    body = _sse(_usage_start(50, 50, 50), _text("ok"), *_usage_stop(10))
    await _drain(_provider(_transport(body), usage=u, long_context_tokens=100))
    cost = (50 + 1.25 * 50 + 0.1 * 50) * 1.0 / 1e6 + 10 * 5.0 / 1e6
    assert u.snapshot(1.0, 5.0)["cost_usd"] == pytest.approx(cost, abs=1e-12)
    assert u.snapshot(1.0, 5.0, 2.0)["cost_usd"] == pytest.approx(cost, abs=1e-12)


async def test_the_usage_log_gets_one_line_per_request(tmp_path):
    from anthropic_backend.provider import Usage

    path = tmp_path / "usage.jsonl"
    u = Usage()
    first = _sse(_usage_start(100, 400, 1000), _text("a"), *_usage_stop(35))
    second = _sse(_usage_start(50, 0, 900), _text("b"), *_usage_stop(20))
    kw = dict(usage=u, usage_log=str(path), agent_id="a1")
    await _drain(_provider(_transport(first), **kw))
    await _drain(_provider(_transport(second), **kw))
    lines = [json.loads(x) for x in path.read_text().splitlines()]
    assert len(lines) == 2
    assert lines[0]["input_tokens"] == 100 and lines[0]["output_tokens"] == 35
    assert lines[1]["input_tokens"] == 50 and lines[1]["cache_read_tokens"] == 900
    assert lines[1]["output_tokens"] == 20
    assert lines[1]["long_context"] is False
    assert lines[1]["agent"] == "a1" and lines[1]["model"] == MODEL
    assert list(lines[0]) == [
        "ts",
        "agent",
        "model",
        "input_tokens",
        "cache_write_tokens",
        "cache_read_tokens",
        "output_tokens",
        "long_context",
    ]


async def test_a_usage_log_that_cannot_be_written_is_reported_not_raised(tmp_path):
    from anthropic_backend.provider import Usage

    path = str(tmp_path / "no-such-dir" / "usage.jsonl")
    u = Usage()
    body = _sse(_usage_start(100, 0, 0), _text("ok"), *_usage_stop(5))
    out = await _drain(_provider(_transport(body), usage=u, usage_log=path))
    assert out == "ok"
    assert u.snapshot()["usage_log_error"].startswith(path + ": ")
    u.reset()
    assert "usage_log_error" not in u.snapshot()


def test_the_new_fields_are_validated():
    def check(meta):
        return validate_meta(meta, _rec())

    assert (
        check({"long_context_tokens": 0})
        == "long_context_tokens must be a positive int"
    )
    assert (
        check({"long_context_tokens": "x"})
        == "long_context_tokens must be a positive int"
    )
    assert (
        check({"long_context_tokens": True})
        == "long_context_tokens must be a positive int"
    )
    assert check({"price_input_long_per_mtok": -1}) == (
        "price_input_long_per_mtok must be a number, zero or more"
    )
    assert check({"price_output_long_per_mtok": "x"}) == (
        "price_output_long_per_mtok must be a number, zero or more"
    )
    assert check({"usage_log": ""}) == "usage_log must be a non-empty string"
    assert check({"usage_log": 3}) == "usage_log must be a non-empty string"
    assert (
        check(
            {
                "long_context_tokens": 200000,
                "price_input_long_per_mtok": 6.0,
                "price_output_long_per_mtok": 22.5,
                "usage_log": "/tmp/usage.jsonl",
            }
        )
        is None
    )


def test_make_provider_reads_the_tier_and_the_log(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    mock_http(_api([]))
    import asyncio

    asyncio.run(
        _create(
            seeded_kernel,
            max_tokens=8000,
            long_context_tokens=200000,
            usage_log="/tmp/u.jsonl",
        )
    )
    from anthropic_backend import tools

    p = tools.make_provider("a1", seeded_kernel)
    assert p._long_context_tokens == 200000
    assert p._usage_log == "/tmp/u.jsonl"
    assert p._agent_id == "a1"


# ─── cache prices per model, 1-hour writes apart (#314 port) ─────────────────


def test_sonnet_cache_reads_bill_at_a_twentieth_of_input():
    from anthropic_backend.provider import Usage

    u = Usage()
    u.add_request(0, 0, 1_000_000)
    s = u.snapshot(3.0, 15.0, model="claude-sonnet-5-5")
    assert s["cost_usd"] == pytest.approx(0.15, abs=1e-12)
    assert s["saved_usd"] == pytest.approx(1_000_000 * (3.0 - 0.15) / 1e6, abs=1e-12)


def test_haiku_cache_reads_bill_at_a_tenth_and_only_dated_ids_match_sonnet():
    from anthropic_backend.provider import Usage

    def read_cost(model: str) -> float:
        u = Usage()
        u.add_request(0, 0, 1_000_000)
        return u.snapshot(3.0, 15.0, model=model)["cost_usd"]

    assert read_cost("claude-haiku-5-5") == pytest.approx(0.30, abs=1e-12)
    # a dated id of the cheap-read model matches; a longer version number does not
    assert read_cost("claude-sonnet-5-5-20261001") == pytest.approx(0.15, abs=1e-12)
    assert read_cost("claude-sonnet-5-50") == pytest.approx(0.30, abs=1e-12)
    assert read_cost("claude-opus-5-5") == pytest.approx(0.15, abs=1e-12)


def test_one_hour_cache_writes_bill_at_twice_input():
    from anthropic_backend.provider import Usage

    u = Usage()
    u.add_request(0, 1_000_000, 0, cache_write_1h=1_000_000)
    s = u.snapshot(3.0, 15.0)
    assert s["cache_write_tokens"] == 1_000_000
    assert s["cost_usd"] == pytest.approx(6.0, abs=1e-12)
    # a 1-hour write that is never read costs more than no cache: negative saving
    assert s["saved_usd"] == pytest.approx(-3.0, abs=1e-12)


async def test_a_one_hour_cache_write_is_counted_apart_from_the_five_minute_one():
    from anthropic_backend.provider import Usage

    start = {
        "type": "message_start",
        "message": {
            "usage": {
                "input_tokens": 0,
                "cache_creation_input_tokens": 500,
                "cache_read_input_tokens": 0,
                "cache_creation": {"ephemeral_1h_input_tokens": 200},
            }
        },
    }
    u = Usage()
    await _drain(_provider(_transport(_sse(start, _text("ok"), *_stop())), usage=u))
    assert u.cache_write_tokens == 300 and u.cache_write_1h_tokens == 200
    s = u.snapshot(3.0, 15.0)
    assert s["cache_write_tokens"] == 500
    # 300 at 1.25x of 3.0, 200 at 2x, output 5 at 15.0
    cost = (300 * 3.75 + 200 * 6.0 + 5 * 15.0) / 1e6
    assert s["cost_usd"] == pytest.approx(cost, abs=1e-12)

    # the long-context bucket splits the same way
    u = Usage()
    body = _sse(
        {
            "type": "message_start",
            "message": {
                "usage": {
                    "input_tokens": 50,
                    "cache_creation_input_tokens": 200,
                    "cache_read_input_tokens": 0,
                    "cache_creation": {"ephemeral_1h_input_tokens": 100},
                }
            },
        },
        _text("ok"),
        *_stop(),
    )
    await _drain(_provider(_transport(body), usage=u, long_context_tokens=100))
    assert (u.long_requests, u.long_cache_write_tokens) == (1, 100)
    assert u.long_cache_write_1h_tokens == 100
    s = u.snapshot(3.0, 15.0)
    assert s["cache_write_tokens"] == 200
    cost = (50 * 3.0 + 100 * 3.75 + 100 * 6.0 + 5 * 15.0) / 1e6
    assert s["cost_usd"] == pytest.approx(cost, abs=1e-12)


def test_the_record_refuses_a_bad_cache_price():
    check = lambda meta: validate_meta(meta, _rec())  # noqa: E731
    assert check({"price_cache_read_per_mtok": 0.3}) is None
    assert check({"price_cache_write_5m_per_mtok": 3.75}) is None
    assert check({"price_cache_write_1h_per_mtok": 6.0}) is None
    assert check({"price_cache_read_per_mtok": -1}) == (
        "price_cache_read_per_mtok must be a number, zero or more"
    )
    assert check({"price_cache_read_per_mtok": "x"}) == (
        "price_cache_read_per_mtok must be a number, zero or more"
    )
    assert check({"price_cache_write_1h_per_mtok": True}) == (
        "price_cache_write_1h_per_mtok must be a number, zero or more"
    )
    names = {f["name"] for f in RECORD_SCHEMA["fields"]}
    assert {
        "price_cache_read_per_mtok",
        "price_cache_write_5m_per_mtok",
        "price_cache_write_1h_per_mtok",
    } <= names


async def _create_as(kernel, agent_id: str, **meta):
    return await _call(
        kernel,
        kernel.id,
        {
            "type": "create_agent",
            "parent_id": kernel.id,
            "handler_module": "anthropic_backend.tools",
            "id": agent_id,
            **_rec(**meta),
        },
    )


async def test_a_record_without_cache_prices_uses_the_model_defaults(
    seeded_kernel, mock_http, fake_providers, _fresh_usage
):
    from anthropic_backend import tools

    mock_http(_api([]))
    base = dict(
        model="claude-sonnet-5-5", price_input_per_mtok=3.0, price_output_per_mtok=15.0
    )
    await _create_as(seeded_kernel, "a1", **base)
    await _create_as(
        seeded_kernel,
        "b1",
        **base,
        price_cache_read_per_mtok=1.0,
        price_input_long_per_mtok=6.0,
        price_output_long_per_mtok=15.0,
    )
    tools.usage_of("a1").add_request(0, 0, 1_000_000)
    tools.usage_of("b1").add_request(0, 0, 1_000_000)
    # no cache prices: the Sonnet 5.5 default, 0.05x of 3.0
    assert tools._usage_now("a1", seeded_kernel)["cost_usd"] == pytest.approx(0.15)
    # an explicit read price is USD at the base input price: 1.0 per million
    assert tools._usage_now("b1", seeded_kernel)["cost_usd"] == pytest.approx(1.0)
    # the long bucket scales the explicit price by long_in/base_in (6.0 / 3.0)
    tools.usage_of("a1").add_request(0, 0, 1_000_000, long=True)
    tools.usage_of("b1").add_request(0, 0, 1_000_000, long=True)
    # a1: no long prices, so the base 3.0 and the default 0.05x: 0.15 more
    assert tools._usage_now("a1", seeded_kernel)["cost_usd"] == pytest.approx(0.30)
    # b1: the long bucket at 2.0 per million reads (1.0 scaled by 6.0 / 3.0) on top
    assert tools._usage_now("b1", seeded_kernel)["cost_usd"] == pytest.approx(3.0)


# ─── usage across a kernel restart (#308, Rust #319) ─────────────────────────


@pytest.fixture
def _process():
    """A fresh kernel process: no counts in RAM from an earlier one."""
    from anthropic_backend import tools

    tools._USAGE.clear()
    tools._BASE.clear()
    yield
    tools._USAGE.clear()
    tools._BASE.clear()


def _restart(root):
    """A kernel restart: the records go to disk, the process's counts are dropped,
    and a fresh bootstrap reads the tree back."""
    from anthropic_backend import tools

    persist(root)
    tools._USAGE.clear()
    tools._BASE.clear()
    return boot_root()


async def _turn(kernel, mock_http, body: str = "") -> None:
    """One turn through the agent's own provider, drained (as Rust's turn() does)."""
    from anthropic_backend import tools

    body = body or _sse(_usage_start(100, 400, 1000), _text("ok"), *_usage_stop(35))
    mock_http(_transport(body))
    await _drain(tools.make_provider("a1", kernel))


def _sidecar(kernel) -> Path:
    """The agent's usage.json, beside its agent.yaml."""
    agent = kernel.ctx.get_agent("a1")
    return Path(kernel.ctx.workdir) / agent._root_path / "usage.json"


async def test_usage_survives_a_restart(seeded_kernel, mock_http, _process):
    mock_http(_api([]))
    await _create(seeded_kernel, max_tokens=8000)
    persist(seeded_kernel)  # the loader's flush: the agent's dir exists now
    await _turn(seeded_kernel, mock_http)
    assert _sidecar(seeded_kernel).is_file()
    k2 = _restart(seeded_kernel)
    got = await _call(k2, "a1", {"type": "usage"})
    assert got["requests"] == 1, got
    assert got["output_tokens"] == 35 and got["cache_read_tokens"] == 1000, got


async def test_usage_since_boot_starts_at_zero(seeded_kernel, mock_http, _process):
    mock_http(_api([]))
    await _create(seeded_kernel, max_tokens=8000)
    persist(seeded_kernel)  # the loader's flush: the agent's dir exists now
    await _turn(seeded_kernel, mock_http)
    k2 = _restart(seeded_kernel)
    r = await _call(k2, "a1", {"type": "reflect"})
    assert r["usage"]["requests"] == 1, r
    assert r["usage_since_boot"]["requests"] == 0, r
    await _turn(k2, mock_http)
    r = await _call(k2, "a1", {"type": "reflect"})
    assert r["usage"]["requests"] == 2, r
    assert r["usage_since_boot"]["requests"] == 1, r


async def test_usage_reset_clears_it(seeded_kernel, mock_http, _process):
    mock_http(_api([]))
    await _create(
        seeded_kernel,
        max_tokens=8000,
        price_input_per_mtok=3.0,
        price_output_per_mtok=15.0,
    )
    persist(seeded_kernel)  # the loader's flush: the agent's dir exists now
    await _turn(seeded_kernel, mock_http)
    got = await _call(seeded_kernel, "a1", {"type": "usage_reset"})
    assert got["requests"] == 1 and got["cost_usd"] > 0, got
    zero = await _call(seeded_kernel, "a1", {"type": "usage"})
    assert zero["requests"] == 0 and zero["cost_usd"] == 0.0, zero
    k2 = _restart(seeded_kernel)
    again = await _call(k2, "a1", {"type": "usage"})
    assert again["requests"] == 0 and again["cost_usd"] == 0.0, again


async def test_usage_reset_true_zeroes_only_since_boot(
    seeded_kernel, mock_http, _process
):
    mock_http(_api([]))
    await _create(seeded_kernel, max_tokens=8000)
    persist(seeded_kernel)  # the loader's flush: the agent's dir exists now
    await _turn(seeded_kernel, mock_http)
    k2 = _restart(seeded_kernel)
    await _turn(k2, mock_http)
    got = await _call(k2, "a1", {"type": "usage", "reset": True})
    assert got["requests"] == 2 and got["usage_since_boot"]["requests"] == 1, got
    after = await _call(k2, "a1", {"type": "usage"})
    assert after["usage_since_boot"]["requests"] == 0, after


async def test_cost_usd_accumulates_across_a_restart(
    seeded_kernel, mock_http, _process
):
    mock_http(_api([]))
    await _create(
        seeded_kernel,
        max_tokens=8000,
        price_input_per_mtok=3.0,
        price_output_per_mtok=15.0,
    )
    persist(seeded_kernel)  # the loader's flush: the agent's dir exists now
    await _turn(seeded_kernel, mock_http)
    first = (await _call(seeded_kernel, "a1", {"type": "usage"}))["cost_usd"]
    assert first == pytest.approx(0.002625), first
    k2 = _restart(seeded_kernel)
    await _turn(k2, mock_http)
    got = await _call(k2, "a1", {"type": "usage"})
    assert got["cost_usd"] == pytest.approx(2 * first), got


async def test_a_corrupt_usage_file_starts_clean(seeded_kernel, mock_http, _process):
    mock_http(_api([]))
    await _create(seeded_kernel, max_tokens=8000)
    persist(seeded_kernel)  # the loader's flush: the agent's dir exists now
    await _turn(seeded_kernel, mock_http)
    _sidecar(seeded_kernel).write_text("{not json")
    k2 = _restart(seeded_kernel)
    got = await _call(k2, "a1", {"type": "usage"})
    assert got["requests"] == 0 and "usage_log_error" not in got, got
    await _turn(k2, mock_http)
    saved = json.loads(_sidecar(k2).read_text())
    assert saved["requests"] == 1, saved


def test_effort_and_thinking_display_shape_the_body():
    msgs = [{"role": "user", "content": "hi"}]
    b = _provider(None, effort="high").body(msgs, None)
    assert b["output_config"] == {"effort": "high"} and "thinking" not in b
    b = _provider(None, thinking_display="summarized", thinking_budget=2048).body(
        msgs, None
    )
    assert b["thinking"] == {"type": "adaptive", "display": "summarized"}
    assert "output_config" not in b
    b = _provider(None).body(msgs, None)
    assert "thinking" not in b and "output_config" not in b


def test_a_bad_effort_or_display_or_both_modes_are_refused():
    def check(meta):
        return validate_meta(meta, _rec())

    assert check({"effort": "ultra"}) == (
        "effort must be one of low, medium, high, xhigh, max"
    )
    assert check({"thinking_display": "verbose"}) == (
        "thinking_display must be summarized or omitted"
    )
    assert check({"thinking_display": "summarized", "thinking_budget": 2048}) == (
        "thinking_display and thinking_budget are two thinking modes — set one"
    )
    assert check({"effort": "xhigh", "thinking_display": "omitted"}) is None


def test_budget_thinking_refused_reads_the_model_family():
    from anthropic_backend.provider import budget_thinking_refused

    for m in (
        "claude-sonnet-5-5",
        "claude-opus-4-7",
        "claude-fable-5-1",
        "claude-haiku-5-5-20260101",
    ):
        assert budget_thinking_refused(m), m
    for m in ("claude-opus-4-6", "claude-3-5-sonnet-20241022", "my-model"):
        assert not budget_thinking_refused(m), m


def test_thinking_budget_is_refused_for_a_5x_model():
    msg = validate_meta({"model": "claude-sonnet-5-5", "thinking_budget": 2048}, _rec())
    assert msg is not None
    assert "thinking_budget is refused by claude-sonnet-5-5" in msg
    assert "thinking_display (summarized|omitted) and effort instead" in msg
    assert (
        validate_meta({"model": "claude-opus-4-6", "thinking_budget": 2048}, _rec())
        is None
    )


# ─── the read clock (#177) ───────────────────────────────────────────────────


class _SlowStream(httpx.AsyncByteStream):
    """A body of chunks, a pause before each."""

    def __init__(self, chunks: list[bytes], pause: float):
        self._chunks, self._pause = chunks, pause

    async def __aiter__(self):
        import asyncio

        for c in self._chunks:
            await asyncio.sleep(self._pause)
            yield c


async def test_a_silent_api_stream_is_stalled_not_hung():
    from ai_core.provider import ProviderError

    first = _sse(START, _text("par")).encode()

    class _Silent(httpx.AsyncByteStream):
        async def __aiter__(self):
            yield first
            raise httpx.ReadTimeout("no byte")

    t = httpx.MockTransport(lambda req: httpx.Response(200, stream=_Silent()))
    err = await _turn_error(_provider(t, stream_idle=0.05))
    assert isinstance(err, ProviderError) and err.transport is True
    assert str(err).startswith("anthropic_backend: stream_stalled")
    assert ENDPOINT in str(err) and "0.05s" in str(err)


async def test_a_future_still_pending_when_the_window_passes_is_stalled():
    from ai_core.provider import ProviderError

    class _Pending(httpx.AsyncByteStream):
        async def __aiter__(self):
            raise httpx.ReadTimeout("window passed")
            yield b""

    t = httpx.MockTransport(lambda req: httpx.Response(200, stream=_Pending()))
    err = await _turn_error(_provider(t, stream_idle=5.0))
    assert isinstance(err, ProviderError) and err.transport is True
    assert str(err).startswith("anthropic_backend: stream_stalled")
    assert ENDPOINT in str(err) and "5s" in str(err)


async def test_a_future_that_answers_within_the_window_passes_its_output():
    chunks = [_sse(START).encode(), _sse(_text("fine")).encode()]
    chunks += [_sse(*_stop()).encode()]
    t = httpx.MockTransport(
        lambda req: httpx.Response(200, stream=_SlowStream(chunks, 0.01))
    )
    assert await _drain(_provider(t, stream_idle=0.5)) == "fine"


async def test_the_stream_runs_on_the_read_clock():
    seen = {}

    def handler(req: httpx.Request) -> httpx.Response:
        seen["timeout"] = req.extensions["timeout"]
        return httpx.Response(200, text=_sse(START, _text("ok"), *_stop()))

    await _drain(_provider(httpx.MockTransport(handler), stream_idle=7.5))
    assert seen["timeout"]["read"] == 7.5 and seen["timeout"]["connect"] == 15.0
    await _drain(_provider(httpx.MockTransport(handler)))
    assert seen["timeout"]["read"] == 60.0


async def test_pings_keep_the_clock_alive():
    ping = _sse({"type": "ping"}).encode()
    chunks = [_sse(START).encode(), ping, ping, ping]
    chunks += [_sse(_text("done"), *_stop()).encode()]
    t = httpx.MockTransport(
        lambda req: httpx.Response(200, stream=_SlowStream(chunks, 0.01))
    )
    assert await _drain(_provider(t, stream_idle=0.5)) == "done"


def test_stream_idle_seconds_is_validated_at_the_write():
    def check(meta):
        return validate_meta(meta, _rec())

    for bad in (0, -1, "x"):
        msg = check({"stream_idle_seconds": bad})
        assert msg and msg.startswith(
            "stream_idle_seconds must be a positive number of seconds"
        )
    assert check({"stream_idle_seconds": 2.5}) is None
    assert "stream_idle_seconds" in [f["name"] for f in RECORD_SCHEMA["fields"]]


# ── twins of the rust anthropic_backend tests (#170) ─────────────────────────


async def test_an_endpoint_that_never_answers_fails_after_the_idle_window():
    """No response headers ever arrive: the handler raises ReadTimeout before any
    response, and the turn fails naming the stall, not a connect error (#177)."""
    from ai_core.provider import ProviderError

    def never(req: httpx.Request) -> httpx.Response:
        raise httpx.ReadTimeout("no headers", request=req)

    err = await _turn_error(_provider(httpx.MockTransport(never), stream_idle=0.2))
    assert isinstance(err, ProviderError) and err.transport is True
    assert "stream_stalled" in str(err)
    assert "sent no bytes for 0.2s" in str(err)
    assert ENDPOINT in str(err)


def test_the_sidecar_keeps_the_one_hour_write_count():
    from anthropic_backend.provider import Usage

    u = Usage()
    u.cache_write_1h_tokens = 40
    u.long_cache_write_1h_tokens = 7
    back = Usage.from_json(u.to_json())
    assert (back.cache_write_1h_tokens, back.long_cache_write_1h_tokens) == (40, 7)
    # a sidecar written before the field existed loads with 0
    old = Usage.from_json({"requests": 3, "cache_write": 5})
    assert (old.requests, old.cache_write_tokens) == (3, 5)
    assert (old.cache_write_1h_tokens, old.long_cache_write_1h_tokens) == (0, 0)
