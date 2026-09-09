"""The end-of-chain contract: DONE is an explicit tool call; silence is nudged.

Reinstated 2026-09-01 (the no-nudge experiment ended when `repo_map:swift`
twice stopped mid-task with a partial artifact): a CHAIN turn that emits no
call gets ONE runtime reminder; a `{"name":"done"}` call ends the chain
silently. EVERY turn is held to it — a first turn from a person too (2026-09-09:
"all is tool calls") — so prose with no call is nudged until `done`."""

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
    from ai_core.tool_parse import render_call

    return render_call(
        "send", {"target_id": "echo", "payload": payload, "reply_to": "self"}
    )


DONE = '<call name="done"></call>'


def _done() -> str:
    return DONE


async def _settle(fp, want, timeout=5.0):
    t0 = asyncio.get_event_loop().time()
    while fp.calls < want:
        assert asyncio.get_event_loop().time() - t0 < timeout, (
            f"stalled at {fp.calls} passes; incoming={fp.incoming}"
        )
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.15)


async def test_silent_chain_turns_are_nudged_until_a_call_or_done(seeded_kernel):
    """The model is QUERIED UNTIL IT CALLS. A second silent turn used to end the
    chain silently (measured 2026-09-01, E30 subai: two prose turns and the run
    read as finished with the task untouched)."""
    fp = _Script(
        [
            _call({"type": "reflect"}),  # turn 1: a real call → chain
            "thinking out loud, no envelope",  # turn 2: SILENT → nudge
            "still just prose",  # turn 3: STILL silent → nudged AGAIN
            "ok, finishing",  # turn 4: silent → nudged again
            _done(),  # turn 5: done → chain ends
        ]
    )
    aid = "nd_a"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        can_call={"cli": "*"},
    )
    core._providers[aid] = fp
    try:
        fp.turns[0] = _call({"type": "reflect"}).replace('"echo"', '"kernel"')
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 5)
        assert fp.calls == 5, fp.incoming
        for i in (2, 3, 4):
            assert fp.incoming[i].startswith("[no call was dispatched]"), fp.incoming[i]
        await asyncio.sleep(0.3)
        assert fp.calls == 5  # done ended it; nothing reminds after done
    finally:
        core._providers.pop(aid, None)


async def test_nudge_limit_bounds_a_never_calling_chain(seeded_kernel):
    """`nudge_limit` on the record caps consecutive silent turns — an unattended
    kernel must not spin forever on a model that never calls."""
    fp = _Script(
        [
            _call({"type": "reflect"}),
            "prose 1",
            "prose 2",
            "prose 3",
            "prose 4 — would be nudged without the limit",
        ]
    )
    aid = "nd_lim"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        can_call={"cli": "*"},
        nudge_limit=2,
    )
    core._providers[aid] = fp
    try:
        fp.turns[0] = _call({"type": "reflect"}).replace('"echo"', '"kernel"')
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 4)
        await asyncio.sleep(0.3)
        # turn 1 call, turn 2 silent → nudge 1, turn 3 silent → nudge 2, turn 4
        # silent → over the limit, chain let go: exactly 4 passes, never 5.
        assert fp.calls == 4, fp.incoming
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
        assert not any(m.startswith("[no call was dispatched]") for m in fp.incoming), (
            fp.incoming
        )
    finally:
        core._providers.pop(aid, None)


async def test_a_first_turn_with_no_call_is_nudged_too(seeded_kernel):
    fp = _Script(["just an answer, no calls", DONE])
    aid = "nd_c"
    seeded_kernel.create(
        "ollama_backend.tools", id=aid, context_window=32768, can_call={"cli": "*"}
    )
    core._providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "hi"}}]
        )
        await asyncio.sleep(0.6)
        assert fp.calls == 2, fp.incoming
        assert "[no call was dispatched]" in fp.incoming[-1]
    finally:
        core._providers.pop(aid, None)


# THE QUESTION-TURN EXEMPTION IS GONE. `test_a_question_turn_is_never_nudged`
# lived here: a turn whose prose carried `{"type":"inquiry"}` ended the chain,
# and the runtime had to RECOGNISE that text to avoid stomping the wait with a
# nudge. Asking is a CALL now — the model `send`s the human agent and the answer
# is the reply — so such a turn dispatches and is never silent in the first
# place. The behaviour is covered by the ordinary dispatched-turn cases above;
# the human itself is exercised in e2e, where its bundle lives (`e2e/probes/`),
# exactly like the other test-only fixtures.
