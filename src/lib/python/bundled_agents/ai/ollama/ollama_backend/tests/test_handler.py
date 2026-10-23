"""ollama_backend handler — verb dispatching, multi-step _run."""

from __future__ import annotations

from _testkit import ensure_memory
import asyncio


import ollama_backend.tools as ot
from ai_core import core


async def _async_iter(items):
    for it in items:
        yield it


class _FakeProvider:
    """Provider stub that yields a scripted stream from .chat()."""

    def __init__(self, scripts: list[list]):
        # `scripts` is a list of stream-iters, one per `chat()` call (multi-step support).
        self._scripts = list(scripts)
        self.calls = 0

    async def chat(self, messages, max_tokens=None):
        items = self._scripts.pop(0) if self._scripts else []
        self.calls += 1
        for x in items:
            yield x


async def _make_ollama(kernel):
    # Deny-all: the backend streams its turn to `cli` via send (default client),
    # and keeps its conversation in its memory store — both explicit grants.
    store = ensure_memory(kernel)
    can_call = {"cli": "*", store: "*"}
    meta = {
        "handler_module": "ollama_backend.tools",
        # required at the write (the schema); the provider is faked, never dialled
        "endpoint": "http://127.0.0.1:9/stub",
        "model": "stub",
        "can_call": can_call,
        "memory_store_id": store,
    }
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
    aid = rec["id"]
    # Two-sided deny-all: open the callee's reciprocal `callable_by` for each edge.
    for target in can_call:
        await _accept(kernel, target, aid)
    return aid


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


async def test_docs_list_send(seeded_kernel, store_agent):
    oid = await _make_ollama(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "docs", "of": oid}}]
        )
    )[0]["result"]
    assert r["bundle"] == "ollama_backend.tools" and "send" in r["verbs"]


async def test_send_works(seeded_kernel, store_agent, fake_providers):
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([["ok"]])
    fake_providers[oid] = fp
    try:
        r = (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
        assert r["final"] == "ok"
    finally:
        fake_providers.pop(oid, None)


async def test_history_empty_when_no_turns(seeded_kernel, store_agent):
    oid = await _make_ollama(seeded_kernel)
    r = (await seeded_kernel.send([{"target": oid, "payload": {"type": "recall"}}]))[0][
        "result"
    ]
    assert r["messages"] == []


async def test_history_returns_messages(seeded_kernel, store_agent, fake_providers):
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([['hello <call name="done"></call>']])
    fake_providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
    finally:
        fake_providers.pop(oid, None)
    r = (await seeded_kernel.send([{"target": oid, "payload": {"type": "recall"}}]))[0][
        "result"
    ]
    assert [m["content"] for m in r["messages"]] == [
        "hi",
        'hello <call name="done"></call>',
    ]


async def test_run_no_tool_calls_returns_content(
    seeded_kernel, store_agent, fake_providers
):
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([["Hello! ", "How are you?"]])
    fake_providers[oid] = fp
    try:
        r = (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
        assert r["final"] == "Hello! How are you?"
        assert fp.calls == 1
    finally:
        fake_providers.pop(oid, None)


# ─── system_prompt / messages door + recursion guard ────────────


class _RecordProvider:
    """Like _FakeProvider but records the messages list of each chat() call."""

    def __init__(self, scripts: list[list]):
        self._scripts = list(scripts)
        self.calls = 0
        self.last_messages = None

    async def chat(self, messages, max_tokens=None):
        self.last_messages = list(messages)  # snapshot — _run mutates the list later
        items = self._scripts.pop(0) if self._scripts else []
        self.calls += 1
        for x in items:
            yield x


async def test_system_prompt_override_replaces_auto_prompt(
    seeded_kernel, store_agent, fake_providers
):
    """A caller-supplied system_prompt REPLACES the auto-built substrate prompt."""
    oid = await _make_ollama(seeded_kernel)
    rp = _RecordProvider([["ok"]])
    fake_providers[oid] = rp
    try:
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": oid,
                        "payload": {
                            "type": "send",
                            "text": "hi",
                            "system_prompt": "ROLE-XYZ",
                        },
                    }
                ]
            )
        )[0]["result"]
        assert r["final"] == "ok"
        assert rp.last_messages[0]["role"] == "system"
        assert rp.last_messages[0]["content"] == "ROLE-XYZ"
    finally:
        fake_providers.pop(oid, None)


