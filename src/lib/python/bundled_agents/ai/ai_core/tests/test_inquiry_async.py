"""INQUIRY, FULLY ASYNC.

An `inquiry` sent to an AI is ACCEPTED at once (`{accepted, re, request_id}`) and
runs as its own chain on the receiver. The answer the receiver gives with
`inquiry_answer` in ANY turn of that chain — after whatever work it needs — reaches
the asker LATER as a message under the inquiry's pid, `[reply to <pid> · <who>
inquiry]`. A chain that ends without answering tells the asker so. Nothing waits:
the asker keeps working or `await`s, and its open inquiry counts as work in flight.
The answer is never read off the receiver's FIRST turn only, so work before
answering is allowed.
"""

from __future__ import annotations

import asyncio

from _testkit import memory_meta
from ai_core.tool_parse import render_call

DONE = '<call name="done"></call>'
AWAIT = '<call name="await"></call>'


class _Script:
    """A scripted mind: yields the queued replies, records every incoming."""

    def __init__(self, turns, hold: float = 0.0):
        self.turns = list(turns)
        self.incoming: list[str] = []
        self.hold = hold

    async def chat(self, messages, max_tokens=None):
        self.incoming.append(messages[-1]["content"])
        if self.hold:
            await asyncio.sleep(self.hold)
        yield self.turns.pop(0) if self.turns else DONE


def _ask(target: str, text: str = "ship?") -> str:
    return render_call(
        "send", {"target_id": target, "payload": {"type": "inquiry", "text": text}}
    )


def _answer(text: str) -> str:
    return render_call("inquiry_answer", {"answer": text})


def _pair(
    k, providers, asker_turns, rcv_turns, *, rcv="ia_rcv", hold=0.2, answer_grant=True
):
    """An asker AI granted `inquiry` on a receiver AI that admits it — and, both
    halves, the receiver's `send` back to the asker: the answer is its own send."""
    a, r = _Script(asker_turns), _Script(rcv_turns, hold=hold)
    back = {"ia_asker": ["send"]} if answer_grant else {}
    k.create(
        "ollama_backend.tools",
        id=rcv,
        context_window=32768,
        callable_by={"ia_asker": "*"},
        **memory_meta(k, rcv, {"cli": "*", **back}),
    )
    k.create(
        "ollama_backend.tools",
        id="ia_asker",
        context_window=32768,
        callable_by={rcv: ["send"]} if answer_grant else {},
        **memory_meta(k, "ia_asker", {rcv: "*", "cli": "*"}),
    )
    providers["ia_asker"], providers[rcv] = a, r
    return a, r


async def _until(cond, timeout=5.0):
    t0 = asyncio.get_event_loop().time()
    while not cond():
        assert asyncio.get_event_loop().time() - t0 < timeout, "timed out"
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.1)


async def _go(k):
    await k.send(
        [{"target": "ia_asker", "payload": {"type": "send", "text": "decide"}}]
    )


