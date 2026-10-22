"""The end-of-chain contract: DONE is an explicit tool call; silence is nudged.

A CHAIN turn that emits no
call gets ONE runtime reminder; a `{"name":"done"}` call ends the chain
silently. EVERY turn is held to it — a first turn from a person too — so prose with no call is nudged until `done`."""

from _testkit import memory_meta
import asyncio

import pytest

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


async def test_silent_chain_turns_are_nudged_until_a_call_or_done(
    seeded_kernel, fake_providers
):
    """The model is QUERIED UNTIL IT CALLS. A second silent turn never ends the
    chain silently (two prose turns would read as finished with the task
    untouched)."""
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
    fake_providers[aid] = fp
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
        fake_providers.pop(aid, None)


async def test_done_call_ends_the_chain_without_a_nudge(seeded_kernel, fake_providers):
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
    fake_providers[aid] = fp
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
        fake_providers.pop(aid, None)


async def test_a_first_turn_with_no_call_is_nudged_too(seeded_kernel, fake_providers):
    fp = _Script(["just an answer, no calls", DONE])
    aid = "nd_c"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    fake_providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "hi"}}]
        )
        await asyncio.sleep(0.6)
        assert fp.calls == 2, fp.incoming
        assert "[no call was dispatched]" in fp.incoming[-1]
    finally:
        fake_providers.pop(aid, None)


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
    fake_providers,
):
    fp = _Script(['waiting <call name="await"></call>', DONE])
    aid = "nd_aw"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    fake_providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "hi"}}]
        )
        await asyncio.sleep(0.6)
        assert fp.calls == 2, fp.incoming
        assert "[await refused]" in fp.incoming[-1]
        assert "nothing of yours is in flight" in fp.incoming[-1]
    finally:
        fake_providers.pop(aid, None)


async def test_await_with_a_message_queued_stops_quietly_and_the_queue_wakes_it(
    seeded_kernel,
    fake_providers,
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
    fake_providers[aid] = fp
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
        fake_providers.pop(aid, None)


async def test_two_thinking_only_turns_escalate_the_nudge(
    seeded_kernel, fake_providers
):
    """Two dead turns in a row ran nothing: the second nudge escalates."""
    dead = f"{core.THINK_OPEN}hmm{core.THINK_CLOSE}\n\n"
    fp = _Script([dead, dead, DONE])
    aid = "dead_think_esc"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    fake_providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 3)
        assert "were only thinking" not in fp.incoming[1], fp.incoming
        assert "[no progress — 2 answers in a row ran nothing]" in fp.incoming[2], (
            fp.incoming
        )
    finally:
        fake_providers.pop(aid, None)


async def test_a_dead_turn_is_discarded_and_rerolled(seeded_kernel, fake_providers):
    """A turn that is ONLY thinking did not answer: it is never stored and never
    counts as completed — it is rerolled by the no-call reminder, told why
    (a tiny thinking budget, then a clean `stop` with no answer; failing it
    outright would end a long chain)."""
    fp = _Script([f"{core.THINK_OPEN}hmm{core.THINK_CLOSE}\n\n", DONE])
    aid = "dead_think"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    fake_providers[aid] = fp
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
        fake_providers.pop(aid, None)


async def test_a_chain_gives_up_after_max_silent_turns(
    seeded_kernel, monkeypatch, fake_providers
):
    """Silent turns in a row are bounded: at MAX_SILENT_TURNS the chain stops
    reminding and tells its caller the model could not answer."""
    monkeypatch.setattr(core, "MAX_SILENT_TURNS", 3)
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
    fake_providers[aid] = fp
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
        assert reasons[-1][1]["detail"]["error"] == core._gave_up(3), reasons
    finally:
        fake_providers.pop(aid, None)
    assert core._gave_up(10) == (
        "The LLM was not able to answer — 10 turns in a row produced no call. "
        "Reiterate the inquiry."
    ), "python and rust, byte for byte"


