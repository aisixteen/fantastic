"""ollama_backend handler — verb dispatching, file_agent_id failfast, multi-step _run."""

from __future__ import annotations

import asyncio
import json
from unittest.mock import patch


import ollama_backend.tools as ot


async def _async_iter(items):
    for it in items:
        yield it


class _FakeProvider:
    """Provider stub that yields a scripted stream from .chat()."""

    def __init__(self, scripts: list[list]):
        # `scripts` is a list of stream-iters, one per `chat()` call (multi-step support).
        self._scripts = list(scripts)
        self.calls = 0

    async def chat(self, messages, tools):
        items = self._scripts.pop(0) if self._scripts else []
        self.calls += 1
        for x in items:
            yield x


async def _make_ollama(kernel, file_agent_id=None):
    meta = {"handler_module": "ollama_backend.tools"}
    if file_agent_id is not None:
        meta["file_agent_id"] = file_agent_id
    rec = await kernel.send("core", {"type": "create_agent", **meta})
    return rec["id"]


async def test_reflect_includes_file_agent_id_and_no_history(seeded_kernel, file_agent):
    oid = await _make_ollama(seeded_kernel, file_agent)
    r = await seeded_kernel.send(oid, {"type": "reflect"})
    assert r["file_agent_id"] == file_agent
    assert "send" in r["verbs"]


async def test_send_requires_file_agent_id(seeded_kernel):
    oid = await _make_ollama(seeded_kernel)
    r = await seeded_kernel.send(oid, {"type": "send", "text": "hi"})
    assert "error" in r
    assert "file_agent_id" in r["error"]


async def test_history_requires_file_agent_id(seeded_kernel):
    oid = await _make_ollama(seeded_kernel)
    r = await seeded_kernel.send(oid, {"type": "history"})
    assert "error" in r


async def test_history_returns_messages(seeded_kernel, file_agent, tmp_path):
    oid = await _make_ollama(seeded_kernel, file_agent)
    # Pre-seed the default 'cli' client thread.
    chat = json.dumps(
        [{"role": "user", "content": "hi"}, {"role": "assistant", "content": "hello"}]
    )
    await seeded_kernel.send(
        file_agent,
        {
            "type": "write",
            "path": f".fantastic/agents/{oid}/chat_cli.json",
            "content": chat,
        },
    )
    r = await seeded_kernel.send(oid, {"type": "history"})
    assert len(r["messages"]) == 2
    assert r["client_id"] == "cli"


async def test_history_per_client(seeded_kernel, file_agent, tmp_path):
    """Two clients = two separate threads; history reads only the asked-for one."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    # Seed two distinct chats.
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
                "path": f".fantastic/agents/{oid}/chat_{cid}.json",
                "content": chat,
            },
        )
    a = await seeded_kernel.send(oid, {"type": "history", "client_id": "alice"})
    b = await seeded_kernel.send(oid, {"type": "history", "client_id": "bob"})
    assert a["messages"][-1]["content"] == "A-reply"
    assert b["messages"][-1]["content"] == "B-reply"
    assert a["client_id"] == "alice" and b["client_id"] == "bob"


async def test_run_no_tool_calls_returns_content(seeded_kernel, file_agent):
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider([["Hello! ", "How are you?"]])
    ot._providers[oid] = fp
    try:
        r = await seeded_kernel.send(oid, {"type": "send", "text": "hi"})
        assert r["final"] == "Hello! How are you?"
        assert fp.calls == 1
    finally:
        ot._providers.pop(oid, None)


async def test_run_with_tool_call_iterates(seeded_kernel, file_agent):
    """Model emits a tool_call to core, gets reply, second iteration finishes."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider(
        [
            # iter 1: emit tool_call
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
            # iter 2: text only, loop ends
            ["Final answer based on tool reply."],
        ]
    )
    ot._providers[oid] = fp
    try:
        r = await seeded_kernel.send(oid, {"type": "send", "text": "list"})
        assert r["final"] == "Final answer based on tool reply."
        assert fp.calls == 2
    finally:
        ot._providers.pop(oid, None)