async def test_a_send_with_no_memory_store_fails_loud(seeded_kernel, fake_providers):
    """No `memory_store_id` ⇒ no turn: the send refuses up front, naming the fix —
    the conversation has nowhere to live, and nothing falls back to RAM."""
    rec = seeded_kernel.create(
        "ollama_backend.tools", id="storeless", can_call={"cli": "*"}
    )
    assert rec.get("id") == "storeless", rec
    fake_providers["storeless"] = _FakeProvider([["never"]])
    try:
        boot = (
            await seeded_kernel.send(
                [{"target": "storeless", "payload": {"type": "boot"}}]
            )
        )[0]["result"]
        assert boot["reason"] == "missing_arg" and boot["arg"] == "memory_store_id"
        out = (
            await seeded_kernel.send(
                [{"target": "storeless", "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
        assert out["reason"] == "missing_arg" and "memory_store_id" in out["error"]
        assert fake_providers["storeless"].calls == 0, "the model was never called"
    finally:
        fake_providers.pop("storeless", None)


async def test_send_refuses_cycle(seeded_kernel, store_agent):
    """_call_stack containing self → immediate cycle refusal (before the lock)."""
    oid = await _make_ollama(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": oid,
                    "payload": {"type": "send", "text": "hi", "_call_stack": [oid]},
                }
            ]
        )
    )[0]["result"]
    assert "error" in r and "cycle" in r["error"]


async def test_send_refuses_over_depth(seeded_kernel, store_agent):
    """_call_stack at/over MAX_CALL_DEPTH → depth refusal (before the lock)."""
    oid = await _make_ollama(seeded_kernel)
    deep = [f"x{i}" for i in range(core.MAX_CALL_DEPTH)]
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": oid,
                    "payload": {"type": "send", "text": "hi", "_call_stack": deep},
                }
            ]
        )
    )[0]["result"]
    assert "error" in r and "depth" in r["error"]


async def test_run_persists_history(seeded_kernel, store_agent, fake_providers):
    """Default caller (no client_id) persists user+assistant to the chat JSONL sidecar."""
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([["reply"]])
    fake_providers[oid] = fp
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
    finally:
        fake_providers.pop(oid, None)
    data = (
        (await seeded_kernel.send([{"target": oid, "payload": {"type": "recall"}}]))[0][
            "result"
        ]
    )["messages"]
    assert (data[-2]["role"], data[-2]["content"]) == ("user", "hi")
    assert (data[-1]["role"], data[-1]["content"]) == ("assistant", "reply")
    # System prompt is NEVER persisted — rebuilt fresh each turn from the live menu.
    assert all(m.get("role") != "system" for m in data)


async def test_unknown_verb_errors(seeded_kernel, store_agent):
    oid = await _make_ollama(seeded_kernel)
    r = (await seeded_kernel.send([{"target": oid, "payload": {"type": "garbage"}}]))[
        0
    ]["result"]
    assert "error" in r


async def test_cli_caller_routes_to_cli_only(
    seeded_kernel, store_agent, fake_providers
):
    """Default caller (client_id='cli') has events dispatched via
    kernel.send([{"target": 'cli', "payload": …}]) so cli's stdout handler runs. No leak to the
    backend's own inbox (which would broadcast to unrelated WS subscribers).
    """
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([["chunk1 ", "chunk2"]])
    fake_providers[oid] = fp
    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(events.append)
    try:
        (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "hi"}}]
            )
        )[0]["result"]
        sends = [
            (e["agent_id"], e["payload"]) for e in events if e.get("kind") == "send"
        ]
        emits = [
            (e["agent_id"], e["payload"]) for e in events if e.get("kind") == "emit"
        ]

        # cli received tokens via kernel.send (so its handler ran).
        cli_tokens = [p for t, p in sends if t == "cli" and p.get("type") == "token"]
        cli_done = [
            p
            for t, p in sends
            if t == "cli" and (p.get("type") == "status" and p.get("phase") == "done")
        ]
        assert len(cli_tokens) == 2
        assert len(cli_done) == 1
        # Backend's own inbox got NO stream events (cli routing only).
        own_stream = [
            p
            for t, p in emits
            if t == oid
            and p.get("type") in ("token", "say")
            or (p.get("type") == "status" and p.get("phase") == "done")
        ]
        assert own_stream == [], f"unexpected leak to own inbox: {own_stream}"
        # Stream events carry client_id="cli".
        assert all(p.get("client_id") == "cli" for p in cli_tokens + cli_done)
    finally:
        unsub()
        fake_providers.pop(oid, None)


