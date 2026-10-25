"""ONE CHAIN PER AGENT (#227): a message that lands while the agent's chain is
alive joins that chain — stored as a row at once, read by the chain's next pass,
no pass of its own. A chain that ends with a message held wakes ONCE for it;
`stop` ends the chain and wakes nothing. Port of the Rust #234 tests."""

from __future__ import annotations

import asyncio
import time

from _testkit import as_agent, memory_meta

from kernel._kernel import _current_origin

from ai_core import core
from ai_core.tool_parse import render_call

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


# ─── a chain that ends without a pass to read its held messages ──────────────
# Twins of the Rust tests in fantastic-ai-core/src/tests.rs.


def _is_held_error(out) -> bool:
    return (
        isinstance(out.get("error"), str)
        and out.get("held") is True
        and "response" not in out
    )


async def _held_behind_a_slow_pass(kernel, fake_providers, aid):
    """Pass 1 is slow; a second send lands while it runs and is held. Returns
    (provider, first send, held send)."""
    fp = _Script([DONE, DONE], delay=1.5)
    _agent(kernel, aid)
    fake_providers[aid] = fp
    first = _send(kernel, aid, "one", "o")
    await _settle(fp, 1)
    second = _send(kernel, aid, "two", "t")
    end = asyncio.get_event_loop().time() + 5
    while len(core._chain_of(aid)["held"]) != 1:
        assert asyncio.get_event_loop().time() < end, "the second send was not held"
        await asyncio.sleep(0.02)
    return fp, first, second


async def test_a_wake_that_cannot_be_delivered_releases_the_held_senders(
    seeded_kernel,
):
    ghost = "no_such_agent_for_the_wake"
    fut = asyncio.get_running_loop().create_future()
    chain = core._chain_of(ghost)
    chain["alive"] = True
    chain["token"] = "tok"
    chain["held"] = [("h", fut)]
    try:
        await core._deliver(
            seeded_kernel,
            ghost,
            "wake",
            "h",
            None,
            born=core._incarnation.get(ghost, 0),
            notice=True,
        )
        out = await asyncio.wait_for(fut, 5)
        assert isinstance(out["error"], str) and out["held"] is True, out
        assert out["client_id"] == "h"
        assert chain["alive"] is False
    finally:
        core._chains.pop(ghost, None)


async def test_a_wake_that_never_runs_releases_the_held_senders_at_the_ceiling(
    seeded_kernel, fake_providers, monkeypatch
):
    aid = "oc_ceiling"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = _Script([])
    monkeypatch.setattr(core, "SEND_TIMEOUT", 0.2)
    fut = asyncio.get_running_loop().create_future()
    chain = core._chain_of(aid)
    chain["alive"] = True
    chain["token"] = "tok"
    chain["held"] = [("h", fut)]
    lock = core._lock_for(aid)
    await lock.acquire()  # the wake cannot take the lock, so it never runs
    try:
        core._chain_ended(seeded_kernel, aid, None)
        out = await asyncio.wait_for(fut, 5)
        assert isinstance(out["error"], str) and out["held"] is True, out
        assert chain["alive"] is False
        assert chain["held"] == []
    finally:
        lock.release()
        await asyncio.sleep(0.1)
        fake_providers.pop(aid, None)


async def test_a_shutdown_releases_the_held_senders(seeded_kernel, fake_providers):
    aid = "oc_shutdown"
    fp, first, second = await _held_behind_a_slow_pass(
        seeded_kernel, fake_providers, aid
    )
    try:
        await core.on_delete(seeded_kernel.ctx.agents[aid])
        out = (await asyncio.wait_for(second, 5))[0]["result"]
        assert _is_held_error(out), out
        assert out["client_id"] == "t"
        assert not core._chains.get(aid, {}).get("alive")
    finally:
        first.cancel()
        fake_providers.pop(aid, None)


async def test_a_context_clear_ends_the_chain_and_releases_the_held(
    seeded_kernel, fake_providers
):
    aid = "oc_clear"
    fp, first, second = await _held_behind_a_slow_pass(
        seeded_kernel, fake_providers, aid
    )
    try:
        await seeded_kernel.send(
            [
                {
                    "target": aid,
                    "payload": {"type": "context_clear", "why": "fresh start"},
                }
            ]
        )
        out = (await asyncio.wait_for(second, 5))[0]["result"]
        assert _is_held_error(out), out
        assert core._chain_of(aid)["alive"] is False
    finally:
        first.cancel()
        fake_providers.pop(aid, None)