async def test_run_persists_history_via_file_agent(seeded_kernel, file_agent, tmp_path):
    """Default caller (no client_id) persists to chat_cli.json."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider([["reply"]])
    ot._providers[oid] = fp
    try:
        await seeded_kernel.send(oid, {"type": "send", "text": "hi"})
        path = tmp_path / ".fantastic" / "agents" / oid / "chat_cli.json"
        assert path.exists()
        data = json.loads(path.read_text())
        assert data[-2] == {"role": "user", "content": "hi"}
        assert data[-1] == {"role": "assistant", "content": "reply"}
        # System prompt must NOT be persisted — it's rebuilt fresh
        # each turn from the live agent menu + howto.
        assert all(m.get("role") != "system" for m in data)
    finally:
        ot._providers.pop(oid, None)


async def test_run_persists_full_tool_call_round_trip(
    seeded_kernel, file_agent, tmp_path
):
    """Lossless persistence of tool turns. The chat sidecar must keep:
      1. The assistant turn that emitted tool_calls (with the tool_call
         entries intact — `function.name` + `arguments`).
      2. The role:tool reply linked by tool_call_id.
      3. The final assistant turn that closed the loop.

    This is the audit trail for malformed Gemma tool-calls
    (chat-template-token leaks like `<|"|verb<|"|`, hallucinated verbs,
    arguments-as-string vs dict). Without it, faulty calls evaporate
    after the turn and we can't diagnose them after the fact.
    """
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider(
        [
            # Iter 1: tool_call to core.list_agents
            [
                {
                    "tool_call": {
                        "id": "call_X",
                        "name": "send",
                        "arguments": {
                            "target_id": "core",
                            "payload": {"type": "list_agents"},
                        },
                    }
                }
            ],
            # Iter 2: final text, loop ends
            ["Done."],
        ]
    )
    ot._providers[oid] = fp
    try:
        await seeded_kernel.send(oid, {"type": "send", "text": "list em"})
        path = tmp_path / ".fantastic" / "agents" / oid / "chat_cli.json"
        data = json.loads(path.read_text())
        roles = [m["role"] for m in data]
        # Expected sequence: user → asst-w-tcs → tool → asst-final
        assert roles == ["user", "assistant", "tool", "assistant"], (
            f"roles malformed: {roles}\nfull: {data}"
        )
        # 1. assistant turn with tool_calls preserved
        asst_tcs = data[1]
        assert asst_tcs["tool_calls"], "tool_calls dropped from persistence"
        tc = asst_tcs["tool_calls"][0]
        assert tc["function"]["name"] == "send"
        assert tc["function"]["arguments"]["target_id"] == "core"
        # 2. tool reply linked by tool_call_id
        assert data[2]["tool_call_id"] == "call_X"
        assert data[2]["name"] == "send"
        # 3. final assistant turn captured
        assert data[3] == {"role": "assistant", "content": "Done."}
    finally:
        ot._providers.pop(oid, None)


async def test_run_persists_per_client_threads(seeded_kernel, file_agent, tmp_path):
    """Two client_ids → two distinct chat files, each with its own turns."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider([["A-reply"], ["B-reply"]])
    ot._providers[oid] = fp
    try:
        await seeded_kernel.send(
            oid, {"type": "send", "text": "hi A", "client_id": "alice"}
        )
        await seeded_kernel.send(
            oid, {"type": "send", "text": "hi B", "client_id": "bob"}
        )
        path_a = tmp_path / ".fantastic" / "agents" / oid / "chat_alice.json"
        path_b = tmp_path / ".fantastic" / "agents" / oid / "chat_bob.json"
        assert path_a.exists() and path_b.exists()
        a = json.loads(path_a.read_text())
        b = json.loads(path_b.read_text())
        assert a[-2]["content"] == "hi A" and a[-1]["content"] == "A-reply"
        assert b[-2]["content"] == "hi B" and b[-1]["content"] == "B-reply"
    finally:
        ot._providers.pop(oid, None)


async def test_run_unbounded_steps_until_no_tool_calls(seeded_kernel, file_agent):
    """Old MAX_STEPS=5 cap is gone. Loop continues as long as the
    model emits tool_calls; safety bounds are SEND_TIMEOUT (180s wall)
    and the user-callable `interrupt` verb. Verify a 7-step chain
    (which old code would have truncated at step 5) completes cleanly."""
    oid = await _make_ollama(seeded_kernel, file_agent)
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
    ot._providers[oid] = fp
    try:
        r = await seeded_kernel.send(oid, {"type": "send", "text": "loop"})
        assert r["final"] == "finally done."
        assert fp.calls == 7  # old MAX_STEPS=5 would have truncated here
    finally:
        ot._providers.pop(oid, None)