async def test_browser_caller_routes_to_own_inbox_only(
    seeded_kernel, store_agent, fake_providers
):
    """Caller passes a non-cli client_id (e.g. browser uuid) → events
    emit to backend's own inbox tagged with client_id, NOT to cli.
    """
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([["a", "b"]])
    fake_providers[oid] = fp
    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(events.append)
    try:
        (
            await seeded_kernel.send(
                [
                    {
                        "target": oid,
                        "payload": {
                            "type": "send",
                            "text": "hi",
                            "client_id": "web_xyz",
                        },
                    }
                ]
            )
        )[0]["result"]
        sends = [
            (e["agent_id"], e["payload"]) for e in events if e.get("kind") == "send"
        ]
        emits = [
            (e["agent_id"], e["payload"]) for e in events if e.get("kind") == "emit"
        ]

        # Backend's own inbox got the stream, tagged with the web client_id.
        own_tokens = [p for t, p in emits if t == oid and p.get("type") == "token"]
        own_done = [
            p
            for t, p in emits
            if t == oid and (p.get("type") == "status" and p.get("phase") == "done")
        ]
        assert len(own_tokens) == 2
        assert len(own_done) == 1
        assert all(p["client_id"] == "web_xyz" for p in own_tokens + own_done)
        # cli got NO stream events (no leak across clients).
        cli_stream = [
            p
            for t, p in sends
            if t == "cli"
            and p.get("type") in ("token", "say")
            or (p.get("type") == "status" and p.get("phase") == "done")
        ]
        assert cli_stream == [], f"cli leak: {cli_stream}"
    finally:
        unsub()
        fake_providers.pop(oid, None)


