"""The call checker runs on the stream (#499): a generation that writes a foreign call
tag is cut there, the rest is never read, and the turn gets the usual correction. Nine
cut turns in a row give up. Rust twin: `tests_stream_cut.rs`."""

import asyncio

from _testkit import memory_meta

from ai_core import core

FOREIGN = 'Sending.\n<invoke name="send">'
DONE = '<call name="done"></call>'
REFLECT = '<call name="send"><target_id>kernel</target_id><type>reflect</type></call>'


def _chars(text):
    return list(text)


class _Stream:
    """A provider: each turn is a list of tokens. Records what it was asked and how
    many tokens were pulled from it."""

    def __init__(self, turns):
        self.turns = [list(t) for t in turns]
        self.calls = 0
        self.incoming: list[str] = []
        self.seen: list[list] = []
        self.pulled = 0

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.incoming.append(messages[-1]["content"])
        self.seen.append(messages)
        for tok in self.turns.pop(0) if self.turns else ["…nothing left…"]:
            self.pulled += 1
            yield tok


async def _settle(fp, want, timeout=10.0):
    t0 = asyncio.get_event_loop().time()
    while fp.calls < want:
        assert asyncio.get_event_loop().time() - t0 < timeout, fp.incoming
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.3)


def _agent(kernel, aid, **extra):
    kernel.create(
        "ai_chat.tools",
        id=aid,
        context_window=32768,
        **extra,
        **memory_meta(kernel, aid, {"cli": "*", "kernel": "*"}),
    )


async def _go(kernel, aid, **extra):
    await kernel.send(
        [
            {
                "target": aid,
                "payload": {"type": "send", "text": "go", **extra},
            }
        ]
    )


async def _said(aid, kernel):
    rows = await core._load_history(aid, kernel)
    return [r for r in rows if r.get("role") == "assistant"]


async def _dones(kernel, watcher):
    q = kernel.ctx.inboxes[watcher]
    out = []
    while not q.empty():
        ev = q.get_nowait()
        if ev.get("type") == "status" and ev.get("phase") == "done":
            out.append(ev)
    return out


async def test_a_looping_foreign_tag_is_cut_and_corrected(seeded_kernel, fake_providers):
    loop = ["\n</invoke"] * 5000
    fp = _Stream([_chars(FOREIGN) + loop, [DONE]])
    aid = "sc_loop"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _go(seeded_kernel, aid)
        await _settle(fp, 2)
        assert fp.pulled < 100, fp.pulled
        said = await _said(aid, seeded_kernel)
        assert said[0]["content"] == FOREIGN, said
        assert "[malformed call — nothing ran]" in fp.incoming[1], fp.incoming[1]
    finally:
        fake_providers.pop(aid, None)


async def test_a_correct_call_stream_is_not_cut(seeded_kernel, fake_providers):
    text = "Done soon.\n" + DONE
    fp = _Stream([_chars(text)])
    aid = "sc_ok"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _go(seeded_kernel, aid)
        await _settle(fp, 1)
        said = await _said(aid, seeded_kernel)
        assert said[0]["content"] == text, said
        assert fp.calls == 1
    finally:
        fake_providers.pop(aid, None)


async def test_thinking_and_prose_are_not_cut(seeded_kernel, fake_providers):
    text = (
        '<think>maybe <invoke name="send"> is it</think>'
        "I should invoke the done call.\n" + DONE
    )
    fp = _Stream([_chars(text)])
    aid = "sc_think"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _go(seeded_kernel, aid)
        await _settle(fp, 1)
        said = await _said(aid, seeded_kernel)
        assert said[0]["content"] == "I should invoke the done call.\n" + DONE, said
    finally:
        fake_providers.pop(aid, None)


async def test_a_cut_after_thinking_keeps_it_in_the_store_not_the_view(
    seeded_kernel, fake_providers
):
    first = _chars("<think>I will use my native tool.</think>" + FOREIGN)
    fp = _Stream([first + ["\n</invoke"] * 2000, [DONE]])
    aid = "sc_keep"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _go(seeded_kernel, aid)
        await _settle(fp, 2)
        said = await _said(aid, seeded_kernel)
        assert said[0]["content"] == FOREIGN, said
        assert said[0]["thinking"] == "I will use my native tool.", said
        seen = str(fp.seen[1])
        assert "Sending." in seen
        assert "native tool" not in seen and "<think>" not in seen, seen
        # the cut answer is its anchor in the next request, at once
        cut = next(m for m in fp.seen[1] if m["role"] == "assistant")["content"]
        assert cut.startswith(
            'Sending.\n[cut · you wrote <invoke name="send"> after thinking ~7 tokens'
            " · nothing ran · ACTION: "
        ), cut
        assert cut.endswith(f" · full: {said[0]['id']}]"), cut
    finally:
        fake_providers.pop(aid, None)