async def test_unknown_verb_errors(seeded_kernel, file_agent):
    oid = await _make_ollama(seeded_kernel, file_agent)
    r = await seeded_kernel.send(oid, {"type": "garbage"})
    assert "error" in r


async def test_cli_caller_routes_to_cli_only(seeded_kernel, file_agent):
    """Default caller (client_id='cli') has events dispatched via
    kernel.send('cli', …) so cli's stdout handler runs. No leak to the
    backend's own inbox (which would broadcast to unrelated WS subscribers).
    """
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider([["chunk1 ", "chunk2"]])
    ot._providers[oid] = fp
    sends: list[tuple[str, dict]] = []
    emits: list[tuple[str, dict]] = []

    real_send, real_emit = seeded_kernel.send, seeded_kernel.emit

    async def spy_send(target, payload):
        sends.append((target, dict(payload)))
        return await real_send(target, payload)

    async def spy_emit(target, payload):
        emits.append((target, dict(payload)))
        return await real_emit(target, payload)

    try:
        with (
            patch.object(seeded_kernel, "send", spy_send),
            patch.object(seeded_kernel, "emit", spy_emit),
        ):
            await seeded_kernel.send(oid, {"type": "send", "text": "hi"})

        # cli received tokens via kernel.send (so its handler ran).
        cli_tokens = [p for t, p in sends if t == "cli" and p.get("type") == "token"]
        cli_done = [p for t, p in sends if t == "cli" and p.get("type") == "done"]
        assert len(cli_tokens) == 2
        assert len(cli_done) == 1
        # Backend's own inbox got NO stream events (cli routing only).
        own_stream = [
            p
            for t, p in emits
            if t == oid and p.get("type") in ("token", "say", "done")
        ]
        assert own_stream == [], f"unexpected leak to own inbox: {own_stream}"
        # Stream events carry client_id="cli".
        assert all(p.get("client_id") == "cli" for p in cli_tokens + cli_done)
    finally:
        ot._providers.pop(oid, None)


async def test_browser_caller_routes_to_own_inbox_only(seeded_kernel, file_agent):
    """Caller passes a non-cli client_id (e.g. browser uuid) → events
    emit to backend's own inbox tagged with client_id, NOT to cli.
    """
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider([["a", "b"]])
    ot._providers[oid] = fp
    sends: list[tuple[str, dict]] = []
    emits: list[tuple[str, dict]] = []

    real_send, real_emit = seeded_kernel.send, seeded_kernel.emit

    async def spy_send(target, payload):
        sends.append((target, dict(payload)))
        return await real_send(target, payload)

    async def spy_emit(target, payload):
        emits.append((target, dict(payload)))
        return await real_emit(target, payload)

    try:
        with (
            patch.object(seeded_kernel, "send", spy_send),
            patch.object(seeded_kernel, "emit", spy_emit),
        ):
            await seeded_kernel.send(
                oid, {"type": "send", "text": "hi", "client_id": "web_xyz"}
            )

        # Backend's own inbox got the stream, tagged with the web client_id.
        own_tokens = [p for t, p in emits if t == oid and p.get("type") == "token"]
        own_done = [p for t, p in emits if t == oid and p.get("type") == "done"]
        assert len(own_tokens) == 2
        assert len(own_done) == 1
        assert all(p["client_id"] == "web_xyz" for p in own_tokens + own_done)
        # cli got NO stream events (no leak across clients).
        cli_stream = [
            p
            for t, p in sends
            if t == "cli" and p.get("type") in ("token", "say", "done")
        ]
        assert cli_stream == [], f"cli leak: {cli_stream}"
    finally:
        ot._providers.pop(oid, None)


async def test_assemble_includes_agent_menu(seeded_kernel, file_agent):
    """System prompt carries a 'Available agents' menu built by reflecting
    on every running agent, plus a `send`-tool how-to. Each turn rebuilds
    the menu lazily; chat.json holds only user/assistant turns.
    """
    oid = await _make_ollama(seeded_kernel, file_agent)
    # Force a fresh menu build.
    ot._invalidate_menu(oid)
    msgs = await ot._assemble(oid, "hello", seeded_kernel, "alice")
    sys_block = msgs[0]["content"]
    assert "Available agents" in sys_block
    # File agent is in the menu (created by the file_agent fixture).
    assert file_agent in sys_block
    # Verbs are listed alongside ids.
    assert "read" in sys_block
    # Send-tool guidance present.
    assert "send tool" in sys_block.lower()
    assert "refresh_menu" in sys_block
    # Menu is now cached.
    assert oid in ot._menu_cache


