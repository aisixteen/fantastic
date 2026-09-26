"""nvidia_nim_backend handler — verb dispatching, file_bridge_id + api_key
failfast, multi-step _run, key sidecar via file_bridge_id, rate-limit retry.

Mirror of ollama_backend's test_handler.py with the provider stubbed
out and the api-key surface (the key comes from the ENVIRONMENT; only the
has_api_key flag appears in reflect) plus 429-retry behavior covered."""

from __future__ import annotations

from _testkit import ensure_memory
import asyncio
import json
import os

import httpx
import pytest

import nvidia_nim_backend.tools as nt
from nvidia_nim_backend.tools import KEY_VAR


class _FakeProvider:
    """Provider stub that yields a scripted stream from .chat()."""

    def __init__(self, scripts: list[list]):
        self._scripts = list(scripts)
        self.calls = 0

    async def chat(self, messages, tools=None, max_tokens=None):
        items = self._scripts.pop(0) if self._scripts else []
        self.calls += 1
        for x in items:
            yield x


async def _accept(kernel, target: str, caller: str, verbs="*") -> None:
    """Open the callee's ingress so a granted egress lands — merges (keeps baseline)."""
    tgt = kernel.ctx.agents.get(target)
    cb = dict(tgt._meta.get("callable_by") or {}) if tgt is not None else {}
    cb[caller] = verbs
    (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "update_agent",
                        "id": target,
                        "callable_by": cb,
                    },
                }
            ]
        )
    )[0]["result"]


@pytest.fixture(autouse=True)
def _clean_key_env():
    """The key is process-global env now, so a test that seeds it must not leak
    into the next one — restore whatever was there."""
    prev = os.environ.get(KEY_VAR)
    os.environ.pop(KEY_VAR, None)
    yield
    if prev is None:
        os.environ.pop(KEY_VAR, None)
    else:
        os.environ[KEY_VAR] = prev


async def _make_nvidia(kernel, file_bridge_id=None, with_key: str | None = None):
    # `context_window` explicitly: the 4096 default leaves only ~1k for the conversation
    # once the reflect primer + howtos are in the system block, so these tests (about
    # queueing, history and status — not context math) would fail on
    # `context_insufficient` for reasons unrelated to what they assert.
    # Deny-all: the backend streams its turn to `cli` via send (default client).
    store = ensure_memory(kernel)
    can_call = {"cli": "*", store: "*"}
    meta = {
        "handler_module": "nvidia_nim_backend.tools",
        "context_window": 32768,
        "can_call": can_call,
        "memory_store_id": store,
    }
    if file_bridge_id is not None:
        meta["file_bridge_id"] = file_bridge_id
        can_call[file_bridge_id] = "*"
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
    nid = rec["id"]
    # Two-sided deny-all: open each callee's reciprocal `callable_by`.
    for target in can_call:
        await _accept(kernel, target, nid)
    # `with_key` puts the key in the ENVIRONMENT — the only place the bundle
    # looks. It used to write a sidecar through the file agent; nothing is
    # written to disk any more, so there is no sidecar to seed.
    if with_key:
        os.environ[KEY_VAR] = with_key
    return nid


# ─── reflect / failfast ────────────────────────────────────────