async def test_contended_send_emits_queued_event(
    seeded_kernel, store_agent, fake_providers
):
    """Second concurrent send arrives while the first holds the lock →
    backend emits a `queued` event tagged with the second caller's
    client_id. UI uses this to mark the message as waiting; first
    `token` for the same client_id implicitly clears the marker."""
    oid = await _make_ollama(seeded_kernel)

    # First provider call sleeps so the second `send` finds the lock held.
    class _SlowFirst:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, max_tokens=None):
            self.calls += 1
            if self.calls == 1:
                await asyncio.sleep(0.20)
            yield 'ok <call name="done"></call>'

    fake_providers[oid] = _SlowFirst()

    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(events.append)
    try:
        t1 = asyncio.create_task(
            seeded_kernel.send(
                [
                    {
                        "target": oid,
                        "payload": {
                            "type": "send",
                            "text": "first",
                            "client_id": "alice",
                        },
                    }
                ]
            )
        )
        await asyncio.sleep(0.05)  # let t1 acquire the lock
        t2 = asyncio.create_task(
            seeded_kernel.send(
                [
                    {
                        "target": oid,
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
        captured = [
            (e["kind"], e["agent_id"], e["payload"])
            for e in events
            if e.get("kind") in ("send", "emit")
        ]

        assert r1["final"].startswith("ok") and r2["final"].startswith("ok")

        # Bob's request hit the lock → got a queued event tagged for bob.
        # Browser-routed events go via emit (target = oid).
        bob_queued = [
            p
            for kind, t, p in captured
            if kind == "emit"
            and t == oid
            and (p.get("type") == "status" and p.get("phase") == "queued")
            and p.get("client_id") == "bob"
        ]
        assert len(bob_queued) == 1, (
            f"expected exactly one queued event for bob; got {len(bob_queued)}"
        )

        # Alice did NOT get queued (she acquired the lock first).
        alice_queued = [
            p
            for kind, t, p in captured
            if (p.get("type") == "status" and p.get("phase") == "queued")
            and p.get("client_id") == "alice"
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
        unsub()
        fake_providers.pop(oid, None)


async def test_concurrent_sends_serialize_per_backend(
    seeded_kernel, store_agent, fake_providers
):
    """Two concurrent _send calls on the same backend → run sequentially
    via the per-backend asyncio.Lock. Verified by the order of provider
    invocations vs the order tasks finish.
    """
    oid = await _make_ollama(seeded_kernel)

    in_flight = 0
    max_in_flight = 0
    order: list[str] = []
    seen: list[list[str]] = []

    class _SerialProvider:
        async def chat(self, messages, max_tokens=None):
            nonlocal in_flight, max_in_flight
            in_flight += 1
            max_in_flight = max(max_in_flight, in_flight)
            try:
                # Cooperative yield so a second caller WOULD overlap if
                # the lock weren't there. The test asserts overlap = 0.
                await asyncio.sleep(0.05)
                user = messages[-1]["content"]
                order.append(user)
                seen.append([m["content"] for m in messages])
                yield 'ok <call name="done"></call>'
            finally:
                in_flight -= 1

    fake_providers[oid] = _SerialProvider()
    try:
        results = await asyncio.gather(
            seeded_kernel.send(
                [
                    {
                        "target": oid,
                        "payload": {"type": "send", "text": "first", "client_id": "a"},
                    }
                ]
            ),
            seeded_kernel.send(
                [
                    {
                        "target": oid,
                        "payload": {"type": "send", "text": "second", "client_id": "b"},
                    }
                ]
            ),
        )
        assert max_in_flight == 1, f"saw {max_in_flight} concurrent generations"
        # ONE CHAIN PER AGENT (#227): "second" landed while the first chain was
        # alive, so it was HELD — stored at once and read by the chain's one wake
        # turn, which answers it under its own client_id.
        assert order[0] == "first", order
        assert order[1].startswith("[1 message(s) arrived while you were working"), (
            order
        )
        assert "second" in seen[1], seen
        replies = [outs[0]["result"] for outs in results]
        assert all(r["final"].startswith("ok") for r in replies)
        assert {r["client_id"] for r in replies} == {"a", "b"}
    finally:
        fake_providers.pop(oid, None)


# ─── status pipeline ───────────────────────────────────────────


def _capture(seeded_kernel):
    """Spy harness: returns (sends, emits, ctx) where ctx is a context
    manager that subscribes to the substrate state stream and splits
    events into sends/emits with full payloads. Replaces the old
    patch.object pattern, which can't see bundle-internal sends in
    the new Agent model where each agent has its own .send method."""
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


def _statuses_for(events: list[dict], client_id: str) -> list[dict]:
    return [
        p
        for p in events
        if p.get("type") == "status" and p.get("client_id") == client_id
    ]


async def test_status_event_sequence_no_tool_calls(
    seeded_kernel, store_agent, fake_providers
):
    """thinking → streaming → done(reason=ok). All carry the same send_id."""
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([['hello <call name="done"></call>']])
    fake_providers[oid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            (
                await seeded_kernel.send(
                    [
                        {
                            "target": oid,
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
        fake_providers.pop(oid, None)


async def test_queue_populated_and_drained_on_contention(
    seeded_kernel, store_agent, fake_providers
):
    """Two concurrent sends; _queue grows, _current is set; both drain."""
    oid = await _make_ollama(seeded_kernel)

    queue_sizes: list[int] = []

    class _SlowFirst:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, max_tokens=None):
            self.calls += 1
            if self.calls == 1:
                # While we're "thinking", measure queue depth.
                await asyncio.sleep(0.10)
                queue_sizes.append(len(core._queue.get(oid, [])))
                await asyncio.sleep(0.10)
            yield 'ok <call name="done"></call>'

    fake_providers[oid] = _SlowFirst()
    try:
        t1 = asyncio.create_task(
            seeded_kernel.send(
                [
                    {
                        "target": oid,
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
        # ONE CHAIN PER AGENT (#227): a plain message mid-chain is HELD, not
        # queued; a turn of the running chain (its token) waits in the queue.
        t2 = asyncio.create_task(
            seeded_kernel.send(
                [
                    {
                        "target": oid,
                        "payload": {
                            "type": "send",
                            "text": "second",
                            "client_id": "bob",
                            "chain": core._chains[oid]["token"],
                        },
                    }
                ]
            )
        )
        await asyncio.gather(t1, t2)
    finally:
        fake_providers.pop(oid, None)

    # Mid-flight: alice was the current entry (popped from queue),
    # bob was waiting in the queue.
    assert queue_sizes == [1], queue_sizes
    # After both complete: queue empty, no current.
    assert core._queue.get(oid, []) == []
    assert oid not in core._current


async def test_status_verb_shape_when_idle(seeded_kernel, store_agent):
    oid = await _make_ollama(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [{"target": oid, "payload": {"type": "status", "client_id": "alice"}}]
        )
    )[0]["result"]
    assert r["generating"] is False
    assert r["current"] is None
    assert r["mine_pending"] == []
    assert r["others_pending"] == 0
    assert r["client_id"] == "alice"


async def test_status_verb_shape_during_inflight_mine(
    seeded_kernel, store_agent, fake_providers
):
    """Mid-flight, the caller's status snapshot reflects current entry
    with is_mine=True and full text."""
    oid = await _make_ollama(seeded_kernel)
    captured_status: list[dict] = []

    class _SlowProvider:
        async def chat(self, messages, max_tokens=None):
            r = (
                await seeded_kernel.send(
                    [
                        {
                            "target": oid,
                            "payload": {"type": "status", "client_id": "alice"},
                        }
                    ]
                )
            )[0]["result"]
            captured_status.append(r)
            yield 'ok <call name="done"></call>'

    fake_providers[oid] = _SlowProvider()
    try:
        (
            await seeded_kernel.send(
                [
                    {
                        "target": oid,
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
        fake_providers.pop(oid, None)

    snap = captured_status[0]
    assert snap["generating"] is True
    assert snap["current"]["is_mine"] is True
    assert snap["current"]["phase"] in ("thinking", "streaming")
    assert snap["current"]["text"] == "hello there"
    assert snap["current"]["elapsed"] >= 0.0


async def test_status_verb_privacy_filter(seeded_kernel, store_agent, fake_providers):
    """Bob's running, alice's queued. Alice's snapshot: is_mine=False,
    no text in current. Bob's snapshot: is_mine=True with text;
    alice's pending shows up as others_pending=1."""
    oid = await _make_ollama(seeded_kernel)
    snapshots: dict[str, dict] = {}

    class _PrivacyProvider:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, max_tokens=None):
            self.calls += 1
            if self.calls == 1:
                # bob runs first; alice gets queued behind him
                await asyncio.sleep(0.15)
                snapshots["alice"] = (
                    await seeded_kernel.send(
                        [
                            {
                                "target": oid,
                                "payload": {"type": "status", "client_id": "alice"},
                            }
                        ]
                    )
                )[0]["result"]
                snapshots["bob"] = (
                    await seeded_kernel.send(
                        [
                            {
                                "target": oid,
                                "payload": {"type": "status", "client_id": "bob"},
                            }
                        ]
                    )
                )[0]["result"]
            yield 'ok <call name="done"></call>'

    fake_providers[oid] = _PrivacyProvider()
    try:
        t1 = asyncio.create_task(
            seeded_kernel.send(
                [
                    {
                        "target": oid,
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
                        "target": oid,
                        "payload": {
                            "type": "send",
                            "text": "alice secret",
                            "client_id": "alice",
                            # ONE CHAIN PER AGENT (#227): a turn of the running chain
                            # (its token) waits in the queue; a plain message is held.
                            "chain": core._chains[oid]["token"],
                        },
                    }
                ]
            )
        )
        await asyncio.gather(t1, t2)
    finally:
        fake_providers.pop(oid, None)

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


async def test_status_verb_no_client_id_redacts_text(
    seeded_kernel, store_agent, fake_providers
):
    """Without client_id the snapshot redacts text everywhere."""
    oid = await _make_ollama(seeded_kernel)
    snap_holder: list[dict] = []

    class _RedactProvider:
        async def chat(self, messages, max_tokens=None):
            snap_holder.append(
                (
                    await seeded_kernel.send(
                        [{"target": oid, "payload": {"type": "status"}}]
                    )
                )[0]["result"]
            )
            yield 'ok <call name="done"></call>'

    fake_providers[oid] = _RedactProvider()
    try:
        (
            await seeded_kernel.send(
                [
                    {
                        "target": oid,
                        "payload": {
                            "type": "send",
                            "text": "private text",
                            "client_id": "alice",
                        },
                    }
                ]
            )
        )[0]["result"]
    finally:
        fake_providers.pop(oid, None)

    snap = snap_holder[0]
    assert snap["client_id"] is None
    assert snap["current"] is not None
    assert "text" not in snap["current"]
    assert "text_so_far" not in snap["current"]
    assert snap["others_pending"] >= 0


async def test_kernel_cancel_kills_an_in_flight_send(
    seeded_kernel, store_agent, fake_providers
):
    """The backend's bespoke `interrupt` is GONE — an in-flight `send` is a KERNEL
    TASK, so it is cancelled the way any dispatch is: find it in `ps`, kill it with
    `cancel`. The bundle's `on_cancel` hook closes the turn out (status(done,
    reason='cancelled') + `done` reach the caller), the blocked `send` resolves to
    {'cancelled': True} — the substrate's contract, not this bundle's — and the
    backend reports no phantom generation afterwards."""
    oid = await _make_ollama(seeded_kernel)

    class _SlowProvider:
        async def chat(self, messages, max_tokens=None):
            await asyncio.sleep(5.0)
            yield "never"

    fake_providers[oid] = _SlowProvider()
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            send_task = asyncio.create_task(
                seeded_kernel.send(
                    [
                        {
                            "target": oid,
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

            # Discover the generation in the live task table…
            table = (
                await seeded_kernel.send(
                    [{"target": "kernel", "payload": {"type": "ps"}}]
                )
            )[0]["result"]
            rows = [
                t for t in table["tasks"] if t["agent"] == oid and t["verb"] == "send"
            ]
            assert len(rows) == 1, table["tasks"]
            task_id = rows[0]["id"]

            # …and kill it with the universal system verb.
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

            # The blocked caller's send resolves — cancellation never raises out.
            out = (await send_task)[0]
            assert out["status"] == "cancelled"
            assert out["result"] == {"cancelled": True}

        # The `on_cancel` hook closed the turn out: status(done, cancelled) + done.
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

        # …and the backend no longer reports a phantom generation.
        snap = (
            await seeded_kernel.send([{"target": oid, "payload": {"type": "status"}}])
        )[0]["result"]
        assert snap["generating"] is False
        assert snap["current"] is None
        rr = (
            await seeded_kernel.send([{"target": oid, "payload": {"type": "reflect"}}])
        )[0]["result"]
        assert rr["generating"] is False
        docs = (
            await seeded_kernel.send(
                [{"target": "kernel", "payload": {"type": "docs", "of": oid}}]
            )
        )[0]["result"]
        assert "interrupt" not in docs["verbs"]
        # The FIFO lock was released with the aborted dispatch — the next send runs.
        fake_providers[oid] = _FakeProvider([["after"]])
        assert (
            (
                (
                    await seeded_kernel.send(
                        [{"target": oid, "payload": {"type": "send", "text": "again"}}]
                    )
                )[0]["result"]
            )["final"]
            == "after"
        )
    finally:
        fake_providers.pop(oid, None)


async def test_on_cancel_ignores_a_non_generation_task(seeded_kernel, store_agent):
    """`on_cancel` is keyed on the TASK id the generation stamped on `_current` —
    cancelling some OTHER dispatch on the same agent must not close out the turn
    (no phantom `done`, no lost in-flight entry)."""
    oid = await _make_ollama(seeded_kernel)
    agent = seeded_kernel.ctx.agents[oid]
    try:
        # No generation in flight: a stray task id for this agent is a no-op.
        await ot.on_cancel(oid, f"{oid}:999", agent)
        assert oid not in core._current
        # A generation in flight, but a DIFFERENT task id → entry survives.
        core._current[oid] = {
            "client_id": "alice",
            "send_id": "s1",
            "started_at": 0.0,
            "phase": "thinking",
            "text_so_far": "",
            "task_id": f"{oid}:1",
        }
        await ot.on_cancel(oid, f"{oid}:2", agent)
        assert core._current[oid]["send_id"] == "s1"
    finally:
        core._current.pop(oid, None)


async def test_status_done_emits_with_reason_timeout(
    seeded_kernel, store_agent, monkeypatch, fake_providers
):
    """Patched low SEND_TIMEOUT → done with reason='timeout'."""
    oid = await _make_ollama(seeded_kernel)
    monkeypatch.setattr(core, "SEND_TIMEOUT", 0.05)

    class _ForeverProvider:
        async def chat(self, messages, max_tokens=None):
            await asyncio.sleep(5.0)
            yield "never"

    fake_providers[oid] = _ForeverProvider()
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            r = (
                await seeded_kernel.send(
                    [
                        {
                            "target": oid,
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
        assert len(done_status) == 1
        assert done_status[0]["detail"]["reason"] == "timeout"
    finally:
        fake_providers.pop(oid, None)


async def test_status_event_includes_send_id(
    seeded_kernel, store_agent, fake_providers
):
    """Drift guard: every status event detail carries a send_id."""
    oid = await _make_ollama(seeded_kernel)
    fp = _FakeProvider([["ok"]])
    fake_providers[oid] = fp
    sends, emits, ctx = _capture(seeded_kernel)
    try:
        with ctx:
            (
                await seeded_kernel.send(
                    [
                        {
                            "target": oid,
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
        fake_providers.pop(oid, None)