async def test_the_answer_arrives_later_under_the_inquiry_pid(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    a, r = _pair(k, fake_providers, [_ask("ia_rcv"), AWAIT, DONE], [_answer("ship it")])
    try:
        await _go(k)
        await _until(lambda: len(a.incoming) >= 3)
        accepted = a.incoming[1]
        assert (
            "ia_rcv inquiry" in accepted and "<accepted>true</accepted>" in accepted
        ), accepted
        answer = a.incoming[2]
        # the receiver's own send, so it carries the sender header, then the tag
        assert answer.startswith("[from ia_rcv]") and "[reply to " in answer, answer
        assert " ia_rcv inquiry]" in answer, answer
        assert "<answer>ship it</answer>" in answer, answer
        assert not any("[await: nothing to wait for]" in m for m in a.incoming), (
            a.incoming
        )
        # the receiver saw the question with its head: who asked, under which pid
        assert r.incoming[0].startswith("[inquiry from ia_asker · re "), r.incoming[0]
    finally:
        fake_providers.pop("ia_asker", None)
        fake_providers.pop("ia_rcv", None)


async def test_the_receiver_may_work_for_several_turns_first(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    look = render_call("send", {"target_id": "ia_rcv", "payload": {"type": "reflect"}})
    a, r = _pair(
        k, fake_providers, [_ask("ia_rcv"), AWAIT, DONE], [look, _answer("ship it")]
    )
    try:
        await _go(k)
        await _until(lambda: len(a.incoming) >= 3)
        assert len(r.incoming) == 2, r.incoming  # it worked a turn, then answered
        assert "<answer>ship it</answer>" in a.incoming[2], a.incoming
    finally:
        fake_providers.pop("ia_asker", None)
        fake_providers.pop("ia_rcv", None)


async def test_ending_without_an_answer_tells_the_asker(seeded_kernel, fake_providers):
    k = seeded_kernel
    a, _ = _pair(k, fake_providers, [_ask("ia_rcv"), AWAIT, DONE], [DONE])
    try:
        await _go(k)
        await _until(lambda: len(a.incoming) >= 3)
        assert "ia_rcv ended its run without answering" in a.incoming[2], a.incoming
    finally:
        fake_providers.pop("ia_asker", None)
        fake_providers.pop("ia_rcv", None)


async def test_a_placeholder_is_refused_and_the_inquiry_stays_open(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    a, r = _pair(
        k,
        fake_providers,
        [_ask("ia_rcv"), AWAIT, DONE],
        [_answer("..."), _answer("ship it")],
    )
    try:
        await _go(k)
        await _until(lambda: len(a.incoming) >= 3)
        assert "PLACEHOLDER" in r.incoming[1], r.incoming
        assert "<answer>ship it</answer>" in a.incoming[2], a.incoming
    finally:
        fake_providers.pop("ia_asker", None)
        fake_providers.pop("ia_rcv", None)


async def test_an_answer_the_asker_does_not_admit_is_told_to_the_receiver(
    seeded_kernel, fake_providers
):
    """The answer is the receiver's own send: without the
    reverse grant it is refused, and the RECEIVER is told — never dropped."""
    k = seeded_kernel
    a, r = _pair(
        k,
        fake_providers,
        [_ask("ia_rcv"), DONE],
        [_answer("ship it"), DONE],
        answer_grant=False,
    )
    try:
        await _go(k)
        await _until(lambda: len(r.incoming) >= 2)
        assert "your inquiry answer did not reach ia_asker" in r.incoming[1], r.incoming
        assert not any("ship it" in m for m in a.incoming), a.incoming
    finally:
        fake_providers.pop("ia_asker", None)
        fake_providers.pop("ia_rcv", None)


async def test_an_answer_with_nothing_open_is_told_so(seeded_kernel, fake_providers):
    k = seeded_kernel
    s = _Script([_answer("ship it"), DONE])
    k.create(
        "ollama_backend.tools",
        id="ia_lone",
        context_window=32768,
        **memory_meta(k, "ia_lone", {"cli": "*"}),
    )
    fake_providers["ia_lone"] = s
    try:
        await k.send([{"target": "ia_lone", "payload": {"type": "send", "text": "hi"}}])
        await _until(lambda: len(s.incoming) >= 2)
        assert "inquiry_answer: nothing asked you in this run" in s.incoming[1], (
            s.incoming
        )
    finally:
        fake_providers.pop("ia_lone", None)


# ─── twins of the Rust owed-pid tests (fantastic-ai-core/src/tests.rs) ───────


async def test_an_answer_after_registration_still_clears_the_owed_pid(
    seeded_kernel, fake_providers
):
    """The normal order: the pid is owed first, then the answer under it clears it."""
    from ai_core import core

    aid = "ia_owed"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    fake_providers[aid] = _Script([DONE])
    try:
        core._awaiting.setdefault(aid, set()).add("7")
        await seeded_kernel.send(
            [
                {
                    "target": aid,
                    "payload": {
                        "type": "send",
                        "text": "[reply to 7 · peer inquiry]\nyes",
                    },
                }
            ]
        )
        assert "7" not in core._awaiting.get(aid, set()), core._awaiting.get(aid)
    finally:
        core._awaiting.pop(aid, None)
        fake_providers.pop(aid, None)


async def test_an_instant_answer_wakes_the_asker_once_and_leaves_nothing_owed(
    seeded_kernel, fake_providers
):
    """A receiver that answers with ZERO hold (the other tests hold 0.2s): the asker
    wakes once with the answer, is told nothing "to wait for", and owes nothing."""
    from ai_core import core

    k = seeded_kernel
    a, r = _pair(
        k, fake_providers, [_ask("ia_rcv"), AWAIT, DONE], [_answer("ship it")], hold=0.0
    )
    try:
        await _go(k)
        await _until(lambda: len(a.incoming) >= 3)
        await asyncio.sleep(0.4)
        third = a.incoming[2]
        assert third.count("<answer>ship it</answer>") == 1, third
        assert "nothing to wait for" not in third, third
        assert not core._awaiting.get("ia_asker"), core._awaiting.get("ia_asker")
        assert len(a.incoming) == 3, a.incoming
    finally:
        fake_providers.pop("ia_asker", None)
        fake_providers.pop("ia_rcv", None)
