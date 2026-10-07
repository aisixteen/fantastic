"""anthropic_backend — the Messages wire it speaks, what it reads from the API when
the record leaves it out, and the optional key (owner, 2026-10-07)."""

from __future__ import annotations

import json

import httpx
import pytest
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