async def test_a_reply_in_flight_across_a_stop_runs_nothing(
    seeded_kernel, fake_providers
):
    aid = "oc_inflight"
    fp = _Script([DONE, DONE], delay=1.5)
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        first = _send(seeded_kernel, aid, "one", "o")
        await _settle(fp, 1)

        # a reply routed back to the agent carries the running chain's token
        async def reply():
            # the reply is routed by the agent's own dispatch: origin is the agent
            _current_origin.set(aid)
            with as_agent(seeded_kernel, aid):
                await core._deliver(
                    seeded_kernel, aid, "[reply to 1 · peer]\nanswered", "cli", None
                )

        late = asyncio.ensure_future(reply())
        await asyncio.sleep(0.1)
        await seeded_kernel.send([{"target": aid, "payload": {"type": "stop"}}])
        await first
        await asyncio.wait_for(late, 5)
        end = asyncio.get_event_loop().time() + 5
        while not [
            r
            for r in await core._load_history(aid, seeded_kernel)
            if r.get("kind") == "late"
        ]:
            assert asyncio.get_event_loop().time() < end, "no late row"
            await asyncio.sleep(0.05)
        await asyncio.sleep(0.3)
        assert fp.calls == 1, "the stale reply ran a turn"
    finally:
        fake_providers.pop(aid, None)


async def test_a_self_chain_does_not_deadlock(seeded_kernel, fake_providers):
    aid = "oc_self"
    call = render_call(
        "send",
        {
            "target_id": "kernel_state",
            "payload": {"type": "list_agents"},
            "reply_to": aid,
        },
    )
    fp = _Script([call, call, 'enough <call name="done"></call>'])
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await asyncio.wait_for(
            seeded_kernel.send(
                [{"target": aid, "payload": {"type": "send", "text": "go"}}]
            ),
            5,
        )
        await _settle(fp, 3)
        await asyncio.sleep(0.3)
        assert fp.calls == 3, f"expected 3 passes, got {fp.calls}"
    finally:
        fake_providers.pop(aid, None)


# ─── twins of the Rust chain tests (fantastic-ai-core/src/tests.rs) ──────────


async def test_the_self_chain_carries_no_call_stack(seeded_kernel, fake_providers):
    """An inbound `_call_stack` must not ride the agent's own follow-up pass: a
    propagated stack would refuse the second pass as a cycle."""
    aid = "oc_stack"
    call = render_call(
        "send",
        {
            "target_id": "kernel_state",
            "payload": {"type": "list_agents"},
            "reply_to": "self",
        },
    )
    fp = _Script([call, DONE])
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        out = (
            await asyncio.wait_for(
                seeded_kernel.send(
                    [
                        {
                            "target": aid,
                            "payload": {
                                "type": "send",
                                "text": "go",
                                "_call_stack": ["upstream"],
                            },
                        }
                    ]
                ),
                5,
            )
        )[0]["result"]
        assert "cycle" not in out, out
        await _settle(fp, 2)
        await asyncio.sleep(0.3)
        assert fp.calls == 2, "the follow-up never ran as its own pass"
        assert not any("cycle detected" in str(m) for m in fp.seen[1]), fp.seen[1]
    finally:
        fake_providers.pop(aid, None)


async def test_a_reboot_ends_the_chain_and_releases_the_held(
    seeded_kernel, fake_providers
):
    aid = "oc_reboot"
    fp, first, second = await _held_behind_a_slow_pass(
        seeded_kernel, fake_providers, aid
    )
    try:
        await seeded_kernel.send([{"target": aid, "payload": {"type": "boot"}}])
        out = (await asyncio.wait_for(second, 5))[0]["result"]
        assert _is_held_error(out), out
        assert out["client_id"] == "t"
        assert core._chain_of(aid)["alive"] is False
        await first
        await asyncio.sleep(0.4)
        assert fp.calls == 1, "a reboot woke the held message"
    finally:
        fake_providers.pop(aid, None)