async def test_a_chain_gives_up_after_its_misfire_limit(seeded_kernel, fake_providers):
    """A record's `misfire_limit` bounds the silent turns in a row (#203): with 3,
    the chain gives up after 3 and says so in its own words."""
    fp = _Script(["just prose"] * 10)
    aid = "gives_up_ml"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        misfire_limit=3,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    seeded_kernel.ctx.ensure_inbox("watch_ml")
    seeded_kernel.watch(aid, "watch_ml")
    fake_providers[aid] = fp
    try:
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
        assert fp.calls == 3, f"no give-up after 3 silent turns; incoming={fp.incoming}"
        q = seeded_kernel.ctx.inboxes["watch_ml"]
        details = []
        while not q.empty():
            ev = q.get_nowait()
            if ev.get("type") == "status" and ev.get("phase") == "done":
                details.append(ev.get("detail") or {})
        assert details and details[-1]["reason"] == "gave_up", details
        assert details[-1]["silent_turns"] == 3, details
        assert details[-1]["error"] == core._gave_up(3), details
        assert "3 turns in a row" in core._gave_up(3)
    finally:
        fake_providers.pop(aid, None)


class _Reporting(_Script):
    """A provider that reports its usage, as vLLM's final chunk does."""

    async def chat(self, messages, max_tokens=None):
        async for c in super().chat(messages, max_tokens):
            yield c
        self.last_prompt_tokens = 1234
        self.last_completion_tokens = 56


def _done_details(kernel, inbox: str) -> list[dict]:
    q = kernel.ctx.inboxes[inbox]
    out = []
    while not q.empty():
        ev = q.get_nowait()
        if ev.get("type") == "status" and ev.get("phase") == "done":
            out.append(ev.get("detail") or {})
    return out


async def test_the_done_status_carries_the_token_usage(seeded_kernel, fake_providers):
    """A finished generation's `status` `done` carries what a chat needs for
    `tok/s · ctx`: the provider's reported `prompt_tokens` / `completion_tokens`
    (estimated and marked so when it reports none), `gen_seconds`, and the
    `context_window` in force."""
    for aid, fp, estimated in [
        ("usage_rep", _Reporting([_done()]), False),
        ("usage_est", _Script([_done()]), True),
    ]:
        seeded_kernel.create(
            "ollama_backend.tools",
            id=aid,
            context_window=32768,
            **memory_meta(seeded_kernel, aid, {"cli": "*"}),
        )
        inbox = f"watch_{aid}"
        seeded_kernel.ctx.ensure_inbox(inbox)
        seeded_kernel.watch(aid, inbox)
        fake_providers[aid] = fp
        try:
            await seeded_kernel.send(
                [
                    {
                        "target": aid,
                        "payload": {"type": "send", "text": "go", "client_id": "tab1"},
                    }
                ]
            )
            await _settle(fp, 1)
            await asyncio.sleep(0.3)
            ok = [
                d
                for d in _done_details(seeded_kernel, inbox)
                if d.get("reason") == "ok"
            ]
            assert ok, aid
            d = ok[-1]
            assert d["context_window"] == 32768, d
            assert isinstance(d["gen_seconds"], float) and d["gen_seconds"] >= 0, d
            if estimated:
                assert (
                    d["prompt_tokens"] > 0 and d["prompt_tokens_estimated"] is True
                ), d
                assert d["completion_tokens"] > 0, d
                assert d["completion_tokens_estimated"] is True, d
            else:
                assert (d["prompt_tokens"], d["completion_tokens"]) == (1234, 56), d
                assert "prompt_tokens_estimated" not in d, d
                assert "completion_tokens_estimated" not in d, d
        finally:
            fake_providers.pop(aid, None)