async def test_a_call_that_lost_its_lt_is_not_cut(seeded_kernel, fake_providers):
    fp = _Stream(
        [_chars("<think>x</think>" + REFLECT[1:]), [DONE]]
    )
    aid = "sc_lt"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _go(seeded_kernel, aid)
        await _settle(fp, 2)
        assert "[malformed call" not in fp.incoming[1], fp.incoming[1]
        assert "kernel reflect" in fp.incoming[1], fp.incoming[1]
    finally:
        fake_providers.pop(aid, None)


async def test_nine_cut_turns_in_a_row_give_up(
    seeded_kernel, fake_providers, monkeypatch
):
    fp = _Stream([_chars(FOREIGN)] * 12)
    aid = "sc_nine"
    _agent(seeded_kernel, aid)
    seeded_kernel.ctx.ensure_inbox("watch_sc")
    seeded_kernel.watch(aid, "watch_sc")
    fake_providers[aid] = fp
    told = []
    real = core._deliver_soon

    def recorder(kernel, target, text, *a, **k):
        if target == "cut_peer":
            told.append((target, text))
            return None
        return real(kernel, target, text, *a, **k)

    monkeypatch.setattr(core, "_deliver_soon", recorder)
    try:
        await _go(seeded_kernel, aid, client_id="tab1", send_to="cut_peer")
        await _settle(fp, 9)
        await asyncio.sleep(0.5)
        assert fp.calls == 9, fp.incoming
        dones = await _dones(seeded_kernel, "watch_sc")
        last = dones[-1]["detail"]
        assert last["reason"] == "gave_up", dones
        text = core._cut_gave_up(9)
        assert last["error"] == text, last
        # only the first turn carries `send_to` (the one-shot hand-off); the give-up
        # is told by the status and the inquiry close
        assert told == [("cut_peer", FOREIGN)], told
        assert text == (
            "The LLM was not able to answer — 9 turns in a row were cut at a "
            "foreign call syntax. Reiterate the inquiry."
        )
    finally:
        fake_providers.pop(aid, None)


async def _gave_up_after(kernel, fake_providers, aid, turns, **meta):
    fp = _Stream(turns)
    _agent(kernel, aid, **meta)
    kernel.ctx.ensure_inbox("watch_" + aid)
    kernel.watch(aid, "watch_" + aid)
    fake_providers[aid] = fp
    try:
        await _go(kernel, aid, client_id="tab1")
        await _settle(fp, len(turns))
        await asyncio.sleep(0.5)
        assert fp.calls == len(turns), fp.incoming
        dones = await _dones(kernel, "watch_" + aid)
        return any(d["detail"].get("reason") == "gave_up" for d in dones)
    finally:
        fake_providers.pop(aid, None)


async def test_eight_cuts_then_a_call_reset_the_cut_count(
    seeded_kernel, fake_providers
):
    turns = [_chars(FOREIGN)] * 8 + [[REFLECT]] + [_chars(FOREIGN)] * 8 + [[DONE]]
    assert len(turns) == 18
    assert not await _gave_up_after(seeded_kernel, fake_providers, "sc_reset", turns)


async def test_a_prose_turn_breaks_the_cut_run(seeded_kernel, fake_providers):
    turns = [_chars(FOREIGN)] * 8 + [["just prose"]] + [_chars(FOREIGN)] * 8 + [[DONE]]
    assert len(turns) == 18
    assert not await _gave_up_after(
        seeded_kernel, fake_providers, "sc_prose", turns, misfire_limit=20
    )


async def test_a_quoted_call_in_an_argument_is_not_cut(seeded_kernel, fake_providers):
    text = (
        '<call name="send"><target_id>probe</target_id><type>say</type>'
        '<text>end with <call name="done"></call> then</text></call>'
    )
    fp = _Stream([_chars(text), [DONE]])
    aid = "sc_quoted"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _go(seeded_kernel, aid)
        await _settle(fp, 1)
        said = await _said(aid, seeded_kernel)
        assert said[0]["content"] == text, said
    finally:
        fake_providers.pop(aid, None)