async def test_menu_invalidates_after_tool_call(seeded_kernel, file_agent):
    """Successful tool_call clears the menu so the next assemble rebuilds it."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider(
        [
            # iter 1: emit a tool_call to core
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
            # iter 2: text only, loop ends
            ["done."],
        ]
    )
    ot._providers[oid] = fp
    try:
        # Prime the cache.
        ot._menu_cache[oid] = [{"id": "stale", "sentence": "", "verbs": []}]
        await seeded_kernel.send(
            oid, {"type": "send", "text": "list", "client_id": "alice"}
        )
        # After the run, cache must be invalidated (popped).
        assert oid not in ot._menu_cache
    finally:
        ot._providers.pop(oid, None)


async def test_refresh_menu_verb_invalidates(seeded_kernel, file_agent):
    """LLM can self-clear the menu via send(<self>, {type:'refresh_menu'})."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    ot._menu_cache[oid] = [{"id": "stale", "sentence": "", "verbs": []}]
    r = await seeded_kernel.send(oid, {"type": "refresh_menu"})
    assert r == {"refreshed": True}
    assert oid not in ot._menu_cache


async def test_menu_not_persisted_to_chat_json(seeded_kernel, file_agent, tmp_path):
    """Menu lives in the system block ONLY — never written to chat_<client>.json."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider([["reply"]])
    ot._providers[oid] = fp
    try:
        await seeded_kernel.send(
            oid, {"type": "send", "text": "hi", "client_id": "alice"}
        )
    finally:
        ot._providers.pop(oid, None)
    chat_path = tmp_path / ".fantastic" / "agents" / oid / "chat_alice.json"
    data = json.loads(chat_path.read_text())
    blob = json.dumps(data)
    # Persisted history is just user/assistant turns — none of the
    # menu prose, send-tool how-to, or substrate primer should leak in.
    assert "Available agents" not in blob
    assert "send tool" not in blob.lower()
    assert "Fantastic kernel" not in blob


async def test_contended_send_emits_queued_event(seeded_kernel, file_agent):
    """Second concurrent send arrives while the first holds the lock →
    backend emits a `queued` event tagged with the second caller's
    client_id. UI uses this to mark the message as waiting; first
    `token` for the same client_id implicitly clears the marker."""
    oid = await _make_ollama(seeded_kernel, file_agent)

    # First provider call sleeps so the second `send` finds the lock held.
    class _SlowFirst:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools):
            self.calls += 1
            if self.calls == 1:
                await asyncio.sleep(0.20)
            yield "ok"

    ot._providers[oid] = _SlowFirst()

    captured: list[tuple[str, dict]] = []
    real_send, real_emit = seeded_kernel.send, seeded_kernel.emit

    async def spy_send(target, payload):
        captured.append(("send", target, dict(payload)))
        return await real_send(target, payload)

    async def spy_emit(target, payload):
        captured.append(("emit", target, dict(payload)))
        return await real_emit(target, payload)

    try:
        with (
            patch.object(seeded_kernel, "send", spy_send),
            patch.object(seeded_kernel, "emit", spy_emit),
        ):
            t1 = asyncio.create_task(
                seeded_kernel.send(
                    oid, {"type": "send", "text": "first", "client_id": "alice"}
                )
            )
            await asyncio.sleep(0.05)  # let t1 acquire the lock
            t2 = asyncio.create_task(
                seeded_kernel.send(
                    oid, {"type": "send", "text": "second", "client_id": "bob"}
                )
            )
            r1, r2 = await asyncio.gather(t1, t2)

        assert r1["final"] == "ok" and r2["final"] == "ok"

        # Bob's request hit the lock → got a queued event tagged for bob.
        # Browser-routed events go via emit (target = oid).
        bob_queued = [
            p
            for kind, t, p in captured
            if kind == "emit"
            and t == oid
            and p.get("type") == "queued"
            and p.get("client_id") == "bob"
        ]
        assert len(bob_queued) == 1, (
            f"expected exactly one queued event for bob; got {len(bob_queued)}"
        )

        # Alice did NOT get queued (she acquired the lock first).
        alice_queued = [
            p
            for kind, t, p in captured
            if p.get("type") == "queued" and p.get("client_id") == "alice"
        ]
        assert alice_queued == []

        # Bob then got tokens (cli routing for client='cli'; otherwise
        # emit on agent's own inbox — bob is a non-cli client_id).
        bob_tokens = [
            p
            for kind, t, p in captured
            if kind == "emit"
            and t == oid
            and p.get("type") == "token"
            and p.get("client_id") == "bob"
        ]
        assert len(bob_tokens) >= 1
    finally:
        ot._providers.pop(oid, None)


async def test_concurrent_sends_serialize_per_backend(seeded_kernel, file_agent):
    """Two concurrent _send calls on the same backend → run sequentially
    via the per-backend asyncio.Lock. Verified by the order of provider
    invocations vs the order tasks finish.
    """
    oid = await _make_ollama(seeded_kernel, file_agent)

    in_flight = 0
    max_in_flight = 0
    order: list[str] = []

    class _SerialProvider:
        async def chat(self, messages, tools):
            nonlocal in_flight, max_in_flight
            in_flight += 1
            max_in_flight = max(max_in_flight, in_flight)
            try:
                # Cooperative yield so a second caller WOULD overlap if
                # the lock weren't there. The test asserts overlap = 0.
                await asyncio.sleep(0.05)
                user = messages[-1]["content"]
                order.append(user)
                yield "ok"
            finally:
                in_flight -= 1

    ot._providers[oid] = _SerialProvider()
    try:
        results = await asyncio.gather(
            seeded_kernel.send(
                oid, {"type": "send", "text": "first", "client_id": "a"}
            ),
            seeded_kernel.send(
                oid, {"type": "send", "text": "second", "client_id": "b"}
            ),
        )
        assert max_in_flight == 1, f"saw {max_in_flight} concurrent generations"
        assert order == ["first", "second"], f"FIFO order broke: {order}"
        assert all(r["final"] == "ok" for r in results)
        assert {r["client_id"] for r in results} == {"a", "b"}
    finally:
        ot._providers.pop(oid, None)


# ─── status pipeline ───────────────────────────────────────────


def _capture(seeded_kernel):
    """Spy harness: returns (sends, emits, ctx) where ctx is a context
    manager that patches kernel.send/emit to record dicts."""
    sends: list[tuple[str, dict]] = []
    emits: list[tuple[str, dict]] = []
    real_send, real_emit = seeded_kernel.send, seeded_kernel.emit

    async def spy_send(target, payload):
        sends.append((target, dict(payload)))
        return await real_send(target, payload)

    async def spy_emit(target, payload):
        emits.append((target, dict(payload)))
        return await real_emit(target, payload)

    class _Ctx:
        def __enter__(self):
            self._a = patch.object(seeded_kernel, "send", spy_send)
            self._b = patch.object(seeded_kernel, "emit", spy_emit)
            self._a.__enter__()
            self._b.__enter__()
            return self

        def __exit__(self, *exc):
            self._b.__exit__(*exc)
            self._a.__exit__(*exc)
            return False

    return sends, emits, _Ctx()


def _statuses_for(events: list[dict], client_id: str) -> list[dict]:
    return [
        p
        for p in events
        if p.get("type") == "status" and p.get("client_id") == client_id
    ]


async def test_status_event_sequence_no_tool_calls(seeded_kernel, file_agent):
    """thinking → streaming → done(reason=ok). All carry the same send_id."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider([["hello"]])
    ot._providers[oid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            await seeded_kernel.send(
                oid, {"type": "send", "text": "hi", "client_id": "alice"}
            )
        statuses = [p for _, p in emits if p.get("type") == "status"]
        phases = [s["phase"] for s in statuses]
        assert phases == ["thinking", "streaming", "done"], phases
        assert statuses[-1]["detail"]["reason"] == "ok"
        send_ids = {s["detail"].get("send_id") for s in statuses}
        assert len(send_ids) == 1 and None not in send_ids
    finally:
        ot._providers.pop(oid, None)


async def test_status_event_sequence_with_tool_call(seeded_kernel, file_agent):
    """thinking → streaming → tool_calling(entry) → tool_calling(exit) →
    thinking → streaming → done. Same call_id between tool entry/exit;
    same send_id across all phases."""
    oid = await _make_ollama(seeded_kernel, file_agent)
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
            ["wrapping up."],
        ]
    )
    ot._providers[oid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            await seeded_kernel.send(
                oid, {"type": "send", "text": "list", "client_id": "alice"}
            )
        statuses = [p for _, p in emits if p.get("type") == "status"]
        phases = [s["phase"] for s in statuses]
        # iter 1: thinking, tool_calling entry, tool_calling exit
        # (no streaming because the iter only produced a tool_call)
        # iter 2: thinking (between iterations), streaming, done
        assert phases == [
            "thinking",
            "tool_calling",
            "tool_calling",
            "thinking",
            "streaming",
            "done",
        ], phases
        tool_entries = [s for s in statuses if s["phase"] == "tool_calling"]
        assert "reply_preview" not in tool_entries[0]["detail"]["tool"]
        assert "reply_preview" in tool_entries[1]["detail"]["tool"]
        assert (
            tool_entries[0]["detail"]["tool"]["call_id"]
            == tool_entries[1]["detail"]["tool"]["call_id"]
            == "call_a"
        )
        send_ids = {s["detail"].get("send_id") for s in statuses}
        assert len(send_ids) == 1 and None not in send_ids
    finally:
        ot._providers.pop(oid, None)


async def test_queue_populated_and_drained_on_contention(seeded_kernel, file_agent):
    """Two concurrent sends; _queue grows, _current is set; both drain."""
    oid = await _make_ollama(seeded_kernel, file_agent)

    queue_sizes: list[int] = []

    class _SlowFirst:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools):
            self.calls += 1
            if self.calls == 1:
                # While we're "thinking", measure queue depth.
                await asyncio.sleep(0.10)
                queue_sizes.append(len(ot._queue.get(oid, [])))
                await asyncio.sleep(0.10)
            yield "ok"

    ot._providers[oid] = _SlowFirst()
    try:
        t1 = asyncio.create_task(
            seeded_kernel.send(
                oid, {"type": "send", "text": "first", "client_id": "alice"}
            )
        )
        await asyncio.sleep(0.05)
        t2 = asyncio.create_task(
            seeded_kernel.send(
                oid, {"type": "send", "text": "second", "client_id": "bob"}
            )
        )
        await asyncio.gather(t1, t2)
    finally:
        ot._providers.pop(oid, None)

    # Mid-flight: alice was the current entry (popped from queue),
    # bob was waiting in the queue.
    assert queue_sizes == [1], queue_sizes
    # After both complete: queue empty, no current.
    assert ot._queue.get(oid, []) == []
    assert oid not in ot._current