@pytest.mark.skip(
    reason="DEFERRED: resume-after-restart is parked "
    "unwired — during a drain the kernel refuses on_cancel's store writes"
)
async def test_a_turn_a_host_stop_cut_is_resumed_once_on_the_next_boot(
    seeded_kernel, fake_providers, monkeypatch
):
    """RESUME AFTER RESTART: the host stops (the kernel drains)
    while a turn is generating — its stopped row is marked `interrupted` with the
    client it streamed to; the next boot runs it again ONCE, as a
    `[resumed after restart]` turn to that client. A cancel by anyone else, or a
    boot with nothing cut, resumes nothing."""
    from ai_core import core

    fp = _Script(["…", DONE], delay=5.0)
    aid = "resumer"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    fake_providers[aid] = fp
    ctx = seeded_kernel.ctx
    try:
        turn = asyncio.ensure_future(
            seeded_kernel.send(
                [
                    {
                        "target": aid,
                        "payload": {
                            "type": "send",
                            "text": "the question",
                            "client_id": "tab7",
                        },
                    }
                ]
            )
        )
        await _settle(fp, 1)
        # a stop waits QUIET_TIMEOUT for in-flight turns; a generation longer than
        # that is cut (here: 0.2 s against a 5 s turn)
        monkeypatch.setattr(ctx, "QUIET_TIMEOUT", 0.2)
        await ctx.shutdown_agents()  # drains: the generating turn is cut
        await asyncio.wait_for(turn, 5)
        ctx.draining = False  # the stop is over; read the store as the next life will
        rows = [r for r in await core._load_history(aid, seeded_kernel)]
        cut = rows[-1]
        assert cut["kind"] == "stopped" and cut["interrupted"] == {
            "client_id": "tab7"
        }, cut
        assert "the host stopping" in cut["content"], cut
        assert rows[-2]["content"] == "the question", rows[-2]
        # the next incarnation: the kernel runs again and the agent boots
        await seeded_kernel.send([{"target": aid, "payload": {"type": "boot"}}])
        await _settle(fp, 2)
        assert fp.incoming[1] == core.RESUMED_NOTICE, fp.incoming
        assert "the question" in [m["content"] for m in fp.seen[1]], fp.seen[1]
        # once: another boot finds the resumed turn newest, not the cut
        await seeded_kernel.send([{"target": aid, "payload": {"type": "boot"}}])
        await asyncio.sleep(0.5)
        assert fp.calls == 2, fp.incoming
    finally:
        ctx.draining = False
        fake_providers.pop(aid, None)


async def test_the_thinking_is_kept_on_its_row_and_never_shown_to_the_model(
    seeded_kernel, fake_providers
):
    """THINKING ON THE ROW: a chat shows a generation's thinking
    collapsed under its answer, after a reload too. The assistant row keeps it as
    `thinking` (a call written there stays undispatched); the model's next view
    carries only the answer; `history` flags the row `has_thinking`."""
    from ai_core import core

    thought = 'weigh it <call name="send"><target_id>x</target_id><type>t</type></call>'
    fp = _Script([f"<think>{thought}</think>the answer", DONE])
    aid = "thinker"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    fake_providers[aid] = fp
    try:
        await seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "go"}}]
        )
        await _settle(fp, 2)  # the answer had no call: the nudge is the 2nd pass
        rows = await core._load_history(aid, seeded_kernel)
        first = next(r for r in rows if r.get("role") == "assistant")
        assert first["content"] == "the answer" and first["thinking"] == thought, first
        assert all("weigh it" not in m["content"] for m in fp.seen[1]), fp.seen[1]
        assert all(set(m) == {"role", "content"} for m in fp.seen[1]), fp.seen[1]
        out = (
            await seeded_kernel.send([{"target": aid, "payload": {"type": "history"}}])
        )[0]["result"]
        flagged = [d for d in out["rows"] if d.get("has_thinking")]
        assert [d["id"] for d in flagged] == [first["id"]], out
    finally:
        fake_providers.pop(aid, None)
