"""ONE CHAIN PER AGENT (#227): a message that lands while the agent's chain is
alive joins that chain — stored as a row at once, read by the chain's next pass,
no pass of its own. A chain that ends with a message held wakes ONCE for it;
`stop` ends the chain and wakes nothing. Port of the Rust #234 tests."""

from __future__ import annotations

import asyncio
import time

from _testkit import memory_meta

from ai_core import core

DONE = '<call name="done"></call>'
REFLECT = '<call name="send"><target_id>kernel</target_id><type>reflect</type></call>'


class _Script:
    """A scripted mind: yields the queued replies, records every pass; the first
    pass sleeps `delay` so a second message lands while the chain is alive."""

    def __init__(self, turns, delay: float = 0.0):
        self.turns = list(turns)
        self.calls = 0
        self.incoming: list[str] = []
        self.seen: list[list[dict]] = []
        self.delay = delay

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.incoming.append(messages[-1]["content"])
        self.seen.append(list(messages))
        if self.calls == 1 and self.delay:
            await asyncio.sleep(self.delay)
        yield self.turns.pop(0) if self.turns else DONE


async def _settle(fp, want, timeout=5.0):
    t0 = asyncio.get_event_loop().time()
    while fp.calls < want:
        assert asyncio.get_event_loop().time() - t0 < timeout, (
            f"stalled at {fp.calls} passes; incoming={fp.incoming}"
        )
        await asyncio.sleep(0.02)


def _agent(kernel, aid):
    kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(kernel, aid, {"cli": "*"}),
    )


def _send(kernel, aid, text, client_id):
    return asyncio.ensure_future(
        kernel.send(
            [
                {
                    "target": aid,
                    "payload": {"type": "send", "text": text, "client_id": client_id},
                }
            ]
        )
    )


def _read(fp, n, text) -> bool:
    """Whether pass `n` read a message whose content contains `text`."""
    return any(text in str(m.get("content")) for m in fp.seen[n])


async def test_a_message_mid_chain_joins_the_chain(seeded_kernel, fake_providers):
    aid = "oc_join"
    fp = _Script([REFLECT, DONE], delay=0.3)
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        first = _send(seeded_kernel, aid, "go", "g")
        await _settle(fp, 1)
        await asyncio.sleep(0.1)
        second = _send(seeded_kernel, aid, "two", "t")
        await asyncio.sleep(0.1)
        assert not second.done(), "a held send waits for its answer"
        await first
        await _settle(fp, 2)
        two = (await second)[0]["result"]
        assert "response" in two
        assert two["client_id"] == "t"
        assert _read(fp, 1, "two")
        await asyncio.sleep(0.6)
        assert fp.calls == 2, "a joined message is no extra pass"
    finally:
        fake_providers.pop(aid, None)


async def test_a_chain_that_ends_with_a_held_message_wakes_once(
    seeded_kernel, fake_providers
):
    aid = "oc_wake"
    fp = _Script([DONE, DONE], delay=0.3)
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        first = _send(seeded_kernel, aid, "one", "o")
        await _settle(fp, 1)
        await asyncio.sleep(0.1)
        second = _send(seeded_kernel, aid, "two", "t")
        await asyncio.sleep(0.1)
        assert not second.done(), "a held send waits for its answer"
        await first
        await _settle(fp, 2)
        two = (await second)[0]["result"]
        assert "response" in two
        assert "arrived while you were working" in fp.incoming[1]
        assert _read(fp, 1, "two")
        await asyncio.sleep(0.6)
        assert fp.calls == 2, "exactly one wake"
    finally:
        fake_providers.pop(aid, None)


async def test_stop_ends_the_chain_and_wakes_nothing(seeded_kernel, fake_providers):
    aid = "oc_stop"
    fp = _Script([DONE], delay=0.5)
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        first = _send(seeded_kernel, aid, "one", "o")
        await _settle(fp, 1)
        await asyncio.sleep(0.1)
        second = _send(seeded_kernel, aid, "two", "t")
        await asyncio.sleep(0.1)
        await seeded_kernel.send([{"target": aid, "payload": {"type": "stop"}}])
        await first
        out = (await second)[0]["result"]
        assert out["cancelled"] is True
        await asyncio.sleep(0.8)
        assert fp.calls == 1, "stop woke the held message"
    finally:
        fake_providers.pop(aid, None)


