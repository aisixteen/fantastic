"""STREAMING PARTS: a model's output
streams to its addressees as parts — animation, never action; a receiver can stop it;
a cut pass leaves a marker on top of the context, never a half answer."""

from __future__ import annotations

import asyncio

from _testkit import as_agent, memory_meta, wire

from ai_core import core
from ai_core.stream_parts import PartSplitter, cut_marker, stop_rows

CALL = (
    '<call name="send">\n<target_id>notes</target_id>\n<type>reflect</type>\n'
    "<bytes>a </call> inside</bytes>\n</call>"
)
DONE = '<call name="done"></call>'


# ─── the splitter ────────────────────────────────────────────────────────────


def _run(text: str, n: int) -> tuple[list, PartSplitter]:
    sp = PartSplitter("ai")
    ev: list = []
    for i in range(0, len(text), n):
        ev += sp.feed(text[i : i + n])
    return ev + sp.finish(), sp


def test_parts_are_the_same_at_every_chunk_size():
    text = f"hi <think>plan</think>ok {CALL} after {DONE}"
    for n in (1, 2, 5, len(text)):
        ev, sp = _run(text, n)
        shape = [(e[0], e[1]["kind"]) for e in ev if e[0] != "delta"]
        assert shape == [
            ("start", "text"),
            ("end", "text"),
            ("start", "thinking"),
            ("end", "thinking"),
            ("start", "text"),
            ("end", "text"),
            ("start", "call"),
            ("end", "call"),
            ("start", "text"),
            ("end", "text"),
            ("start", "call"),
            ("end", "call"),
        ], (n, shape)
        call = sp.parts[3]
        assert call["name"] == "send" and call["target"] == "notes"
        body = "".join(e[2] for e in ev if e[0] == "delta" and e[1] is call)
        assert "a </call> inside" in body  # verbatim bytes never close the call


def test_an_open_call_when_the_pass_ends_aborts():
    ev, sp = _run('<call name="send"><target_id>x</target_id><type>wr', 3)
    assert ("abort", "call") in [(e[0], e[1]["kind"]) for e in ev]


def test_a_call_inside_a_thought_is_thinking_text():
    """ONLY THE ANSWER ACTS: a `<call>` inside `<think>` streams
    as thinking — no call part opens."""
    ev, _ = _run(f"<think>let me {DONE} then</think>", 4)
    assert [(e[0], e[1]["kind"]) for e in ev if e[0] != "delta"] == [
        ("start", "thinking"),
        ("end", "thinking"),
    ]
    text = "".join(e[2] for e in ev if e[0] == "delta")
    assert DONE in text, text


def test_the_cut_marker_is_a_marker_not_a_half_answer():
    m = cut_marker(
        'said this <think>hidden</think><call name="send"><target_id>x</target_id>',
        "model stopped generating",
        [{"name": "send", "target": "x"}],
    )
    assert m.splitlines() == [
        "[model stopped generating]",
        "said this",
        "[a send call to x was not sent]",
    ]
    assert (
        cut_marker("<think>hmm", "stopped by rx", [])
        == "[stopped by rx while thinking]"
    )


def test_stop_rows_with_nothing_said_has_no_answer():
    shown, notice = stop_rows("", "rx", [])
    assert shown is None
    assert notice == "[your turn was stopped by rx before you answered]"


def test_stop_rows_while_thinking():
    shown, notice = stop_rows("<" + "think" + ">hmm", "rx", [])
    assert shown is None
    assert notice == "[your turn was stopped by rx while thinking]"


def test_stop_rows_keeps_the_partial_text():
    shown, notice = stop_rows("partial words", "rx", [])
    assert shown == "partial words"
    assert notice == "[your turn was stopped by rx]"


def test_stop_rows_names_each_unsent_call():
    shown, notice = stop_rows(
        "said this " + '<call name="send">' + "<target_id>x</target_id>",
        "rx",
        [{"name": "send", "target": "x"}],
    )
    assert shown == "said this"
    assert notice.splitlines() == [
        "[your turn was stopped by rx]",
        "[a send call to x was not sent]",
    ]


# ─── through an AI agent ─────────────────────────────────────────────────────


