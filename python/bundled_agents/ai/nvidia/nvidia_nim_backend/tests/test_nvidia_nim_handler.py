"""nvidia_nim_backend handler — verb dispatching, file_agent_id + api_key
failfast, multi-step _run, key sidecar via file_agent_id, rate-limit retry.

Mirror of ollama_backend's test_handler.py with the provider stubbed
out and the new api_key surface (set_api_key, clear_api_key,
has_api_key flag in reflect) plus 429-retry behavior covered."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import httpx

import nvidia_nim_backend.tools as nt


class _FakeProvider:
    """Provider stub that yields a scripted stream from .chat()."""

    def __init__(self, scripts: list[list]):
        self._scripts = list(scripts)
        self.calls = 0

    async def chat(self, messages, tools=None):
        items = self._scripts.pop(0) if self._scripts else []
        self.calls += 1
        for x in items:
            yield x


async def _make_nvidia(kernel, file_agent_id=None, with_key: str | None = None):
    meta = {"handler_module": "nvidia_nim_backend.tools"}
    if file_agent_id is not None:
        meta["file_agent_id"] = file_agent_id
    rec = await kernel.send("core", {"type": "create_agent", **meta})
    nid = rec["id"]
    if with_key and file_agent_id is not None:
        await kernel.send(
            file_agent_id,
            {
                "type": "write",
                "path": f".fantastic/agents/{nid}/api_key",
                "content": with_key,
            },
        )
    return nid


# ─── reflect / failfast ────────────────────────────────────────


async def test_reflect_includes_file_agent_id_and_no_history(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent)
    r = await seeded_kernel.send(nid, {"type": "reflect"})
    assert r["file_agent_id"] == file_agent
    assert "send" in r["verbs"]
    assert "set_api_key" in r["verbs"]
    assert "clear_api_key" in r["verbs"]


async def test_reflect_has_api_key_flips_after_set(seeded_kernel, file_agent):
    """has_api_key reports the boolean — never the key value itself."""
    nid = await _make_nvidia(seeded_kernel, file_agent)
    r0 = await seeded_kernel.send(nid, {"type": "reflect"})
    assert r0["has_api_key"] is False
    blob0 = json.dumps(r0)
    assert "nvapi-" not in blob0

    await seeded_kernel.send(
        nid, {"type": "set_api_key", "api_key": "nvapi-secret-xxxxxxxxxxxx"}
    )
    r1 = await seeded_kernel.send(nid, {"type": "reflect"})
    assert r1["has_api_key"] is True
    blob1 = json.dumps(r1)
    # Reflect must never leak the key value.
    assert "nvapi-secret" not in blob1


async def test_send_requires_file_agent_id(seeded_kernel):
    nid = await _make_nvidia(seeded_kernel)
    r = await seeded_kernel.send(nid, {"type": "send", "text": "hi"})
    assert "error" in r
    assert "file_agent_id" in r["error"]


async def test_send_requires_api_key(seeded_kernel, file_agent):
    """No api_key sidecar → send failfasts before touching the network."""
    nid = await _make_nvidia(seeded_kernel, file_agent)
    r = await seeded_kernel.send(nid, {"type": "send", "text": "hi"})
    assert "error" in r
    assert "api_key" in r["error"]


async def test_history_requires_file_agent_id(seeded_kernel):
    nid = await _make_nvidia(seeded_kernel)
    r = await seeded_kernel.send(nid, {"type": "history"})
    assert "error" in r


async def test_unknown_verb_errors(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent)
    r = await seeded_kernel.send(nid, {"type": "garbage"})
    assert "error" in r


# ─── api_key verbs ─────────────────────────────────────────────


async def test_set_api_key_writes_via_file_agent(seeded_kernel, file_agent, tmp_path):
    nid = await _make_nvidia(seeded_kernel, file_agent)
    r = await seeded_kernel.send(nid, {"type": "set_api_key", "api_key": "nvapi-abc"})
    assert r == {"ok": True}
    key_path = tmp_path / ".fantastic" / "agents" / nid / "api_key"
    assert key_path.exists()
    assert key_path.read_text().strip() == "nvapi-abc"


async def test_set_api_key_strips_whitespace(seeded_kernel, file_agent, tmp_path):
    nid = await _make_nvidia(seeded_kernel, file_agent)
    await seeded_kernel.send(
        nid, {"type": "set_api_key", "api_key": "   nvapi-padded  \n"}
    )
    key_path = tmp_path / ".fantastic" / "agents" / nid / "api_key"
    assert key_path.read_text() == "nvapi-padded"


async def test_set_api_key_invalidates_cached_provider(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-old")
    sentinel = object()
    nt._providers[nid] = sentinel
    try:
        await seeded_kernel.send(nid, {"type": "set_api_key", "api_key": "nvapi-new"})
        assert nt._providers.get(nid) is not sentinel
    finally:
        nt._providers.pop(nid, None)


async def test_set_api_key_failfast_without_file_agent_id(seeded_kernel):
    nid = await _make_nvidia(seeded_kernel)
    r = await seeded_kernel.send(nid, {"type": "set_api_key", "api_key": "nvapi-x"})
    assert "error" in r
    assert "file_agent_id" in r["error"]


async def test_set_api_key_rejects_empty(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent)
    for bad in ("", "   ", None, 42):
        r = await seeded_kernel.send(nid, {"type": "set_api_key", "api_key": bad})
        assert "error" in r, f"unexpected ok for api_key={bad!r}"


async def test_clear_api_key_deletes_file(seeded_kernel, file_agent, tmp_path):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    key_path = tmp_path / ".fantastic" / "agents" / nid / "api_key"
    assert key_path.exists()
    r = await seeded_kernel.send(nid, {"type": "clear_api_key"})
    assert r["ok"] is True
    assert not key_path.exists()
    # Now reflect reports no key, send refuses.
    r2 = await seeded_kernel.send(nid, {"type": "reflect"})
    assert r2["has_api_key"] is False
    r3 = await seeded_kernel.send(nid, {"type": "send", "text": "hi"})
    assert "api_key" in r3.get("error", "")


async def test_clear_api_key_failfast_without_file_agent_id(seeded_kernel):
    nid = await _make_nvidia(seeded_kernel)
    r = await seeded_kernel.send(nid, {"type": "clear_api_key"})
    assert "error" in r


# ─── history persistence (per-client) ─────────────────────────


async def test_history_returns_messages(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent)
    chat = json.dumps(
        [{"role": "user", "content": "hi"}, {"role": "assistant", "content": "hello"}]
    )
    await seeded_kernel.send(
        file_agent,
        {
            "type": "write",
            "path": f".fantastic/agents/{nid}/chat_cli.json",
            "content": chat,
        },
    )
    r = await seeded_kernel.send(nid, {"type": "history"})
    assert len(r["messages"]) == 2
    assert r["client_id"] == "cli"


async def test_history_per_client(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent)
    for cid, last in (("alice", "A-reply"), ("bob", "B-reply")):
        chat = json.dumps(
            [
                {"role": "user", "content": f"hi from {cid}"},
                {"role": "assistant", "content": last},
            ]
        )
        await seeded_kernel.send(
            file_agent,
            {
                "type": "write",
                "path": f".fantastic/agents/{nid}/chat_{cid}.json",
                "content": chat,
            },
        )
    a = await seeded_kernel.send(nid, {"type": "history", "client_id": "alice"})
    b = await seeded_kernel.send(nid, {"type": "history", "client_id": "bob"})
    assert a["messages"][-1]["content"] == "A-reply"
    assert b["messages"][-1]["content"] == "B-reply"


# ─── _run multi-step / persistence ────────────────────────────


async def test_run_no_tool_calls_returns_content(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    fp = _FakeProvider([["Hello! ", "How are you?"]])
    nt._providers[nid] = fp
    try:
        r = await seeded_kernel.send(nid, {"type": "send", "text": "hi"})
        assert r["final"] == "Hello! How are you?"
        assert fp.calls == 1
    finally:
        nt._providers.pop(nid, None)


async def test_run_with_tool_call_iterates(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    fp = _FakeProvider(
        [
            [
                {
                    "tool_call": {
                        "id": "call_a",
                        "name": "send",
                        "arguments": {
                            "target_id": "core",
                            "payload": {"type": "list_agents"},
                        },
                    }
                }
            ],
            ["Final answer based on tool reply."],
        ]
    )
    nt._providers[nid] = fp
    try:
        r = await seeded_kernel.send(nid, {"type": "send", "text": "list"})
        assert r["final"] == "Final answer based on tool reply."
        assert fp.calls == 2
    finally:
        nt._providers.pop(nid, None)


async def test_run_persists_history_via_file_agent(seeded_kernel, file_agent, tmp_path):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    fp = _FakeProvider([["reply"]])
    nt._providers[nid] = fp
    try:
        await seeded_kernel.send(nid, {"type": "send", "text": "hi"})
        path = tmp_path / ".fantastic" / "agents" / nid / "chat_cli.json"
        assert path.exists()
        data = json.loads(path.read_text())
        assert data[-2] == {"role": "user", "content": "hi"}
        assert data[-1] == {"role": "assistant", "content": "reply"}
    finally:
        nt._providers.pop(nid, None)


async def test_run_persists_per_client_threads(seeded_kernel, file_agent, tmp_path):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    fp = _FakeProvider([["A-reply"], ["B-reply"]])
    nt._providers[nid] = fp
    try:
        await seeded_kernel.send(
            nid, {"type": "send", "text": "hi A", "client_id": "alice"}
        )
        await seeded_kernel.send(
            nid, {"type": "send", "text": "hi B", "client_id": "bob"}
        )
        path_a = tmp_path / ".fantastic" / "agents" / nid / "chat_alice.json"
        path_b = tmp_path / ".fantastic" / "agents" / nid / "chat_bob.json"
        assert path_a.exists() and path_b.exists()
        a = json.loads(path_a.read_text())
        b = json.loads(path_b.read_text())
        assert a[-2]["content"] == "hi A" and a[-1]["content"] == "A-reply"
        assert b[-2]["content"] == "hi B" and b[-1]["content"] == "B-reply"
    finally:
        nt._providers.pop(nid, None)


async def test_run_unbounded_steps_until_no_tool_calls(seeded_kernel, file_agent):
    """No fixed step cap. Same Claude-Code-style loop as ollama_backend."""
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    scripts = [
        [
            {
                "tool_call": {
                    "id": f"call_{i}",
                    "name": "send",
                    "arguments": {
                        "target_id": "core",
                        "payload": {"type": "list_agents"},
                    },
                }
            }
        ]
        for i in range(6)
    ]
    scripts.append(["finally done."])
    fp = _FakeProvider(scripts)
    nt._providers[nid] = fp
    try:
        r = await seeded_kernel.send(nid, {"type": "send", "text": "loop"})
        assert r["final"] == "finally done."
        assert fp.calls == 7
    finally:
        nt._providers.pop(nid, None)


# ─── routing ───────────────────────────────────────────────────


async def test_cli_caller_routes_to_cli_only(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    fp = _FakeProvider([["chunk1 ", "chunk2"]])
    nt._providers[nid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            await seeded_kernel.send(nid, {"type": "send", "text": "hi"})

        cli_tokens = [p for t, p in sends if t == "cli" and p.get("type") == "token"]
        cli_done = [p for t, p in sends if t == "cli" and p.get("type") == "done"]
        assert len(cli_tokens) == 2
        assert len(cli_done) == 1
        own_stream = [
            p
            for t, p in emits
            if t == nid and p.get("type") in ("token", "say", "done")
        ]
        assert own_stream == []
        assert all(p.get("client_id") == "cli" for p in cli_tokens + cli_done)
    finally:
        nt._providers.pop(nid, None)


async def test_browser_caller_routes_to_own_inbox_only(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    fp = _FakeProvider([["a", "b"]])
    nt._providers[nid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            await seeded_kernel.send(
                nid, {"type": "send", "text": "hi", "client_id": "web_xyz"}
            )

        own_tokens = [p for t, p in emits if t == nid and p.get("type") == "token"]
        own_done = [p for t, p in emits if t == nid and p.get("type") == "done"]
        assert len(own_tokens) == 2
        assert len(own_done) == 1
        assert all(p["client_id"] == "web_xyz" for p in own_tokens + own_done)
        cli_stream = [
            p
            for t, p in sends
            if t == "cli" and p.get("type") in ("token", "say", "done")
        ]
        assert cli_stream == []
    finally:
        nt._providers.pop(nid, None)


# ─── menu ───────────────────────────────────────────────────────


async def test_assemble_includes_agent_menu(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent)
    nt._invalidate_menu(nid)
    msgs = await nt._assemble(nid, "hello", seeded_kernel, "alice")
    sys_block = msgs[0]["content"]
    assert "Available agents" in sys_block
    assert file_agent in sys_block
    assert "read" in sys_block
    assert "send tool" in sys_block.lower()
    assert "refresh_menu" in sys_block
    assert nid in nt._menu_cache


async def test_menu_invalidates_after_tool_call(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    fp = _FakeProvider(
        [
            [
                {
                    "tool_call": {
                        "id": "call_a",
                        "name": "send",
                        "arguments": {
                            "target_id": "core",
                            "payload": {"type": "list_agents"},
                        },
                    }
                }
            ],
            ["done."],
        ]
    )
    nt._providers[nid] = fp
    try:
        nt._menu_cache[nid] = [{"id": "stale", "sentence": "", "verbs": []}]
        await seeded_kernel.send(
            nid, {"type": "send", "text": "list", "client_id": "alice"}
        )
        assert nid not in nt._menu_cache
    finally:
        nt._providers.pop(nid, None)


async def test_refresh_menu_verb_invalidates(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent)
    nt._menu_cache[nid] = [{"id": "stale", "sentence": "", "verbs": []}]
    r = await seeded_kernel.send(nid, {"type": "refresh_menu"})
    assert r == {"refreshed": True}
    assert nid not in nt._menu_cache


# ─── concurrency ────────────────────────────────────────────────


async def test_contended_send_emits_queued_event(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")

    class _SlowFirst:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None):
            self.calls += 1
            if self.calls == 1:
                await asyncio.sleep(0.20)
            yield "ok"

    nt._providers[nid] = _SlowFirst()

    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            t1 = asyncio.create_task(
                seeded_kernel.send(
                    nid, {"type": "send", "text": "first", "client_id": "alice"}
                )
            )
            await asyncio.sleep(0.05)
            t2 = asyncio.create_task(
                seeded_kernel.send(
                    nid, {"type": "send", "text": "second", "client_id": "bob"}
                )
            )
            r1, r2 = await asyncio.gather(t1, t2)

        assert r1["final"] == "ok" and r2["final"] == "ok"

        bob_queued = [
            p
            for t, p in emits
            if t == nid and p.get("type") == "queued" and p.get("client_id") == "bob"
        ]
        assert len(bob_queued) == 1
        all_pairs = [("send", t, p) for t, p in sends] + [
            ("emit", t, p) for t, p in emits
        ]
        alice_queued = [
            p
            for kind, t, p in all_pairs
            if p.get("type") == "queued" and p.get("client_id") == "alice"
        ]
        assert alice_queued == []
    finally:
        nt._providers.pop(nid, None)


async def test_concurrent_sends_serialize_per_backend(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")

    in_flight = 0
    max_in_flight = 0
    order: list[str] = []

    class _SerialProvider:
        async def chat(self, messages, tools=None):
            nonlocal in_flight, max_in_flight
            in_flight += 1
            max_in_flight = max(max_in_flight, in_flight)
            try:
                await asyncio.sleep(0.05)
                user = messages[-1]["content"]
                order.append(user)
                yield "ok"
            finally:
                in_flight -= 1

    nt._providers[nid] = _SerialProvider()
    try:
        results = await asyncio.gather(
            seeded_kernel.send(
                nid, {"type": "send", "text": "first", "client_id": "a"}
            ),
            seeded_kernel.send(
                nid, {"type": "send", "text": "second", "client_id": "b"}
            ),
        )
        assert max_in_flight == 1
        assert order == ["first", "second"]
        assert all(r["final"] == "ok" for r in results)
    finally:
        nt._providers.pop(nid, None)


# ─── tool_call argument shape ─────────────────────────────────


async def test_assistant_tool_calls_serialize_arguments_to_json_string(
    seeded_kernel, file_agent
):
    """OpenAI-flavored backends require tool_call.function.arguments as
    a JSON string. We hand-roll the assistant message inside _run, so
    verify the serialization happens correctly when the second iteration
    inspects message history."""
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    seen_messages: list[list[dict]] = []

    class _CapturingProvider:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None):
            self.calls += 1
            seen_messages.append([dict(m) for m in messages])
            if self.calls == 1:
                yield {
                    "tool_call": {
                        "id": "call_a",
                        "name": "send",
                        "arguments": {
                            "target_id": "core",
                            "payload": {"type": "list_agents"},
                        },
                    }
                }
            else:
                yield "done"

    nt._providers[nid] = _CapturingProvider()
    try:
        await seeded_kernel.send(nid, {"type": "send", "text": "go"})
    finally:
        nt._providers.pop(nid, None)

    # Second call must have seen the assistant turn with the previously
    # emitted tool_call. arguments should be a JSON-encoded string.
    second_history = seen_messages[1]
    assistant_turns = [m for m in second_history if m.get("role") == "assistant"]
    assert assistant_turns, "no assistant turn re-played to second iteration"
    tcs = assistant_turns[-1].get("tool_calls") or []
    assert tcs, "tool_calls missing from re-played assistant turn"
    args = tcs[0]["function"]["arguments"]
    assert isinstance(args, str), f"OpenAI-shape requires string args, got {type(args)}"
    decoded = json.loads(args)
    assert decoded == {"target_id": "core", "payload": {"type": "list_agents"}}


# ─── rate-limit retry ──────────────────────────────────────────


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


async def test_send_retries_once_on_rate_limit(seeded_kernel, file_agent):
    """Provider raises 429 on first chat() call (before any chunk yielded);
    backend sleeps Retry-After, emits a `say` event, retries, succeeds."""
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")

    class _RateLimitedThenOk:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None):
            self.calls += 1
            if self.calls == 1:
                raise _http_status_error(429, retry_after="0")
            yield "ok"

    fp = _RateLimitedThenOk()
    nt._providers[nid] = fp

    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            r = await seeded_kernel.send(nid, {"type": "send", "text": "hi"})
        assert r["final"] == "ok"
        assert fp.calls == 2  # initial + retry
        # `say` event surfaced the rate-limit wait to the caller (cli).
        cli_says = [p for t, p in sends if t == "cli" and p.get("type") == "say"]
        rate_says = [p for p in cli_says if "rate limited" in p.get("text", "")]
        assert len(rate_says) == 1, f"expected one rate-limit say, got {cli_says}"
    finally:
        nt._providers.pop(nid, None)


async def test_send_surfaces_error_after_repeated_rate_limit(seeded_kernel, file_agent):
    """Two 429s in a row → retry exhausted → `_send` returns a clean error."""
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")

    class _AlwaysRateLimited:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None):
            self.calls += 1
            raise _http_status_error(429, retry_after="0")
            yield  # unreachable; makes this an async generator

    fp = _AlwaysRateLimited()
    nt._providers[nid] = fp
    try:
        r = await seeded_kernel.send(nid, {"type": "send", "text": "hi"})
    finally:
        nt._providers.pop(nid, None)

    assert "error" in r
    assert "rate limited" in r["error"].lower()
    assert "429" in r["error"]
    # 1 initial + 1 retry = 2; we don't retry indefinitely.
    assert fp.calls == 2


async def test_send_does_not_retry_on_non_429_http_error(seeded_kernel, file_agent):
    """500/503/auth errors are NOT retried — surfaced once, cleanly."""
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")

    class _ServerError:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None):
            self.calls += 1
            raise _http_status_error(503)
            yield

    fp = _ServerError()
    nt._providers[nid] = fp
    try:
        r = await seeded_kernel.send(nid, {"type": "send", "text": "hi"})
    finally:
        nt._providers.pop(nid, None)

    assert "error" in r
    assert "503" in r["error"]
    assert fp.calls == 1  # no retry on 5xx


async def test_mid_stream_429_propagates_without_retry(seeded_kernel, file_agent):
    """If a 429 fires AFTER chunks were yielded (rare; quota usually
    checked up front), retrying would duplicate tokens — so we don't.
    The error surfaces; partial tokens already streamed are fine."""
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")

    class _MidStream429:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None):
            self.calls += 1
            yield "partial "
            raise _http_status_error(429, retry_after="0")

    fp = _MidStream429()
    nt._providers[nid] = fp
    try:
        r = await seeded_kernel.send(nid, {"type": "send", "text": "hi"})
    finally:
        nt._providers.pop(nid, None)

    assert "error" in r
    assert "429" in r["error"]
    assert fp.calls == 1  # mid-stream → no retry


# ─── housekeeping: assert key file path matches the design ────


def test_key_path_lives_under_agent_dir():
    p = nt._key_path("nvidia_xxx")
    assert Path(p).parts[-2:] == ("nvidia_xxx", "api_key")
    assert ".fantastic/agents" in p


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


async def test_status_event_sequence_no_tool_calls(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    fp = _FakeProvider([["hello"]])
    nt._providers[nid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            await seeded_kernel.send(
                nid, {"type": "send", "text": "hi", "client_id": "alice"}
            )
        statuses = [p for _, p in emits if p.get("type") == "status"]
        phases = [s["phase"] for s in statuses]
        assert phases == ["thinking", "streaming", "done"], phases
        assert statuses[-1]["detail"]["reason"] == "ok"
        send_ids = {s["detail"].get("send_id") for s in statuses}
        assert len(send_ids) == 1 and None not in send_ids
    finally:
        nt._providers.pop(nid, None)


async def test_status_event_sequence_with_tool_call(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    fp = _FakeProvider(
        [
            [
                {
                    "tool_call": {
                        "id": "call_a",
                        "name": "send",
                        "arguments": {
                            "target_id": "core",
                            "payload": {"type": "list_agents"},
                        },
                    }
                }
            ],
            ["wrap up."],
        ]
    )
    nt._providers[nid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            await seeded_kernel.send(
                nid, {"type": "send", "text": "list", "client_id": "alice"}
            )
        statuses = [p for _, p in emits if p.get("type") == "status"]
        phases = [s["phase"] for s in statuses]
        assert phases == [
            "thinking",
            "tool_calling",
            "tool_calling",
            "thinking",
            "streaming",
            "done",
        ], phases
        tcs = [s for s in statuses if s["phase"] == "tool_calling"]
        assert "reply_preview" not in tcs[0]["detail"]["tool"]
        assert "reply_preview" in tcs[1]["detail"]["tool"]
        assert (
            tcs[0]["detail"]["tool"]["call_id"]
            == tcs[1]["detail"]["tool"]["call_id"]
            == "call_a"
        )
    finally:
        nt._providers.pop(nid, None)


async def test_queue_populated_and_drained_on_contention(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    queue_sizes: list[int] = []

    class _SlowFirst:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None):
            self.calls += 1
            if self.calls == 1:
                await asyncio.sleep(0.10)
                queue_sizes.append(len(nt._queue.get(nid, [])))
                await asyncio.sleep(0.10)
            yield "ok"

    nt._providers[nid] = _SlowFirst()
    try:
        t1 = asyncio.create_task(
            seeded_kernel.send(
                nid, {"type": "send", "text": "first", "client_id": "alice"}
            )
        )
        await asyncio.sleep(0.05)
        t2 = asyncio.create_task(
            seeded_kernel.send(
                nid, {"type": "send", "text": "second", "client_id": "bob"}
            )
        )
        await asyncio.gather(t1, t2)
    finally:
        nt._providers.pop(nid, None)

    assert queue_sizes == [1], queue_sizes
    assert nt._queue.get(nid, []) == []
    assert nid not in nt._current


async def test_status_verb_shape_when_idle(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent)
    r = await seeded_kernel.send(nid, {"type": "status", "client_id": "alice"})
    assert r["generating"] is False
    assert r["current"] is None
    assert r["mine_pending"] == []
    assert r["others_pending"] == 0
    assert r["client_id"] == "alice"


async def test_status_verb_shape_during_inflight_mine(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    captured: list[dict] = []

    class _SlowProvider:
        async def chat(self, messages, tools=None):
            captured.append(
                await seeded_kernel.send(nid, {"type": "status", "client_id": "alice"})
            )
            yield "ok"

    nt._providers[nid] = _SlowProvider()
    try:
        await seeded_kernel.send(
            nid, {"type": "send", "text": "hello there", "client_id": "alice"}
        )
    finally:
        nt._providers.pop(nid, None)

    snap = captured[0]
    assert snap["generating"] is True
    assert snap["current"]["is_mine"] is True
    assert snap["current"]["phase"] in ("thinking", "streaming")
    assert snap["current"]["text"] == "hello there"


async def test_status_verb_privacy_filter(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    snapshots: dict[str, dict] = {}

    class _PrivacyProvider:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None):
            self.calls += 1
            if self.calls == 1:
                await asyncio.sleep(0.15)
                snapshots["alice"] = await seeded_kernel.send(
                    nid, {"type": "status", "client_id": "alice"}
                )
                snapshots["bob"] = await seeded_kernel.send(
                    nid, {"type": "status", "client_id": "bob"}
                )
            yield "ok"

    nt._providers[nid] = _PrivacyProvider()
    try:
        t1 = asyncio.create_task(
            seeded_kernel.send(
                nid, {"type": "send", "text": "bob secret", "client_id": "bob"}
            )
        )
        await asyncio.sleep(0.05)
        t2 = asyncio.create_task(
            seeded_kernel.send(
                nid, {"type": "send", "text": "alice secret", "client_id": "alice"}
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


async def test_status_verb_no_client_id_redacts_text(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    holder: list[dict] = []

    class _RedactProvider:
        async def chat(self, messages, tools=None):
            holder.append(await seeded_kernel.send(nid, {"type": "status"}))
            yield "ok"

    nt._providers[nid] = _RedactProvider()
    try:
        await seeded_kernel.send(
            nid, {"type": "send", "text": "private", "client_id": "alice"}
        )
    finally:
        nt._providers.pop(nid, None)

    snap = holder[0]
    assert snap["client_id"] is None
    assert snap["current"] is not None
    assert "text" not in snap["current"]
    assert "text_so_far" not in snap["current"]


async def test_status_done_emits_with_reason_interrupt(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")

    class _SlowProvider:
        async def chat(self, messages, tools=None):
            await asyncio.sleep(2.0)
            yield "never"

    nt._providers[nid] = _SlowProvider()
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            send_task = asyncio.create_task(
                seeded_kernel.send(
                    nid, {"type": "send", "text": "stop me", "client_id": "alice"}
                )
            )
            await asyncio.sleep(0.10)
            await seeded_kernel.send(nid, {"type": "interrupt"})
            await send_task
        done_status = [
            p
            for _, p in emits
            if p.get("type") == "status" and p.get("phase") == "done"
        ]
        assert len(done_status) == 1
        assert done_status[0]["detail"]["reason"] == "interrupted"
    finally:
        nt._providers.pop(nid, None)


async def test_status_done_emits_with_reason_timeout(
    seeded_kernel, file_agent, monkeypatch
):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    monkeypatch.setattr(nt, "SEND_TIMEOUT", 0.05)

    class _ForeverProvider:
        async def chat(self, messages, tools=None):
            await asyncio.sleep(5.0)
            yield "never"

    nt._providers[nid] = _ForeverProvider()
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            r = await seeded_kernel.send(
                nid, {"type": "send", "text": "slow", "client_id": "alice"}
            )
        assert "timeout" in r.get("error", "")
        done_status = [
            p
            for _, p in emits
            if p.get("type") == "status" and p.get("phase") == "done"
        ]
        assert done_status[-1]["detail"]["reason"] == "timeout"
    finally:
        nt._providers.pop(nid, None)


async def test_status_event_includes_send_id(seeded_kernel, file_agent):
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")
    fp = _FakeProvider([["ok"]])
    nt._providers[nid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            await seeded_kernel.send(
                nid, {"type": "send", "text": "hi", "client_id": "alice"}
            )
        statuses = [p for _, p in emits if p.get("type") == "status"]
        for s in statuses:
            assert "send_id" in s["detail"], s
    finally:
        nt._providers.pop(nid, None)


async def test_status_thinking_during_429_wait(seeded_kernel, file_agent):
    """The 429 retry path emits status(thinking, waiting_on='rate_limit')
    in addition to the back-compat `say` notice."""
    nid = await _make_nvidia(seeded_kernel, file_agent, with_key="nvapi-x")

    class _RLThenOk:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools=None):
            self.calls += 1
            if self.calls == 1:
                req = httpx.Request("POST", "https://x")
                resp = httpx.Response(429, headers={"retry-after": "0"}, request=req)
                raise httpx.HTTPStatusError("rl", request=req, response=resp)
            yield "ok"

    nt._providers[nid] = _RLThenOk()
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            await seeded_kernel.send(
                nid, {"type": "send", "text": "hi", "client_id": "alice"}
            )
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