async def test_a_held_send_returns_the_answer_of_the_pass_that_reads_it(
    seeded_kernel, fake_providers
):
    aid = "oc_answer"
    fp = _Script([DONE, "ANSWER-OF-THE-READING-PASS " + DONE], delay=0.3)
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        first = _send(seeded_kernel, aid, "one", "o")
        await _settle(fp, 1)
        await asyncio.sleep(0.1)
        second = _send(seeded_kernel, aid, "two", "t")
        await first
        out = (await second)[0]["result"]
        assert "ANSWER-OF-THE-READING-PASS" in out["response"]
    finally:
        fake_providers.pop(aid, None)


async def test_a_message_held_on_a_chain_that_just_ended_wakes_it(
    seeded_kernel, fake_providers, monkeypatch
):
    """The chain ends while the held message's row is being stored: the late push
    must revive the chain and wake it, or the sender waits out SEND_TIMEOUT."""
    aid = "oc_dead"
    fp = _Script([DONE, DONE], delay=0.3)
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    monkeypatch.setattr(core, "SEND_TIMEOUT", 3.0)
    real_save = core._save_history

    async def slow_save(self_id, kernel, rows):
        # the row save for the held message outlasts the first pass, which ends
        # the chain (nothing held yet) while this await is still running
        if any("two" in str(r.get("content")) for r in rows):
            await asyncio.sleep(0.8)
        await real_save(self_id, kernel, rows)

    monkeypatch.setattr(core, "_save_history", slow_save)
    try:
        first = _send(seeded_kernel, aid, "one", "o")
        await _settle(fp, 1)
        t0 = time.monotonic()
        second = _send(seeded_kernel, aid, "two", "t")
        await first
        out = (await second)[0]["result"]
        assert "response" in out, f"held send was not answered: {out}"
        assert time.monotonic() - t0 < 3.0, "answered only after SEND_TIMEOUT"
        await _settle(fp, 2)
        assert "arrived while you were working" in fp.incoming[1]
        assert _read(fp, 1, "two")
        await asyncio.sleep(0.3)
        assert fp.calls == 2, "exactly one wake"
        chain = core._chain_of(aid)
        assert chain["alive"] is False
        assert chain["held"] == []
    finally:
        fake_providers.pop(aid, None)
        monkeypatch.setattr(core, "_save_history", real_save)


async def test_a_late_push_revives_the_dead_chain_with_a_token(
    seeded_kernel, monkeypatch
):
    aid = "oc_late_push"
    _agent(seeded_kernel, aid)
    woken = []
    monkeypatch.setattr(
        core,
        "_deliver_soon",
        lambda kernel, target, text, client_id, system_prompt, **kw: woken.append(
            (target, text, client_id, system_prompt)
        ),
    )
    chain = core._chain_of(aid)
    chain["alive"] = False
    chain["token"] = ""
    chain["held"] = []
    fut = asyncio.get_running_loop().create_future()
    core._push_held_or_wake(seeded_kernel, aid, "h", fut, None)
    assert chain["alive"] is True
    assert chain["token"] != ""
    assert len(woken) == 1, "the late message was not handed to a wake"
    assert woken[0][0] == aid
    assert woken[0][2] == "h"
    # the stub wake takes nothing: the message stays held until a pass takes it
    assert len(chain["held"]) == 1
    assert not fut.done()
    core._chains.pop(aid, None)


async def test_a_push_onto_a_live_chain_does_not_wake(seeded_kernel, monkeypatch):
    aid = "oc_live_push"
    _agent(seeded_kernel, aid)
    woken = []
    monkeypatch.setattr(
        core,
        "_deliver_soon",
        lambda *a, **kw: woken.append(a),
    )
    chain = core._chain_of(aid)
    chain["alive"] = True
    chain["token"] = "tok"
    chain["held"] = []
    fut = asyncio.get_running_loop().create_future()
    core._push_held_or_wake(seeded_kernel, aid, "h", fut, None)
    assert chain["alive"] is True
    assert chain["token"] == "tok"
    assert len(chain["held"]) == 1
    assert woken == [], "a push onto a live chain woke it twice"
    core._chains.pop(aid, None)
