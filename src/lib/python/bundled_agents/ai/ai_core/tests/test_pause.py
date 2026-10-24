"""PAUSE / RESUME (#219): a paused AI agent stops its generation and HOLDS every
incoming message, in order, in its memory store. `resume` writes one resumed row and
releases the held messages after it. The pause state and the held queue are rows in
the store, so a restart reads them back. Port of the Rust #219 tests
(fantastic-ai-core `tests_pause.rs`)."""

from __future__ import annotations

import asyncio

from _testkit import memory_meta

from ai_core import core

DONE = '<call name="done"></call>'


class _Recorder:
    """Answers DONE to every pass and records the messages of every request."""

    def __init__(self):
        self.calls = 0
        self.seen: list[list[dict]] = []

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.seen.append(list(messages))
        yield DONE


class _Streamer:
    """Streams a partial answer, then stalls so the turn can be cut mid-answer."""

    def __init__(self, partial: str):
        self.partial = partial
        self.calls = 0
        self.seen: list[list[dict]] = []

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.seen.append(list(messages))
        yield self.partial
        await asyncio.sleep(5)
        yield " never seen"


def _agent(kernel, aid):
    kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(kernel, aid, {"cli": "*"}),
    )


async def _verb(kernel, aid, payload):
    return (await kernel.send([{"target": aid, "payload": payload}]))[0].get("result")


async def _say(kernel, aid, text):
    return await _verb(
        kernel,
        aid,
        {"type": "send", "text": text, "client_id": "g"},
    )


async def _request_with(fp, needle: str, timeout: float = 5.0) -> list[dict]:
    """The messages of the first request whose content contains `needle`."""
    t0 = asyncio.get_event_loop().time()
    while True:
        for msgs in fp.seen:
            if any(needle in str(m.get("content")) for m in msgs):
                return msgs
        assert asyncio.get_event_loop().time() - t0 < timeout, (
            f"no request carried {needle!r}; seen={fp.seen}"
        )
        await asyncio.sleep(0.02)


def _position(msgs: list[dict], needle: str) -> int:
    for i, m in enumerate(msgs):
        if needle in str(m.get("content")):
            return i
    raise AssertionError(f"{needle!r} not in request: {msgs}")


async def _rows_of_kind(kernel, aid, kind: str) -> list[dict]:
    rows = await core._refresh_head(aid, kernel)
    return [r for r in rows if r.get("kind") == kind]


async def test_pause_stops_the_generation_and_holds_a_send(seeded_kernel, fake_providers):
    aid = "pause_hold"
    fp = _Recorder()
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        assert await _verb(seeded_kernel, aid, {"type": "pause"}) == {
            "paused": True,
            "held": 0,
        }
        reply = await _say(seeded_kernel, aid, "held-probe")
        assert reply["held"] == 1 and reply["paused"] is True, reply
        await asyncio.sleep(0.2)
        assert not any(
            "held-probe" in str(m.get("content")) for msgs in fp.seen for m in msgs
        ), "a held message reached the model"
    finally:
        fake_providers.pop(aid, None)


async def test_resume_runs_held_after_the_resumed_row(seeded_kernel, fake_providers):
    aid = "pause_resume"
    fp = _Recorder()
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _verb(seeded_kernel, aid, {"type": "pause"})
        await _say(seeded_kernel, aid, "held-probe")
        assert await _verb(seeded_kernel, aid, {"type": "resume"}) == {
            "resumed": True,
            "released": 1,
        }
        msgs = await _request_with(fp, "held-probe")
        assert _position(msgs, "resumed after a pause") < _position(msgs, "held-probe")
    finally:
        fake_providers.pop(aid, None)


async def test_held_messages_keep_their_order(seeded_kernel, fake_providers):
    aid = "pause_order"
    fp = _Recorder()
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _verb(seeded_kernel, aid, {"type": "pause"})
        for text in ("m1-probe", "m2-probe", "m3-probe"):
            await _say(seeded_kernel, aid, text)
        assert await _verb(seeded_kernel, aid, {"type": "resume"}) == {
            "resumed": True,
            "released": 3,
        }
        msgs = await _request_with(fp, "m3-probe")
        assert (
            _position(msgs, "m1-probe")
            < _position(msgs, "m2-probe")
            < _position(msgs, "m3-probe")
        )
    finally:
        fake_providers.pop(aid, None)


async def test_pause_and_resume_twice_are_noops(seeded_kernel, fake_providers):
    aid = "pause_twice"
    fp = _Recorder()
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        first = await _verb(seeded_kernel, aid, {"type": "pause"})
        second = await _verb(seeded_kernel, aid, {"type": "pause"})
        assert first["paused"] is True and second["paused"] is True, (first, second)
        assert len(await _rows_of_kind(seeded_kernel, aid, "pause")) == 1
        assert await _verb(seeded_kernel, aid, {"type": "resume"}) == {
            "resumed": True,
            "released": 0,
        }
        assert await _verb(seeded_kernel, aid, {"type": "resume"}) == {
            "resumed": False,
            "released": 0,
        }
    finally:
        fake_providers.pop(aid, None)


async def test_pause_survives_a_restart(seeded_kernel, fake_providers):
    aid = "pause_restart"
    fp = _Recorder()
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _verb(seeded_kernel, aid, {"type": "pause"})
        await _say(seeded_kernel, aid, "held-a-probe")
        # the process restarts: the head cache is gone, the store is not
        core._mem(seeded_kernel)["heads"].pop(aid, None)
        reply = await _say(seeded_kernel, aid, "held-b-probe")
        assert reply["held"] == 2 and reply["paused"] is True, reply
        assert await _verb(seeded_kernel, aid, {"type": "resume"}) == {
            "resumed": True,
            "released": 2,
        }
        msgs = await _request_with(fp, "held-b-probe")
        assert _position(msgs, "held-a-probe") < _position(msgs, "held-b-probe")
    finally:
        fake_providers.pop(aid, None)


async def test_pause_cancels_a_stalled_generation_and_stores_the_notice(
    seeded_kernel, fake_providers
):
    aid = "pause_stalled"
    fp = _Streamer("half an answer")
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        turn = asyncio.ensure_future(_say(seeded_kernel, aid, "the question"))
        for _ in range(100):
            if fp.calls:
                break
            await asyncio.sleep(0.02)
        await asyncio.sleep(0.15)
        assert await _verb(seeded_kernel, aid, {"type": "pause"}) == {
            "paused": True,
            "held": 0,
        }
        await asyncio.wait_for(turn, 5)
        notices = await _rows_of_kind(seeded_kernel, aid, "stopped")
        assert notices and "stopped by" in notices[-1]["content"], notices
    finally:
        fake_providers.pop(aid, None)