async def test_status_verb_shape_when_idle(seeded_kernel, file_agent):
    oid = await _make_ollama(seeded_kernel, file_agent)
    r = await seeded_kernel.send(oid, {"type": "status", "client_id": "alice"})
    assert r["generating"] is False
    assert r["current"] is None
    assert r["mine_pending"] == []
    assert r["others_pending"] == 0
    assert r["client_id"] == "alice"


async def test_status_verb_shape_during_inflight_mine(seeded_kernel, file_agent):
    """Mid-flight, the caller's status snapshot reflects current entry
    with is_mine=True and full text."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    captured_status: list[dict] = []

    class _SlowProvider:
        async def chat(self, messages, tools):
            r = await seeded_kernel.send(oid, {"type": "status", "client_id": "alice"})
            captured_status.append(r)
            yield "ok"

    ot._providers[oid] = _SlowProvider()
    try:
        await seeded_kernel.send(
            oid, {"type": "send", "text": "hello there", "client_id": "alice"}
        )
    finally:
        ot._providers.pop(oid, None)

    snap = captured_status[0]
    assert snap["generating"] is True
    assert snap["current"]["is_mine"] is True
    assert snap["current"]["phase"] in ("thinking", "streaming")
    assert snap["current"]["text"] == "hello there"
    assert snap["current"]["elapsed"] >= 0.0


async def test_status_verb_privacy_filter(seeded_kernel, file_agent):
    """Bob's running, alice's queued. Alice's snapshot: is_mine=False,
    no text in current. Bob's snapshot: is_mine=True with text;
    alice's pending shows up as others_pending=1."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    snapshots: dict[str, dict] = {}

    class _PrivacyProvider:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, tools):
            self.calls += 1
            if self.calls == 1:
                # bob runs first; alice gets queued behind him
                await asyncio.sleep(0.15)
                snapshots["alice"] = await seeded_kernel.send(
                    oid, {"type": "status", "client_id": "alice"}
                )
                snapshots["bob"] = await seeded_kernel.send(
                    oid, {"type": "status", "client_id": "bob"}
                )
            yield "ok"

    ot._providers[oid] = _PrivacyProvider()
    try:
        t1 = asyncio.create_task(
            seeded_kernel.send(
                oid, {"type": "send", "text": "bob secret", "client_id": "bob"}
            )
        )
        await asyncio.sleep(0.05)
        t2 = asyncio.create_task(
            seeded_kernel.send(
                oid, {"type": "send", "text": "alice secret", "client_id": "alice"}
            )
        )
        await asyncio.gather(t1, t2)
    finally:
        ot._providers.pop(oid, None)

    a = snapshots["alice"]
    assert a["current"]["is_mine"] is False
    assert "text" not in a["current"]
    assert "text_so_far" not in a["current"]
    assert len(a["mine_pending"]) == 1
    assert a["mine_pending"][0]["text"] == "alice secret"
    assert a["others_pending"] == 0  # bob is current, not in queue

    b = snapshots["bob"]
    assert b["current"]["is_mine"] is True
    assert b["current"]["text"] == "bob secret"
    assert b["mine_pending"] == []
    assert b["others_pending"] == 1  # alice's pending


