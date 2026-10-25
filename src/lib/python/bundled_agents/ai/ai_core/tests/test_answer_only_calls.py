"""ONLY THE ANSWER ACTS.

A `<call>` written inside `<think>…</think>` is thinking — a draft, an illustration,
a plan — never an action: not dispatched, not stored. A call in thinking is often a
copy of an answer call (dispatching both runs the action twice) or a draft the model
never wrote in its answer.
"""

from __future__ import annotations

import asyncio

from _testkit import memory_meta
from ai_core.tool_parse import render_call

AID = "ao_ai"
DONE = '<call name="done"></call>'


class _Script:
    """A scripted mind: yields the queued replies, records every incoming."""

    def __init__(self, turns):
        self.turns = list(turns)
        self.incoming: list[str] = []

    async def chat(self, messages, max_tokens=None):
        self.incoming.append(messages[-1]["content"])
        yield self.turns.pop(0) if self.turns else DONE


def _reflect(target: str = AID) -> str:
    return render_call("send", {"target_id": target, "payload": {"type": "reflect"}})


def _status() -> str:
    return render_call("send", {"target_id": AID, "payload": {"type": "status"}})


async def _drive(k, fake_providers, turns, want: int) -> list[str]:
    s = _Script(turns)
    k.create(
        "ollama_backend.tools",
        id=AID,
        context_window=32768,
        **memory_meta(k, AID, {"cli": "*"}),
    )
    fake_providers[AID] = s
    try:
        await k.send([{"target": AID, "payload": {"type": "send", "text": "go"}}])
        for _ in range(250):
            if len(s.incoming) >= want:
                break
            await asyncio.sleep(0.02)
        await asyncio.sleep(0.1)
        return s.incoming
    finally:
        fake_providers.pop(AID, None)


async def test_a_call_in_thinking_and_answer_dispatches_once(
    seeded_kernel, fake_providers
):
    """The `fanout` shape: drafted in the thought, written again in the answer."""
    got = await _drive(
        seeded_kernel,
        fake_providers,
        [f"<think>the shape is {_reflect()} — do it</think>{_reflect()}", DONE],
        2,
    )
    assert got[1].count("[reply to ") == 1, got[1]


async def test_thinking_is_streamed_raw_but_neither_dispatched_nor_persisted(seeded_kernel, fake_providers):
    got = await _drive(
        seeded_kernel,
        fake_providers,
        [f"<think>maybe {_status()} — no</think>{_reflect()}", DONE],
        2,
    )
    assert got[1].count("[reply to ") == 1 and " reflect" in got[1], got[1]
    assert " status" not in got[1].split("\n", 1)[0], got[1]


async def test_calls_only_in_thinking_make_a_dead_turn(seeded_kernel, fake_providers):
    """Nothing of it runs; the empty answer is rerolled with the no-answer line."""
    got = await _drive(
        seeded_kernel, fake_providers, [f"<think>{_reflect()}</think>", DONE], 2
    )
    assert "[reply to " not in got[1], got[1]
    assert got[1].startswith("[no answer] Your last turn produced only thinking"), got[
        1
    ]


async def test_done_inside_thinking_does_not_end_the_chain(
    seeded_kernel, fake_providers
):
    got = await _drive(
        seeded_kernel, fake_providers, [f"<think>{DONE}</think>I am finished.", DONE], 2
    )
    assert got[1].startswith("[no call was dispatched]"), got[1]


async def test_an_unclosed_thought_with_a_call_does_not_dispatch(
    seeded_kernel, fake_providers
):
    got = await _drive(
        seeded_kernel, fake_providers, [f"<think>plan {_reflect()} and more", DONE], 2
    )
    assert "[reply to " not in got[1], got[1]
    assert got[1].startswith("[no answer]"), got[1]
