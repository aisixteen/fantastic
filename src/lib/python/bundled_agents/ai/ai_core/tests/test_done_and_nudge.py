"""The end-of-chain contract: DONE is an explicit tool call; silence is nudged.

Reinstated 2026-09-01 (the no-nudge experiment ended when `repo_map:swift`
twice stopped mid-task with a partial artifact): a CHAIN turn that emits no
call gets ONE runtime reminder; a `{"name":"done"}` call ends the chain
silently; a chat turn (prose to a person) is never nudged."""

import asyncio

from ai_core import core


class _Script:
    """A scripted mind: yields the queued replies, records every incoming."""

    def __init__(self, turns):
        self.turns = list(turns)
        self.calls = 0
        self.incoming: list[str] = []

    async def chat(self, messages):
        self.calls += 1
        self.incoming.append(messages[-1]["content"])
        out = self.turns.pop(0) if self.turns else "…nothing left…"
        yield out


def _call(payload: dict) -> str:
    import json

    return (
        "<tool_call>"
        + json.dumps({"name": "send", "arguments": {"target_id": "echo", "payload": payload, "reply_to": "self"}})
        + "</tool_call>"
    )


DONE = '<tool_call>{"name":"done"}</tool_call>'


async def _settle(fp, want, timeout=5.0):
    t0 = asyncio.get_event_loop().time()
    while fp.calls < want:
        assert asyncio.get_event_loop().time() - t0 < timeout, (
            f"stalled at {fp.calls} passes; incoming={fp.incoming}"
        )
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.15)


async def test_silent_chain_turn_is_nudged_once_then_ends(seeded_kernel):
    fp = _Script(
        [
            _call({"type": "reflect"}),  # turn 1: a real call → chain
            "thinking out loud, no envelope",  # turn 2: SILENT chain turn → nudge
            "still just prose",  # turn 3: answers the nudge silently → chain ends
        ]
    )
    aid = "nd_a"
    seeded_kernel.create(
        "ollama_backend.tools", id=aid, context_window=32768,
        can_call={"cli": "*"},
    )
    core._providers[aid] = fp
    try:
        # target `cli` exists in seeded_kernel and accepts $parent? cli is root's
        # child; grant both ends explicitly instead: reflect the KERNEL (always
        # reachable — kernel verbs are the root's).
        fp.turns[0] = _call({"type": "reflect"}).replace('"echo"', '"kernel"')
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 3)
        assert fp.calls == 3, fp.incoming
        assert fp.incoming[2].startswith("[no call was dispatched]"), fp.incoming[2]
        # the nudged-silent turn ended the chain — no fourth pass
        await asyncio.sleep(0.3)
        assert fp.calls == 3
    finally:
        core._providers.pop(aid, None)


async def test_done_call_ends_the_chain_without_a_nudge(seeded_kernel):
    fp = _Script(
        [
            _call({"type": "reflect"}).replace('"echo"', '"kernel"'),
            "finished. " + DONE,  # turn 2: explicit done → silent end
        ]
    )
    aid = "nd_b"
    seeded_kernel.create(
        "ollama_backend.tools", id=aid, context_window=32768, can_call={"cli": "*"}
    )
    core._providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 2)
        await asyncio.sleep(0.3)
        assert fp.calls == 2, fp.incoming
        assert not any(
            m.startswith("[no call was dispatched]") for m in fp.incoming
        ), fp.incoming
    finally:
        core._providers.pop(aid, None)


async def test_chat_turn_is_never_nudged(seeded_kernel):
    fp = _Script(["just an answer, no calls"])
    aid = "nd_c"
    seeded_kernel.create(
        "ollama_backend.tools", id=aid, context_window=32768, can_call={"cli": "*"}
    )
    core._providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "hi"}}]
        )
        await asyncio.sleep(0.4)
        assert fp.calls == 1, fp.incoming
    finally:
        core._providers.pop(aid, None)


async def test_a_question_turn_is_never_nudged(seeded_kernel):
    """Ask + stop is the OTHER legitimate silent end — the human's answer
    resumes the chain, so the runtime must not stomp the wait with a nudge."""
    fp = _Script(
        [
            _call({"type": "reflect"}).replace('"echo"', '"kernel"'),
            'I need a decision.\n{"type":"question","request_id":"q1","kind":"text","question":"which?"}',
        ]
    )
    aid = "nd_q"
    seeded_kernel.create(
        "ollama_backend.tools", id=aid, context_window=32768, can_call={"cli": "*"}
    )
    core._providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 2)
        await asyncio.sleep(0.4)
        assert fp.calls == 2, fp.incoming
        assert not any(
            m.startswith("[no call was dispatched]") for m in fp.incoming
        ), fp.incoming
    finally:
        core._providers.pop(aid, None)