async def test_status_verb_no_client_id_redacts_text(seeded_kernel, file_agent):
    """Without client_id the snapshot redacts text everywhere."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    snap_holder: list[dict] = []

    class _RedactProvider:
        async def chat(self, messages, tools):
            snap_holder.append(await seeded_kernel.send(oid, {"type": "status"}))
            yield "ok"

    ot._providers[oid] = _RedactProvider()
    try:
        await seeded_kernel.send(
            oid, {"type": "send", "text": "private text", "client_id": "alice"}
        )
    finally:
        ot._providers.pop(oid, None)

    snap = snap_holder[0]
    assert snap["client_id"] is None
    assert snap["current"] is not None
    assert "text" not in snap["current"]
    assert "text_so_far" not in snap["current"]
    assert snap["others_pending"] >= 0


async def test_status_done_emits_with_reason_interrupt(seeded_kernel, file_agent):
    """Interrupt mid-stream → final status carries reason='interrupted'."""
    oid = await _make_ollama(seeded_kernel, file_agent)

    class _SlowProvider:
        async def chat(self, messages, tools):
            await asyncio.sleep(2.0)
            yield "never"

    ot._providers[oid] = _SlowProvider()
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            send_task = asyncio.create_task(
                seeded_kernel.send(
                    oid, {"type": "send", "text": "stop me", "client_id": "alice"}
                )
            )
            await asyncio.sleep(0.10)
            await seeded_kernel.send(oid, {"type": "interrupt"})
            await send_task
        done_status = [
            p
            for _, p in emits
            if p.get("type") == "status" and p.get("phase") == "done"
        ]
        assert len(done_status) == 1
        assert done_status[0]["detail"]["reason"] == "interrupted"
    finally:
        ot._providers.pop(oid, None)


async def test_status_done_emits_with_reason_timeout(
    seeded_kernel, file_agent, monkeypatch
):
    """Patched low SEND_TIMEOUT → done with reason='timeout'."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    monkeypatch.setattr(ot, "SEND_TIMEOUT", 0.05)

    class _ForeverProvider:
        async def chat(self, messages, tools):
            await asyncio.sleep(5.0)
            yield "never"

    ot._providers[oid] = _ForeverProvider()
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            r = await seeded_kernel.send(
                oid, {"type": "send", "text": "slow", "client_id": "alice"}
            )
        assert "timeout" in r.get("error", "")
        done_status = [
            p
            for _, p in emits
            if p.get("type") == "status" and p.get("phase") == "done"
        ]
        assert len(done_status) == 1
        assert done_status[0]["detail"]["reason"] == "timeout"
    finally:
        ot._providers.pop(oid, None)


async def test_status_event_includes_send_id(seeded_kernel, file_agent):
    """Drift guard: every status event detail carries a send_id."""
    oid = await _make_ollama(seeded_kernel, file_agent)
    fp = _FakeProvider([["ok"]])
    ot._providers[oid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            await seeded_kernel.send(
                oid, {"type": "send", "text": "hi", "client_id": "alice"}
            )
        statuses = [p for _, p in emits if p.get("type") == "status"]
        for s in statuses:
            assert "send_id" in s["detail"], s
    finally:
        ot._providers.pop(oid, None)