async def test_reflect_carries_no_file_bridge_id_and_no_history(
    seeded_kernel, store_agent
):
    nid = await _make_nvidia(seeded_kernel, store_agent)
    r = (await seeded_kernel.send([{"target": nid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert "file_bridge_id" not in r
    assert "send" in r["verbs"]
    # The key-storage verbs are GONE — the key is read from the environment.
    assert "set_api_key" not in r["verbs"]
    assert "clear_api_key" not in r["verbs"]


async def test_reflect_has_api_key_tracks_the_env(seeded_kernel, store_agent):
    """has_api_key reports the boolean — never the key value itself."""
    nid = await _make_nvidia(seeded_kernel, store_agent)
    r0 = (await seeded_kernel.send([{"target": nid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert r0["has_api_key"] is False
    blob0 = json.dumps(r0)
    assert "nvapi-" not in blob0

    os.environ[KEY_VAR] = "nvapi-secret-xxxxxxxxxxxx"
    r1 = (await seeded_kernel.send([{"target": nid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert r1["has_api_key"] is True
    blob1 = json.dumps(r1)
    # Reflect must never leak the key value.
    assert "nvapi-secret" not in blob1


async def test_send_without_provider_failfasts(seeded_kernel):
    # No file_bridge_id and no key in the env ⇒ make_provider returns None ⇒ failfast
    # (chat history no longer needs file_bridge_id — it's a mounted chat JSONL sidecar).
    nid = await _make_nvidia(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [{"target": nid, "payload": {"type": "send", "text": "hi"}}]
        )
    )[0]["result"]
    assert "error" in r


async def test_send_requires_api_key(seeded_kernel, store_agent):
    """Key variable unset → send failfasts before touching the network, and the
    refusal NAMES the variable so an unwired connector says what to set."""
    nid = await _make_nvidia(seeded_kernel, store_agent)
    r = (
        await seeded_kernel.send(
            [{"target": nid, "payload": {"type": "send", "text": "hi"}}]
        )
    )[0]["result"]
    assert "error" in r
    assert KEY_VAR in r["error"]


async def test_history_empty_when_no_turns(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent)
    r = (await seeded_kernel.send([{"target": nid, "payload": {"type": "recall"}}]))[0][
        "result"
    ]
    assert r["messages"] == []


async def test_unknown_verb_errors(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent)
    r = (await seeded_kernel.send([{"target": nid, "payload": {"type": "garbage"}}]))[
        0
    ]["result"]
    assert "error" in r


async def test_history_returns_messages(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    fp = _FakeProvider([['hello <call name="done"></call>']])
    nt._providers[nid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": nid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
    finally:
        nt._providers.pop(nid, None)
    r = (await seeded_kernel.send([{"target": nid, "payload": {"type": "recall"}}]))[0][
        "result"
    ]
    assert [m["content"] for m in r["messages"]] == [
        "hi",
        'hello <call name="done"></call>',
    ]


# ─── _run multi-step / persistence ────────────────────────────


async def test_run_no_tool_calls_returns_content(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    fp = _FakeProvider([["Hello! ", "How are you?"]])
    nt._providers[nid] = fp
    try:
        r = (
            await seeded_kernel.send(
                [{"target": nid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
        assert r["final"] == "Hello! How are you?"
        assert fp.calls == 1
    finally:
        nt._providers.pop(nid, None)


async def test_run_persists_history(seeded_kernel, store_agent):
    # History lands in the mounted chat JSONL sidecar (persisted THROUGH the loader),
    # NOT a chat_<client>.json the AI writes itself — read it back via `history`.
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    fp = _FakeProvider([["reply"]])
    nt._providers[nid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": nid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
    finally:
        nt._providers.pop(nid, None)
    data = (
        (await seeded_kernel.send([{"target": nid, "payload": {"type": "recall"}}]))[0][
            "result"
        ]
    )["messages"]
    assert (data[-2]["role"], data[-2]["content"]) == ("user", "hi")
    assert (data[-1]["role"], data[-1]["content"]) == ("assistant", "reply")


async def test_cli_caller_routes_to_cli_only(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    fp = _FakeProvider([["chunk1 ", "chunk2"]])
    nt._providers[nid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            (
                await seeded_kernel.send(
                    [{"target": nid, "payload": {"type": "send", "text": "hi"}}]
                )
            )[0]["result"]

        cli_tokens = [p for t, p in sends if t == "cli" and p.get("type") == "token"]
        cli_done = [
            p
            for t, p in sends
            if t == "cli" and (p.get("type") == "status" and p.get("phase") == "done")
        ]
        assert len(cli_tokens) == 2
        assert len(cli_done) == 1
        own_stream = [
            p
            for t, p in emits
            if t == nid
            and p.get("type") in ("token", "say")
            or (p.get("type") == "status" and p.get("phase") == "done")
        ]
        assert own_stream == []
        assert all(p.get("client_id") == "cli" for p in cli_tokens + cli_done)
    finally:
        nt._providers.pop(nid, None)


async def test_browser_caller_routes_to_own_inbox_only(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    fp = _FakeProvider([["a", "b"]])
    nt._providers[nid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            (
                await seeded_kernel.send(
                    [
                        {
                            "target": nid,
                            "payload": {
                                "type": "send",
                                "text": "hi",
                                "client_id": "web_xyz",
                            },
                        }
                    ]
                )
            )[0]["result"]

        own_tokens = [p for t, p in emits if t == nid and p.get("type") == "token"]
        own_done = [
            p
            for t, p in emits
            if t == nid and (p.get("type") == "status" and p.get("phase") == "done")
        ]
        assert len(own_tokens) == 2
        assert len(own_done) == 1
        assert all(p["client_id"] == "web_xyz" for p in own_tokens + own_done)
        cli_stream = [
            p
            for t, p in sends
            if t == "cli"
            and p.get("type") in ("token", "say")
            or (p.get("type") == "status" and p.get("phase") == "done")
        ]
        assert cli_stream == []
    finally:
        nt._providers.pop(nid, None)


# ─── menu ───────────────────────────────────────────────────────


async def test_contended_send_emits_queued_event(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")

    class _SlowFirst:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None, max_tokens=None):
            self.calls += 1
            if self.calls == 1:
                await asyncio.sleep(0.20)
            yield 'ok <call name="done"></call>'

    nt._providers[nid] = _SlowFirst()

    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            t1 = asyncio.create_task(
                seeded_kernel.send(
                    [
                        {
                            "target": nid,
                            "payload": {
                                "type": "send",
                                "text": "first",
                                "client_id": "alice",
                            },
                        }
                    ]
                )
            )
            await asyncio.sleep(0.05)
            t2 = asyncio.create_task(
                seeded_kernel.send(
                    [
                        {
                            "target": nid,
                            "payload": {
                                "type": "send",
                                "text": "second",
                                "client_id": "bob",
                            },
                        }
                    ]
                )
            )
            (o1,), (o2,) = await asyncio.gather(t1, t2)
            r1, r2 = o1["result"], o2["result"]

        assert r1["final"].startswith("ok") and r2["final"].startswith("ok")

        bob_queued = [
            p
            for t, p in emits
            if t == nid
            and (p.get("type") == "status" and p.get("phase") == "queued")
            and p.get("client_id") == "bob"
        ]
        assert len(bob_queued) == 1
        all_pairs = [("send", t, p) for t, p in sends] + [
            ("emit", t, p) for t, p in emits
        ]
        alice_queued = [
            p
            for kind, t, p in all_pairs
            if (p.get("type") == "status" and p.get("phase") == "queued")
            and p.get("client_id") == "alice"
        ]
        assert alice_queued == []
    finally:
        nt._providers.pop(nid, None)


async def test_concurrent_sends_serialize_per_backend(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")

    in_flight = 0
    max_in_flight = 0
    order: list[str] = []

    class _SerialProvider:
        async def chat(self, messages, tools=None, max_tokens=None):
            nonlocal in_flight, max_in_flight
            in_flight += 1
            max_in_flight = max(max_in_flight, in_flight)
            try:
                await asyncio.sleep(0.05)
                user = messages[-1]["content"]
                order.append(user)
                yield 'ok <call name="done"></call>'
            finally:
                in_flight -= 1

    nt._providers[nid] = _SerialProvider()
    try:
        results = await asyncio.gather(
            seeded_kernel.send(
                [
                    {
                        "target": nid,
                        "payload": {"type": "send", "text": "first", "client_id": "a"},
                    }
                ]
            ),
            seeded_kernel.send(
                [
                    {
                        "target": nid,
                        "payload": {"type": "send", "text": "second", "client_id": "b"},
                    }
                ]
            ),
        )
        assert max_in_flight == 1
        assert order == ["first", "second"]
        assert all(outs[0]["result"]["final"].startswith("ok") for outs in results)
    finally:
        nt._providers.pop(nid, None)


# ─── rate-limit retry (429) ────────────────────────────────────


def _http_status_error(code: int, retry_after: str | None = None):
    """Construct a real httpx.HTTPStatusError with a synthetic response
    so fake providers can raise it like the real httpx stream would."""
    headers = {"retry-after": retry_after} if retry_after is not None else {}
    req = httpx.Request("POST", "https://integrate.api.nvidia.com/v1/chat/completions")
    resp = httpx.Response(code, headers=headers, request=req)
    return httpx.HTTPStatusError(f"http {code}", request=req, response=resp)


def test_parse_retry_after_clamps_and_defaults():
    """Numeric → clamped to [1, 60]; missing/garbage → default 5."""

    def make(val):
        h = {} if val is None else {"retry-after": val}
        return httpx.Response(429, headers=h)

    assert nt._parse_retry_after(make("3")) == 3
    assert nt._parse_retry_after(make("0")) == 1  # min clamp
    assert nt._parse_retry_after(make("999")) == nt.RATE_LIMIT_MAX_WAIT
    assert nt._parse_retry_after(make(None)) == nt.RATE_LIMIT_DEFAULT_WAIT
    assert nt._parse_retry_after(make("garbage")) == nt.RATE_LIMIT_DEFAULT_WAIT


async def test_send_retries_once_on_rate_limit(seeded_kernel, store_agent):
    """Provider raises 429 on first chat() call (before any chunk yielded);
    backend sleeps Retry-After, emits a status(thinking, waiting_on=
    'rate_limit') event, retries, succeeds."""
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")

    class _RateLimitedThenOk:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None, max_tokens=None):
            self.calls += 1
            if self.calls == 1:
                raise _http_status_error(429, retry_after="0")
            yield 'ok <call name="done"></call>'

    fp = _RateLimitedThenOk()
    nt._providers[nid] = fp

    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            r = (
                await seeded_kernel.send(
                    [{"target": nid, "payload": {"type": "send", "text": "hi"}}]
                )
            )[0]["result"]
        assert r["final"].startswith("ok")
        assert fp.calls == 2  # initial + retry
        # status(thinking, waiting_on='rate_limit') surfaced the wait to
        # the caller (cli — the default client routes via `send`).
        rate_status = [
            p
            for t, p in sends
            if t == "cli"
            and p.get("type") == "status"
            and p.get("phase") == "thinking"
            and p.get("detail", {}).get("waiting_on") == "rate_limit"
        ]
        assert len(rate_status) == 1, f"expected one rate-limit status, got {sends}"
    finally:
        nt._providers.pop(nid, None)


async def test_send_surfaces_error_after_repeated_rate_limit(
    seeded_kernel, store_agent
):
    """Two 429s in a row → retry exhausted → `_send` returns a clean error."""
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")

    class _AlwaysRateLimited:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None, max_tokens=None):
            self.calls += 1
            raise _http_status_error(429, retry_after="0")
            yield  # unreachable; makes this an async generator

    fp = _AlwaysRateLimited()
    nt._providers[nid] = fp
    try:
        r = (
            await seeded_kernel.send(
                [{"target": nid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
    finally:
        nt._providers.pop(nid, None)

    assert "error" in r
    assert "rate limited" in r["error"].lower()
    assert "429" in r["error"]
    # 1 initial + 1 retry = 2; we don't retry indefinitely.
    assert fp.calls == 2


async def test_send_does_not_retry_on_non_429_http_error(seeded_kernel, store_agent):
    """500/503/auth errors are NOT retried — surfaced once, cleanly."""
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")

    class _ServerError:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None, max_tokens=None):
            self.calls += 1
            raise _http_status_error(503)
            yield

    fp = _ServerError()
    nt._providers[nid] = fp
    try:
        r = (
            await seeded_kernel.send(
                [{"target": nid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
    finally:
        nt._providers.pop(nid, None)

    assert "error" in r
    assert "503" in r["error"]
    assert fp.calls == 1  # no retry on 5xx


async def test_mid_stream_429_propagates_without_retry(seeded_kernel, store_agent):
    """If a 429 fires AFTER chunks were yielded (rare; quota usually
    checked up front), retrying would duplicate tokens — so we don't.
    The error surfaces; partial tokens already streamed are fine."""
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")

    class _MidStream429:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None, max_tokens=None):
            self.calls += 1
            yield "partial "
            raise _http_status_error(429, retry_after="0")

    fp = _MidStream429()
    nt._providers[nid] = fp
    try:
        r = (
            await seeded_kernel.send(
                [{"target": nid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
    finally:
        nt._providers.pop(nid, None)

    assert "error" in r
    assert "429" in r["error"]
    assert fp.calls == 1  # mid-stream → no retry


# ─── housekeeping: assert key file path matches the design ────


# ─── status pipeline (mirrors ollama_backend) ──────────────────


def _capture(seeded_kernel):
    """State-subscriber-based capture of send/emit events. Bundle-
    internal sends bypass root.send, so we listen on the state stream."""
    sends: list[tuple[str, dict]] = []
    emits: list[tuple[str, dict]] = []

    def _on_event(e: dict) -> None:
        kind = e.get("kind")
        if kind == "send":
            sends.append((e["agent_id"], e["payload"]))
        elif kind == "emit":
            emits.append((e["agent_id"], e["payload"]))

    class _Ctx:
        def __enter__(self):
            self._unsub = seeded_kernel.add_state_subscriber(_on_event)
            return self

        def __exit__(self, *exc):
            self._unsub()
            return False

    return sends, emits, _Ctx()


async def test_status_event_sequence_no_tool_calls(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    fp = _FakeProvider([['hello <call name="done"></call>']])
    nt._providers[nid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            (
                await seeded_kernel.send(
                    [
                        {
                            "target": nid,
                            "payload": {
                                "type": "send",
                                "text": "hi",
                                "client_id": "alice",
                            },
                        }
                    ]
                )
            )[0]["result"]
        statuses = [p for _, p in emits if p.get("type") == "status"]
        phases = [s["phase"] for s in statuses]
        assert phases == ["thinking", "streaming", "done"], phases
        assert statuses[-1]["detail"]["reason"] == "ok"
        send_ids = {s["detail"].get("send_id") for s in statuses}
        assert len(send_ids) == 1 and None not in send_ids
    finally:
        nt._providers.pop(nid, None)


async def test_queue_populated_and_drained_on_contention(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    queue_sizes: list[int] = []

    class _SlowFirst:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None, max_tokens=None):
            self.calls += 1
            if self.calls == 1:
                await asyncio.sleep(0.10)
                queue_sizes.append(len(nt._queue.get(nid, [])))
                await asyncio.sleep(0.10)
            yield 'ok <call name="done"></call>'

    nt._providers[nid] = _SlowFirst()
    try:
        t1 = asyncio.create_task(
            seeded_kernel.send(
                [
                    {
                        "target": nid,
                        "payload": {
                            "type": "send",
                            "text": "first",
                            "client_id": "alice",
                        },
                    }
                ]
            )
        )
        await asyncio.sleep(0.05)
        t2 = asyncio.create_task(
            seeded_kernel.send(
                [
                    {
                        "target": nid,
                        "payload": {
                            "type": "send",
                            "text": "second",
                            "client_id": "bob",
                        },
                    }
                ]
            )
        )
        await asyncio.gather(t1, t2)
    finally:
        nt._providers.pop(nid, None)

    assert queue_sizes == [1], queue_sizes
    assert nt._queue.get(nid, []) == []
    assert nid not in nt._current


async def test_status_verb_shape_when_idle(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent)
    r = (
        await seeded_kernel.send(
            [{"target": nid, "payload": {"type": "status", "client_id": "alice"}}]
        )
    )[0]["result"]
    assert r["generating"] is False
    assert r["current"] is None
    assert r["mine_pending"] == []
    assert r["others_pending"] == 0
    assert r["client_id"] == "alice"


async def test_status_verb_shape_during_inflight_mine(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    captured: list[dict] = []

    class _SlowProvider:
        async def chat(self, messages, tools=None, max_tokens=None):
            captured.append(
                (
                    await seeded_kernel.send(
                        [
                            {
                                "target": nid,
                                "payload": {"type": "status", "client_id": "alice"},
                            }
                        ]
                    )
                )[0]["result"]
            )
            yield 'ok <call name="done"></call>'

    nt._providers[nid] = _SlowProvider()
    try:
        (
            await seeded_kernel.send(
                [
                    {
                        "target": nid,
                        "payload": {
                            "type": "send",
                            "text": "hello there",
                            "client_id": "alice",
                        },
                    }
                ]
            )
        )[0]["result"]
    finally:
        nt._providers.pop(nid, None)

    snap = captured[0]
    assert snap["generating"] is True
    assert snap["current"]["is_mine"] is True
    assert snap["current"]["phase"] in ("thinking", "streaming")
    assert snap["current"]["text"] == "hello there"


async def test_status_verb_privacy_filter(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    snapshots: dict[str, dict] = {}

    class _PrivacyProvider:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None, max_tokens=None):
            self.calls += 1
            if self.calls == 1:
                await asyncio.sleep(0.15)
                snapshots["alice"] = (
                    await seeded_kernel.send(
                        [
                            {
                                "target": nid,
                                "payload": {"type": "status", "client_id": "alice"},
                            }
                        ]
                    )
                )[0]["result"]
                snapshots["bob"] = (
                    await seeded_kernel.send(
                        [
                            {
                                "target": nid,
                                "payload": {"type": "status", "client_id": "bob"},
                            }
                        ]
                    )
                )[0]["result"]
            yield 'ok <call name="done"></call>'

    nt._providers[nid] = _PrivacyProvider()
    try:
        t1 = asyncio.create_task(
            seeded_kernel.send(
                [
                    {
                        "target": nid,
                        "payload": {
                            "type": "send",
                            "text": "bob secret",
                            "client_id": "bob",
                        },
                    }
                ]
            )
        )
        await asyncio.sleep(0.05)
        t2 = asyncio.create_task(
            seeded_kernel.send(
                [
                    {
                        "target": nid,
                        "payload": {
                            "type": "send",
                            "text": "alice secret",
                            "client_id": "alice",
                        },
                    }
                ]
            )
        )
        await asyncio.gather(t1, t2)
    finally:
        nt._providers.pop(nid, None)

    a = snapshots["alice"]
    assert a["current"]["is_mine"] is False
    assert "text" not in a["current"]
    assert len(a["mine_pending"]) == 1
    assert a["mine_pending"][0]["text"] == "alice secret"
    assert a["others_pending"] == 0

    b = snapshots["bob"]
    assert b["current"]["is_mine"] is True
    assert b["current"]["text"] == "bob secret"
    assert b["mine_pending"] == []
    assert b["others_pending"] == 1


async def test_status_verb_no_client_id_redacts_text(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    holder: list[dict] = []

    class _RedactProvider:
        async def chat(self, messages, tools=None, max_tokens=None):
            holder.append(
                (
                    await seeded_kernel.send(
                        [{"target": nid, "payload": {"type": "status"}}]
                    )
                )[0]["result"]
            )
            yield 'ok <call name="done"></call>'

    nt._providers[nid] = _RedactProvider()
    try:
        (
            await seeded_kernel.send(
                [
                    {
                        "target": nid,
                        "payload": {
                            "type": "send",
                            "text": "private",
                            "client_id": "alice",
                        },
                    }
                ]
            )
        )[0]["result"]
    finally:
        nt._providers.pop(nid, None)

    snap = holder[0]
    assert snap["client_id"] is None
    assert snap["current"] is not None
    assert "text" not in snap["current"]
    assert "text_so_far" not in snap["current"]


async def test_kernel_cancel_kills_an_in_flight_send(seeded_kernel, store_agent):
    """No bespoke `interrupt` verb: an in-flight `send` is a KERNEL task, killed via
    `ps` → `cancel`. The shared `on_cancel` hook (inherited from ai_core) closes the
    turn out; the blocked caller gets {'cancelled': True} and no phantom generation
    survives."""
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")

    class _SlowProvider:
        async def chat(self, messages, tools=None, max_tokens=None):
            await asyncio.sleep(5.0)
            yield "never"

    nt._providers[nid] = _SlowProvider()
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            send_task = asyncio.create_task(
                seeded_kernel.send(
                    [
                        {
                            "target": nid,
                            "payload": {
                                "type": "send",
                                "text": "stop me",
                                "client_id": "alice",
                            },
                        }
                    ]
                )
            )
            await asyncio.sleep(0.10)
            table = (
                await seeded_kernel.send(
                    [{"target": "kernel", "payload": {"type": "ps"}}]
                )
            )[0]["result"]
            rows = [
                t for t in table["tasks"] if t["agent"] == nid and t["verb"] == "send"
            ]
            assert len(rows) == 1, table["tasks"]
            task_id = rows[0]["id"]
            r = (
                await seeded_kernel.send(
                    [
                        {
                            "target": "kernel",
                            "payload": {"type": "cancel", "task_id": task_id},
                        }
                    ]
                )
            )[0]["result"]
            assert r == {"cancelled": True, "killed": [task_id]}
            out = (await send_task)[0]
            assert out["status"] == "cancelled"
            assert out["result"] == {"cancelled": True}

        done_status = [
            p
            for _, p in emits
            if p.get("type") == "status" and p.get("phase") == "done"
        ]
        assert len(done_status) == 1
        assert done_status[0]["detail"]["reason"] == "cancelled"
        assert any(
            (p.get("type") == "status" and p.get("phase") == "done") for _, p in emits
        )

        snap = (
            await seeded_kernel.send([{"target": nid, "payload": {"type": "status"}}])
        )[0]["result"]
        assert snap["generating"] is False
        rr = (
            await seeded_kernel.send([{"target": nid, "payload": {"type": "reflect"}}])
        )[0]["result"]
        assert rr["generating"] is False
        assert "interrupt" not in rr["verbs"]
    finally:
        nt._providers.pop(nid, None)


async def test_status_done_emits_with_reason_timeout(
    seeded_kernel, store_agent, monkeypatch
):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    monkeypatch.setattr(nt, "SEND_TIMEOUT", 0.05)

    class _ForeverProvider:
        async def chat(self, messages, tools=None, max_tokens=None):
            await asyncio.sleep(5.0)
            yield "never"

    nt._providers[nid] = _ForeverProvider()
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            r = (
                await seeded_kernel.send(
                    [
                        {
                            "target": nid,
                            "payload": {
                                "type": "send",
                                "text": "slow",
                                "client_id": "alice",
                            },
                        }
                    ]
                )
            )[0]["result"]
        assert "timeout" in r.get("error", "")
        done_status = [
            p
            for _, p in emits
            if p.get("type") == "status" and p.get("phase") == "done"
        ]
        assert done_status[-1]["detail"]["reason"] == "timeout"
    finally:
        nt._providers.pop(nid, None)


async def test_status_event_includes_send_id(seeded_kernel, store_agent):
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")
    fp = _FakeProvider([["ok"]])
    nt._providers[nid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            (
                await seeded_kernel.send(
                    [
                        {
                            "target": nid,
                            "payload": {
                                "type": "send",
                                "text": "hi",
                                "client_id": "alice",
                            },
                        }
                    ]
                )
            )[0]["result"]
        statuses = [p for _, p in emits if p.get("type") == "status"]
        for s in statuses:
            assert "send_id" in s["detail"], s
    finally:
        nt._providers.pop(nid, None)


async def test_status_thinking_during_429_wait(seeded_kernel, store_agent):
    """The 429 retry path emits status(thinking, waiting_on='rate_limit')."""
    nid = await _make_nvidia(seeded_kernel, store_agent, with_key="nvapi-x")

    class _RLThenOk:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None, max_tokens=None):
            self.calls += 1
            if self.calls == 1:
                req = httpx.Request("POST", "https://x")
                resp = httpx.Response(429, headers={"retry-after": "0"}, request=req)
                raise httpx.HTTPStatusError("rl", request=req, response=resp)
            yield 'ok <call name="done"></call>'

    nt._providers[nid] = _RLThenOk()
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            (
                await seeded_kernel.send(
                    [
                        {
                            "target": nid,
                            "payload": {
                                "type": "send",
                                "text": "hi",
                                "client_id": "alice",
                            },
                        }
                    ]
                )
            )[0]["result"]
        rate_limit_status = [
            p
            for _, p in emits
            if p.get("type") == "status"
            and p.get("phase") == "thinking"
            and p.get("detail", {}).get("waiting_on") == "rate_limit"
        ]
        assert len(rate_limit_status) == 1, rate_limit_status
        assert "wait_s" in rate_limit_status[0]["detail"]
    finally:
        nt._providers.pop(nid, None)