class _Slow:
    """A provider that streams each script entry chunk by chunk, slowly."""

    def __init__(self, scripts: list[str], step: float = 0.0):
        self._scripts = list(scripts)
        self.step = step
        self.requests: list[list[dict]] = []

    async def chat(self, messages, max_tokens=None):
        self.requests.append(list(messages))
        text = self._scripts.pop(0) if self._scripts else DONE
        for i in range(0, len(text), 4):
            if self.step:
                await asyncio.sleep(self.step)
            yield text[i : i + 4]


def _ai(k, aid: str) -> None:
    k.create(
        "ollama_backend.tools",
        id=aid,
        endpoint="http://127.0.0.1:9/stub",
        model="stub",
        context_window=32768,
        **memory_meta(k, aid, {"cli": "*"}),
    )


def _drain(k, inbox: str) -> list[dict]:
    q = k.ctx.inboxes[inbox]
    out = []
    while not q.empty():
        out.append(q.get_nowait())
    return out


async def _until(cond, timeout: float = 5.0) -> None:
    end = asyncio.get_event_loop().time() + timeout
    while not cond():
        assert asyncio.get_event_loop().time() < end, "timed out"
        await asyncio.sleep(0.02)


async def test_a_call_part_streams_to_its_addressee(seeded_kernel, fake_providers):
    k = seeded_kernel
    _ai(k, "sp_ai")
    k.create("ram_store.tools", id="notes")
    wire(k, "sp_ai", "notes")
    k.ctx.ensure_inbox("w_notes")
    k.watch("notes", "w_notes")
    fp = fake_providers["sp_ai"] = _Slow([CALL, DONE])
    await k.send([{"target": "sp_ai", "payload": {"type": "send", "text": "go"}}])
    await _until(lambda: len(fp.requests) >= 2)
    evs = [
        e for e in _drain(k, "w_notes") if str(e.get("type", "")).startswith("part_")
    ]
    types = [e["type"] for e in evs]
    assert types[0] == "part_start" and types[-1] == "part_end", types
    assert evs[0]["name"] == "send" and evs[0]["target"] == "notes"
    assert len({e["stream_id"] for e in evs}) == 1
    body = "".join(e["text"] for e in evs if e["type"] == "part_delta")
    assert "a </call> inside" in body


async def test_a_model_that_stops_with_a_block_open_leaves_the_marker_on_top(
    seeded_kernel, fake_providers
):
    """The model stops generating mid-call: the call is NOT dispatched, and the
    conversation's newest assistant row is the cut marker, which the next turn sees."""
    k = seeded_kernel
    _ai(k, "sp_cut")
    k.create("ram_store.tools", id="notes")
    wire(k, "sp_cut", "notes")
    k.ctx.ensure_inbox("w_cut")
    k.watch("notes", "w_cut")
    open_call = '<call name="send"><target_id>notes</target_id><type>refl'
    fp = fake_providers["sp_cut"] = _Slow([open_call, DONE])
    await k.send([{"target": "sp_cut", "payload": {"type": "send", "text": "go"}}])
    await _until(lambda: len(fp.requests) >= 2)
    nxt = fp.requests[1]
    marker = [m["content"] for m in nxt if m["role"] == "assistant"][-1]
    assert marker.splitlines() == [
        "[model stopped generating]",
        "[a send call to notes was not sent]",
    ], marker
    seen = [
        e["type"]
        for e in _drain(k, "w_cut")
        if str(e.get("type", "")).startswith("part_")
    ]
    assert seen[0] == "part_start" and seen[-1] == "part_abort", seen
    # nothing was dispatched to notes: no reply came back for it
    assert "reflect" not in str(nxt[-1]["content"])