async def _late_rows(kernel, aid) -> list[dict]:
    return [r for r in await core._load_history(aid, kernel) if r.get("kind") == "late"]


async def test_a_routed_reply_carries_the_token_read_when_it_was_decided(
    seeded_kernel, fake_providers
):
    """The chain token is read when the delivery is DECIDED: a newer chain that
    begins before the spawned delivery runs must not adopt the old chain's reply."""
    aid = "oc_tokwhen"
    fp = _Script([DONE])
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    chain = core._chain_of(aid)
    chain["alive"] = True
    chain["token"] = "old"
    chain["held"] = []
    try:
        core._deliver_soon(seeded_kernel, aid, "an old reply", "cli", None, notice=True)
        # A NEW chain began before the delivery ran (nothing awaited in between).
        chain["token"] = "new"
        end = asyncio.get_event_loop().time() + 10
        while not (chain["held"] or await _late_rows(seeded_kernel, aid) or fp.calls):
            assert asyncio.get_event_loop().time() < end, "the old reply went nowhere"
            await asyncio.sleep(0.02)
        assert fp.calls == 0, "the old reply ran as a turn of the new chain"
    finally:
        core._end_chain(aid)
        await asyncio.sleep(0.1)
        core._chains.pop(aid, None)
        fake_providers.pop(aid, None)


async def test_an_await_reply_after_a_new_chain_began_is_held_not_dropped(
    seeded_kernel, fake_providers
):
    """A reply to an `await`ed call is routed UNTOKENED (the replier is the origin),
    so a new chain does not turn it into a `late` row: it is held for that chain's
    next pass."""
    aid = "oc_awaitnew"
    fp = _Script([DONE])
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        callable_by={"oc_replier": "*"},
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    seeded_kernel.create("", id="oc_replier", can_call={aid: "*"})
    fake_providers[aid] = fp
    chain = core._chain_of(aid)
    chain["alive"] = True
    chain["token"] = "new"
    chain["held"] = []

    async def reply():
        with as_agent(seeded_kernel, "oc_replier"):
            return await seeded_kernel.send(
                [
                    {
                        "target": aid,
                        "payload": {
                            "type": "send",
                            "text": "[reply to 1 · peer]\nanswered",
                            "client_id": "cli",
                        },
                    }
                ]
            )

    late = asyncio.ensure_future(reply())
    try:
        end = asyncio.get_event_loop().time() + 5
        while not (chain["held"] or fp.calls):
            assert asyncio.get_event_loop().time() < end, "the reply went nowhere"
            await asyncio.sleep(0.02)
        assert len(chain["held"]) == 1, "held for the live chain's next pass"
        assert not await _late_rows(seeded_kernel, aid), (
            "recorded as late, not acted on"
        )
        assert fp.calls == 0
    finally:
        core._end_chain(aid)
        await asyncio.wait_for(late, 5)
        core._chains.pop(aid, None)
        fake_providers.pop(aid, None)


async def test_an_inquiry_answer_obeys_the_hold(seeded_kernel, fake_providers):
    """An answer under an owed inquiry pid that lands mid-chain is HELD like any
    message and read by the chain's next pass — never a chain of its own."""
    aid = "oc_inqhold"
    fp = _Script([DONE, DONE], delay=0.8)
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        first = _send(seeded_kernel, aid, "one", "o")
        await _settle(fp, 1)
        core._awaiting.setdefault(aid, set()).add("p9")
        second = _send(seeded_kernel, aid, "[reply to p9 · peer inquiry]\nyes", "t")
        end = asyncio.get_event_loop().time() + 5
        while len(core._chain_of(aid)["held"]) != 1:
            assert asyncio.get_event_loop().time() < end, "the answer was not held"
            await asyncio.sleep(0.02)
        assert not second.done(), "the answer ran a chain of its own"
        assert "p9" not in core._awaiting.get(aid, set()), "the pid is still owed"
        await first
        out = (await asyncio.wait_for(second, 5))[0]["result"]
        assert "response" in out, out
        await asyncio.sleep(0.4)
        assert fp.calls == 2, "the held answer is read by exactly one wake pass"
        assert _read(fp, 1, "yes")
    finally:
        core._awaiting.pop(aid, None)
        fake_providers.pop(aid, None)
