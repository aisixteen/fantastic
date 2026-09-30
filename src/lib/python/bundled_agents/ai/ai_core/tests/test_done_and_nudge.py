"""The end-of-chain contract: DONE is an explicit tool call; silence is nudged.

Reinstated 2026-09-01 (the no-nudge experiment ended when `repo_map:swift`
twice stopped mid-task with a partial artifact): a CHAIN turn that emits no
call gets ONE runtime reminder; a `{"name":"done"}` call ends the chain
silently. EVERY turn is held to it — a first turn from a person too (2026-09-09:
"all is tool calls") — so prose with no call is nudged until `done`."""

from _testkit import memory_meta
import asyncio

from ai_core import core


class _Script:
    """A scripted mind: yields the queued replies, records every incoming."""

    def __init__(self, turns, delay: float = 0.0):
        self.turns = list(turns)
        self.calls = 0
        self.incoming: list[str] = []
        self.seen: list[list[dict]] = []  # every pass's whole message list
        self.delay = delay  # a slow first turn, so a second send can queue behind it

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.incoming.append(messages[-1]["content"])
        self.seen.append(list(messages))
        if self.calls == 1 and self.delay:
            await asyncio.sleep(self.delay)
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
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
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


async def test_done_call_ends_the_chain_without_a_nudge(seeded_kernel):
    fp = _Script(
        [
            _call({"type": "reflect"}).replace('"echo"', '"kernel"'),
            "finished. " + DONE,  # turn 2: explicit done → silent end
        ]
    )
    aid = "nd_b"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
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
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
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


async def test_await_with_nothing_in_flight_is_refused_and_told_what_to_do(
    seeded_kernel,
):
    fp = _Script(['waiting <call name="await"></call>', DONE])
    aid = "nd_aw"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    core._providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "hi"}}]
        )
        await asyncio.sleep(0.6)
        assert fp.calls == 2, fp.incoming
        assert "[await refused]" in fp.incoming[-1]
        assert "nothing of yours is in flight" in fp.incoming[-1]
    finally:
        core._providers.pop(aid, None)


async def test_await_with_a_message_queued_stops_quietly_and_the_queue_wakes_it(
    seeded_kernel,
):
    """The honest wait: something is queued for the agent, so `await` ends the turn
    with no nudge and no refusal; the queued message is the next turn."""
    fp = _Script(['waiting <call name="await"></call>', DONE], delay=0.3)
    aid = "nd_aw2"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    core._providers[aid] = fp
    try:
        first = asyncio.create_task(
            seeded_kernel.send(
                [{"target": aid, "payload": {"type": "send", "text": "one"}}]
            )
        )
        await asyncio.sleep(0.1)
        second = asyncio.create_task(
            seeded_kernel.send(
                [{"target": aid, "payload": {"type": "send", "text": "two"}}]
            )
        )
        await asyncio.gather(first, second)
        await asyncio.sleep(0.4)
        assert fp.calls == 2, fp.incoming
        assert not any("[await refused]" in m for m in fp.incoming)
        assert not any("[no call was dispatched]" in m for m in fp.incoming)
        assert fp.incoming[-1].endswith("two")
    finally:
        core._providers.pop(aid, None)


async def test_a_dead_turn_is_discarded_and_rerolled(seeded_kernel):
    """A turn that is ONLY thinking did not answer: it is never stored and never
    counts as completed — it is rerolled by the no-call reminder, told why
    (measured on vLLM 2026-09-30: an 8-token thinking budget, then a clean
    `stop` with no answer; failing it outright ended a 30-pass chain)."""
    fp = _Script([f"{core.THINK_OPEN}hmm{core.THINK_CLOSE}\n\n", DONE])
    aid = "dead_think"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    core._providers[aid] = fp
    try:
        out = (
            await seeded_kernel.send(
                [{"target": aid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]["result"]
        assert out.get("dead_turn") is True, out
        await _settle(fp, 2)
        assert fp.incoming[1].startswith("[no answer]"), fp.incoming
        assert "[no call was dispatched]" in fp.incoming[1], fp.incoming
        # the turn the dead one answered is KEPT (a reply batch must not vanish);
        # only its empty answer is dropped
        seen = [m.get("content") for m in fp.seen[1]]
        assert any(
            "go" == c or (isinstance(c, str) and c.endswith("go")) for c in seen
        ), seen
        assert not any(
            m.get("role") == "assistant" and not (m.get("content") or "").strip()
            for m in fp.seen[1]
        ), fp.seen[1]
    finally:
        core._providers.pop(aid, None)


async def test_a_chain_gives_up_after_max_silent_turns(seeded_kernel, monkeypatch):
    """Silent turns in a row are bounded: at MAX_SILENT_TURNS the chain stops
    reminding and tells its caller the model could not answer."""
    monkeypatch.setattr(core, "MAX_SILENT_TURNS", 3)
    monkeypatch.setattr(
        core,
        "_GAVE_UP",
        "The LLM was not able to answer — 3 turns in a row "
        "produced no call. Reiterate the inquiry.",
    )
    fp = _Script(["just prose"] * 10)
    aid = "gives_up"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    seeded_kernel.ctx.ensure_inbox("watch_gu")
    seeded_kernel.watch(aid, "watch_gu")
    core._providers[aid] = fp
    try:
        # a browser-style client: its events land on the agent's own inbox,
        # where a watcher sees them (`cli` events go to the terminal instead)
        await seeded_kernel.send(
            [
                {
                    "target": aid,
                    "payload": {"type": "send", "text": "go", "client_id": "tab1"},
                }
            ]
        )
        await _settle(fp, 3)
        await asyncio.sleep(0.5)
        assert fp.calls == 3, f"no reminder after the cap; incoming={fp.incoming}"
        q = seeded_kernel.ctx.inboxes["watch_gu"]
        reasons = []
        while not q.empty():
            ev = q.get_nowait()
            if ev.get("type") == "status" and ev.get("phase") == "done":
                reasons.append(((ev.get("detail") or {}).get("reason"), ev))
        assert reasons and reasons[-1][0] == "gave_up", reasons
        assert "Reiterate the inquiry" in reasons[-1][1]["detail"]["error"], reasons
    finally:
        core._providers.pop(aid, None)