async def test_a_receiver_stops_the_stream_and_the_turn_keeps_a_marker(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    _ai(k, "sp_stop")
    k.create("ram_store.tools", id="rx")
    wire(k, "rx", "sp_stop")
    k.ctx.ensure_inbox("w_stop")
    k.watch("sp_stop", "w_stop")
    long = "partial words " * 200
    fp = fake_providers["sp_stop"] = _Slow([long, DONE], step=0.01)
    # the send dispatch lasts as long as the stream: run it, stop it mid-way
    turn = asyncio.ensure_future(
        k.send(
            [
                {
                    "target": "sp_stop",
                    "payload": {"type": "send", "text": "go", "client_id": "tab"},
                }
            ]
        )
    )
    await _until(lambda: len(fp.requests) >= 1)
    await asyncio.sleep(0.2)  # some text has streamed
    with as_agent(k, "rx"):
        out = (await k.send([{"target": "sp_stop", "payload": {"type": "stop"}}]))[0]
    assert out["result"] == {"stopped": True}, out
    assert (await turn)[0]["status"] == "cancelled"
    seen = [
        e["type"]
        for e in _drain(k, "w_stop")
        if str(e.get("type", "")).startswith("part_")
    ]
    assert seen[-1] == "part_abort", seen
    # the next turn sees what it was asked and the marker — never the half answer
    fp.step = 0.0
    await k.send([{"target": "sp_stop", "payload": {"type": "send", "text": "again"}}])
    await _until(lambda: len(fp.requests) >= 2)
    nxt = fp.requests[1]
    assert any(m["role"] == "user" and "go" in m["content"] for m in nxt), nxt
    # the visible text is the answer — never the bare marker; the stopped notice
    # is a user row after it (#232)
    answer = [m["content"] for m in nxt if m["role"] == "assistant"][-1]
    assert answer.startswith("partial words"), answer[:80]
    assert any(
        m["role"] == "user" and m["content"].startswith("[your turn was stopped by rx]")
        for m in nxt
    ), nxt


async def test_stop_with_nothing_in_flight_says_so(seeded_kernel, fake_providers):
    k = seeded_kernel
    _ai(k, "sp_idle")
    out = (await k.send([{"target": "sp_idle", "payload": {"type": "stop"}}]))[0]
    assert out["result"] == {"stopped": False}


# ─── the match: draft ↔ final message ────────────────────────────────────────


async def test_a_message_carries_the_stream_it_was_previewed_as(
    seeded_kernel, fake_providers
):
    """AI → AI: the consumer watches `part_*` on its inbox, then receives the
    message; the message's header names the same stream id, `part_start` names the
    message the producer answers (`re`, the draft's anchor), `part_end` says ok."""
    k = seeded_kernel
    _ai(k, "sp_prod")
    _ai(k, "sp_cons")
    wire(k, "sp_prod", "sp_cons")
    k.ctx.ensure_inbox("w_cons")
    k.watch("sp_cons", "w_cons")
    msg = (
        '<call name="send">\n<target_id>sp_cons</target_id>\n<type>send</type>\n'
        "<text>hello consumer</text>\n</call>"
    )
    prod = fake_providers["sp_prod"] = _Slow([msg, DONE])
    cons = fake_providers["sp_cons"] = _Slow([DONE])
    await k.send([{"target": "sp_prod", "payload": {"type": "send", "text": "go"}}])
    await _until(lambda: len(cons.requests) >= 1)
    parts = [
        e for e in _drain(k, "w_cons") if str(e.get("type", "")).startswith("part_")
    ]
    start = next(e for e in parts if e["type"] == "part_start")
    end = next(e for e in parts if e["type"] == "part_end")
    assert start["target"] == "sp_cons" and isinstance(start.get("re"), int), start
    assert end["ok"] is True and end["stream_id"] == start["stream_id"]
    delivered = cons.requests[0][-1]["content"]
    assert f"stream {start['stream_id']}" in delivered.splitlines()[0], delivered
    assert "hello consumer" in delivered
    assert len(prod.requests) >= 1


def test_a_malformed_closed_call_ends_not_ok():
    ev, _ = _run(
        '<call name="send"><target_id>x</target_id><type>a<path>p</path></call>', 3
    )
    end = next(e for e in ev if e[0] == "end" and e[1]["kind"] == "call")
    assert end[2] == {"ok": False}


async def test_status_during_a_stream_is_json(seeded_kernel, fake_providers):
    """The in-flight entry holds the live splitter: a `status` snapshot never
    carries it (it is bookkeeping, and not JSON)."""
    import json

    k = seeded_kernel
    _ai(k, "sp_st")
    fp = fake_providers["sp_st"] = _Slow(["words " * 300, DONE], step=0.01)
    turn = asyncio.ensure_future(
        k.send([{"target": "sp_st", "payload": {"type": "send", "text": "go"}}])
    )
    await _until(lambda: len(fp.requests) >= 1)
    await asyncio.sleep(0.1)
    out = (await k.send([{"target": "sp_st", "payload": {"type": "status"}}]))[0]
    json.dumps(out["result"])
    assert "parts" not in (out["result"].get("current") or {})
    await turn


async def test_rows_is_a_view_of_what_the_agent_holds(seeded_kernel, fake_providers):
    """`rows` is the agent's in-memory head, as held — no store read, no transform:
    nothing loaded → loaded:false; after a turn → its rows, paged by row id."""
    k = seeded_kernel
    _ai(k, "sp_rows")
    k.create("ram_store.tools", id="viewer")
    wire(k, "viewer", "sp_rows", ["rows"])
    with as_agent(k, "viewer"):
        cold = (await k.send([{"target": "sp_rows", "payload": {"type": "rows"}}]))[0][
            "result"
        ]
    assert cold == {"rows": [], "next": None, "total": 0, "loaded": False}, cold
    fp = fake_providers["sp_rows"] = _Slow(["hello there", DONE])
    await k.send([{"target": "sp_rows", "payload": {"type": "send", "text": "hi"}}])
    await _until(lambda: len(fp.requests) >= 2)
    with as_agent(k, "viewer"):
        page = (
            await k.send(
                [{"target": "sp_rows", "payload": {"type": "rows", "limit": 1}}]
            )
        )[0]["result"]
        rest = (
            await k.send(
                [
                    {
                        "target": "sp_rows",
                        "payload": {"type": "rows", "after": page["next"]},
                    }
                ]
            )
        )[0]["result"]
    assert page["loaded"] is True and page["total"] >= 2, page
    assert page["rows"][0]["role"] == "user" and "hi" in page["rows"][0]["content"]
    assert (
        len(page["rows"]) + len(rest["rows"]) == page["total"] and rest["next"] is None
    )
    # an ungranted caller is refused by the gate
    k.create("ram_store.tools", id="stranger")
    with as_agent(k, "stranger"):
        out = (await k.send([{"target": "sp_rows", "payload": {"type": "rows"}}]))[0]
    assert (out["result"] or {}).get("reason") == "unauthorized", out


async def test_a_cut_pass_sends_nothing_not_even_its_closed_calls(
    seeded_kernel, fake_providers
):
    """Only a successful generation dispatches: a pass that
    wrote a complete call and then stopped with another block open sends NEITHER,
    and the marker names both as not sent."""
    k = seeded_kernel
    _ai(k, "sp_all")
    k.create("ram_store.tools", id="notes")
    wire(k, "sp_all", "notes")
    done_one = (
        '<call name="send"><target_id>notes</target_id><type>reflect</type></call>'
    )
    open_two = '<call name="send"><target_id>notes</target_id><type>refl'
    fp = fake_providers["sp_all"] = _Slow([done_one + " then " + open_two, DONE])
    await k.send([{"target": "sp_all", "payload": {"type": "send", "text": "go"}}])
    await _until(lambda: len(fp.requests) >= 2)
    nxt = fp.requests[1]
    marker = [m["content"] for m in nxt if m["role"] == "assistant"][-1]
    assert marker.splitlines() == [
        "[model stopped generating]",
        "then",
        "[a send call to notes was not sent]",
        "[a send call to notes was not sent]",
    ], marker
    # nothing reached notes: no reply came back for the closed call either
    assert "[reply to" not in nxt[-1]["content"], nxt[-1]["content"]


async def test_deltas_never_reach_the_state_stream(seeded_kernel, fake_providers):
    """Deltas go to the addressee's inbox only: the state
    stream carries a part's lifecycle — start / end / abort — never a delta."""
    k = seeded_kernel
    _ai(k, "sp_q")
    k.create("ram_store.tools", id="notes")
    wire(k, "sp_q", "notes")
    k.ctx.ensure_inbox("w_q")
    k.watch("notes", "w_q")
    seen: list[dict] = []
    k.ctx.state_subscribers.append(lambda ev: seen.append(ev))
    fp = fake_providers["sp_q"] = _Slow([CALL, DONE])
    await k.send([{"target": "sp_q", "payload": {"type": "send", "text": "go"}}])
    await _until(lambda: len(fp.requests) >= 2)
    inbox = [
        e["type"]
        for e in _drain(k, "w_q")
        if str(e.get("type", "")).startswith("part_")
    ]
    assert (
        "part_delta" in inbox and inbox[0] == "part_start" and inbox[-1] == "part_end"
    )
    on_stream = {
        (e.get("payload") or {}).get("type")
        for e in seen
        if str((e.get("payload") or {}).get("type", "")).startswith("part_")
    }
    assert on_stream == {"part_start", "part_end"}, on_stream


# ─── stop reaches the turn however it started (#177 / #232) ─────────────────


def _start_send(k, aid, text="go"):
    return asyncio.ensure_future(
        k.send(
            [
                {
                    "target": aid,
                    "payload": {"type": "send", "text": text, "client_id": "tab"},
                }
            ]
        )
    )


async def _stop_as_rx(k, aid) -> dict:
    with as_agent(k, "rx"):
        return (await k.send([{"target": aid, "payload": {"type": "stop"}}]))[0][
            "result"
        ]


async def _generating(k, aid) -> bool:
    out = (await k.send([{"target": aid, "payload": {"type": "status"}}]))[0]
    return out["result"]["generating"]


async def _rows_of(k, aid) -> list[dict]:
    return await core._load_history(aid, k)


async def test_stop_closes_a_stalled_turn_the_kernel_cancel_did_not_reach(
    seeded_kernel, fake_providers, monkeypatch
):
    k = seeded_kernel
    aid = "sp_nofind"
    _ai(k, aid)
    k.create("ram_store.tools", id="rx")
    wire(k, "rx", aid)

    async def _not_found(task_id):
        return {"cancelled": False, "killed": []}

    monkeypatch.setattr(k.ctx, "cancel_task", _not_found)
    fp = fake_providers[aid] = _Slow(["never"], step=600)
    turn = _start_send(k, aid)
    try:
        await _until(lambda: len(fp.requests) >= 1)
        await asyncio.sleep(0.1)
        assert await _stop_as_rx(k, aid) == {"stopped": True}
        assert await _generating(k, aid) is False
        stopped = [r for r in await _rows_of(k, aid) if r.get("kind") == "stopped"]
        assert stopped and stopped[-1]["content"].startswith(
            "[your turn was stopped by rx"
        ), stopped
        fp.step = 0
        out = (await asyncio.wait_for(_start_send(k, aid, "after"), 5))[0]["result"]
        assert "error" not in out and "raised" not in str(out), out
        assert len(fp.requests) == 2
    finally:
        turn.cancel()


async def test_stop_aborts_a_send_the_kernel_cancel_does_not_find(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    aid = "sp_abort"
    _ai(k, aid)
    k.create("ram_store.tools", id="rx")
    wire(k, "rx", aid)
    fp = fake_providers[aid] = _Slow(["never"], step=600)
    turn = _start_send(k, aid)
    await _until(lambda: len(fp.requests) >= 1)
    await asyncio.sleep(0.1)
    # a task id the kernel's table does not hold
    core._current[aid]["task_id"] = "9000000001"
    assert await _stop_as_rx(k, aid) == {"stopped": True}
    out = (await asyncio.wait_for(turn, 5))[0]
    assert out["status"] == "cancelled", out
    assert await _generating(k, aid) is False
    fp.step = 0
    nxt = (await asyncio.wait_for(_start_send(k, aid, "after"), 5))[0]["result"]
    assert "error" not in nxt, nxt
    assert len(fp.requests) == 2


async def test_stop_reaches_a_reply_woken_turn(seeded_kernel, fake_providers):
    k = seeded_kernel
    aid = "sp_woken"
    _ai(k, aid)
    k.create("ram_store.tools", id="rx")
    wire(k, "rx", aid)
    fp = fake_providers[aid] = _Slow(["never"], step=600)
    k.ctx.spawn_task(
        aid,
        "deliver_reply",
        core._deliver(
            k, aid, "tool reply", "tab", None, born=core._incarnation.get(aid, 0)
        ),
    )
    await _until(lambda: len(fp.requests) >= 1)
    await asyncio.sleep(0.1)
    assert await _stop_as_rx(k, aid) == {"stopped": True}
    await _until(lambda: not core._current.get(aid))
    assert await _generating(k, aid) is False
    stopped = [r for r in await _rows_of(k, aid) if r.get("kind") == "stopped"]
    assert stopped and stopped[-1]["content"].startswith(
        "[your turn was stopped by rx"
    ), stopped
    fp.step = 0
    nxt = (await asyncio.wait_for(_start_send(k, aid, "after"), 5))[0]["result"]
    assert "error" not in nxt, nxt
    assert len(fp.requests) == 2
