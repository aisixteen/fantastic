"""C24 — streaming: order preserved, terminal event closes, subscribers agree.

Exemplar `tmp/repos/a2a-tck/tests/compatibility/core_operations/test_multi_stream.py`
(STREAM-ORDER-001..004), plus `dbos/tests/test_streaming.py` for the offset and
termination cases. The kernel's streaming surfaces are `emit` into an inbox,
`watch` mirroring one agent's traffic into another's, and the kernel-wide state
stream (`publish_state` / `add_state_subscriber`).

The property under test is ORDER and TERMINATION, not throughput: a consumer that
cannot tell where a stream ended, or that sees two producers interleaved
differently from its neighbour, cannot reassemble what it was sent.
"""

from __future__ import annotations

import asyncio

import pytest

TOKENS = [f"tok{i:02d}" for i in range(12)]


def _drain(kernel, inbox_id: str) -> list[dict]:
    # THE TEST IS THE READER, so it registers. A queue exists iff someone
    # called `ensure_inbox` for it; nothing mints one on an agent's behalf.
    q = kernel.ctx.ensure_inbox(inbox_id)
    out = []
    while not q.empty():
        out.append(q.get_nowait())
    return out


async def _agent(kernel, aid: str) -> None:
    """Create the agent AND register this test as the reader of its inbox.

    A queue exists iff someone called `ensure_inbox` for it — an agent does not
    get one merely by existing, and nothing mints one on its behalf. Every test
    below drains this agent, so the registration belongs here, before the
    traffic: a reader that arrives after the emits sees an empty queue, which is
    the honest semantics and not a bug to paper over.
    """
    (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": aid,
                    },
                }
            ]
        )
    )[0]["result"]
    kernel.ctx.ensure_inbox(aid)


async def _stream(kernel, src: str, tokens=TOKENS) -> None:
    """One producer pass: N `token` events then exactly one `done`.

    `emit` resolves its target as an AGENT (`ctx.agents.get`), so a bare inbox id
    is silently a no-op — the producer must be a real agent.
    """
    for t in tokens:
        await kernel.emit(src, {"type": "token", "text": t})
    await kernel.emit(src, {"type": "done"})


# ─── order + termination ────────────────────────────────────────


async def test_a_stream_arrives_in_order_and_the_last_event_is_terminal(kernel):
    """STREAM-ORDER-001. Order is the whole contract for a token stream — the
    text is only meaningful reassembled in sequence — and the terminal event is
    what tells a consumer to stop waiting rather than hang."""
    await _agent(kernel, "sink")
    await _stream(kernel, "sink")

    got = _drain(kernel, "sink")
    assert [e["text"] for e in got if e["type"] == "token"] == TOKENS
    assert got[-1]["type"] == "done", "the stream must end with its terminal event"
    assert [e["type"] for e in got].count("done") == 1, "exactly one terminal"


async def test_nothing_follows_the_terminal_event_within_a_pass(kernel):
    """A `done` that is not last is worse than a missing one: a consumer that
    stops on `done` silently drops whatever came after it."""
    await _agent(kernel, "sink")
    await _stream(kernel, "sink")
    got = _drain(kernel, "sink")
    assert all(e["type"] != "done" for e in got[:-1])


# ─── multiple subscribers ───────────────────────────────────────


async def test_two_watchers_of_one_source_see_the_same_events_in_the_same_order(
    kernel,
):
    """STREAM-ORDER-002/003: a broadcast reaches every subscriber, and each sees
    the SAME sequence. Two consumers that disagree about order cannot be
    reconciled by anything downstream."""
    await _agent(kernel, "src")
    for w in ("w1", "w2"):
        kernel.ctx.ensure_inbox(w)  # a watcher is a reader; it registers first
        kernel.watch("src", w)

    await _stream(kernel, "src")

    a, b = _drain(kernel, "w1"), _drain(kernel, "w2")
    assert a == b, "subscribers must agree, event for event"
    assert [e["text"] for e in a if e["type"] == "token"] == TOKENS


async def test_dropping_one_subscriber_does_not_disturb_the_others(kernel):
    """STREAM-ORDER-004. A consumer leaving is routine; it must not truncate or
    reorder anyone else's view."""
    await _agent(kernel, "src")
    for w in ("stay", "leave"):
        kernel.ctx.ensure_inbox(w)
        kernel.watch("src", w)

    await kernel.emit("src", {"type": "token", "text": "first"})
    kernel.unwatch("src", "leave")
    for t in ("second", "third"):
        await kernel.emit("src", {"type": "token", "text": t})
    await kernel.emit("src", {"type": "done"})

    stayed = [e["text"] for e in _drain(kernel, "stay") if e["type"] == "token"]
    left = [e["text"] for e in _drain(kernel, "leave") if e["type"] == "token"]
    assert stayed == ["first", "second", "third"]
    assert left == ["first"], "an unwatched subscriber stops receiving, mid-stream"


async def test_two_producers_do_not_interleave_into_one_subscriber_out_of_order(
    kernel,
):
    """Per-source order is what must hold. Two streams into one inbox may
    interleave — nothing promises otherwise — but each source's OWN sequence must
    still come out intact, or a consumer cannot demultiplex them."""
    for aid in ("p1", "p2"):
        await _agent(kernel, aid)
        kernel.ctx.ensure_inbox("mux")
        kernel.watch(aid, "mux")

    await asyncio.gather(
        _stream(kernel, "p1", [f"a{i}" for i in range(6)]),
        _stream(kernel, "p2", [f"b{i}" for i in range(6)]),
    )

    got = [e["text"] for e in _drain(kernel, "mux") if e["type"] == "token"]
    assert [t for t in got if t.startswith("a")] == [f"a{i}" for i in range(6)]
    assert [t for t in got if t.startswith("b")] == [f"b{i}" for i in range(6)]


# ─── the kernel-wide state stream ───────────────────────────────


async def test_the_state_stream_delivers_in_order_to_every_subscriber(kernel):
    """The loader's flush loop rides this stream, so its ordering is not a
    telemetry nicety: `added` before `updated` before `removed` is what makes a
    persisted tree match a live one."""
    seen_a: list[str] = []
    seen_b: list[str] = []
    # `kind` is the state-event discriminator; `type` is the PAYLOAD's verb,
    # which only traffic events carry.
    kernel.ctx.add_state_subscriber(lambda e: seen_a.append(e["kind"]))
    kernel.ctx.add_state_subscriber(lambda e: seen_b.append(e["kind"]))

    await _agent(kernel, "s1")
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {"type": "update_agent", "id": "s1", "note": "x"},
                }
            ]
        )
    )[0]["result"]
    (
        await kernel.send(
            [{"target": kernel.id, "payload": {"type": "delete_agent", "id": "s1"}}]
        )
    )[0]["result"]

    assert seen_a == seen_b, "every subscriber sees the same stream"
    lifecycle = [k for k in seen_a if k in ("added", "updated", "removed")]
    assert lifecycle == ["added", "updated", "removed"]


async def test_a_raising_subscriber_does_not_break_the_stream_for_the_others(kernel):
    """One bad consumer must not take the loader down with it — the flush loop
    is a subscriber, and losing it silently stops all persistence."""
    good: list[dict] = []
    kernel.ctx.add_state_subscriber(lambda e: (_ for _ in ()).throw(RuntimeError("x")))
    kernel.ctx.add_state_subscriber(good.append)

    await _agent(kernel, "s2")
    assert good, "the surviving subscriber still received the event"


# ─── backpressure ───────────────────────────────────────────────


async def test_an_inbox_is_bounded_rather_than_unbounded(kernel):
    """`INBOX_BOUND` exists so a fast producer cannot consume all memory on
    behalf of a consumer that stopped reading. Pinned because the failure mode of
    getting this wrong is not an error — it is a process that grows until it
    dies."""
    from kernel import INBOX_BOUND

    kernel.ctx.ensure_inbox("bounded")
    assert kernel.ctx.inboxes["bounded"].maxsize == INBOX_BOUND
    assert INBOX_BOUND > 0


@pytest.mark.parametrize("verb", ["token", "done"])
async def test_emit_never_invokes_a_handler(kernel, verb):
    """`emit` is non-routing by contract: it drops into an inbox and tells
    watchers. If it dispatched, a stream of tokens would run a handler per token
    and a `done` would be indistinguishable from a call."""
    await _agent(kernel, "quiet")
    await kernel.emit("quiet", {"type": verb})
    assert not kernel.ctx.ensure_inbox("quiet").empty()


# ─── a queue exists iff a reader registered it ──────────────────


async def test_an_agent_has_no_inbox_until_a_reader_registers_one(kernel):
    """A QUEUE EXISTS IFF A READER REGISTERED IT. Until 2026-09-11 every agent
    got one at construction and NOTHING in the tree read it: a 256-slot ring
    that filled once and then evicted one payload per message, for the life of
    the process. `ensure_inbox` IS the registration; an agent does not get a
    queue merely by existing."""
    await _agent(kernel, "lonely")
    kernel.ctx.inboxes.pop("lonely", None)  # undo _agent's registration
    assert "lonely" not in kernel.ctx.inboxes

    await kernel.emit("lonely", {"type": "token", "text": "x"})
    assert "lonely" not in kernel.ctx.inboxes, "an emit allocated a queue"

    q = kernel.ctx.ensure_inbox("lonely")
    assert q.empty(), "the reader arrived after the emit; it sees nothing"
    await kernel.emit("lonely", {"type": "token", "text": "y"})
    assert q.get_nowait()["text"] == "y"


async def test_watch_without_a_registered_inbox_is_refused(kernel):
    """`watch` used to lazy-create the watcher's queue, which meant a caller
    could watch and never drain: payloads piled to INBOX_BOUND and were then
    evicted one per message, with nothing to say so. Inventing a consumer on
    the caller's behalf is the bug; refusing names it."""
    await _agent(kernel, "src_agent")
    denial = kernel.watch("src_agent", "ghost")
    assert denial is not None and denial["reason"] == "no_consumer", denial
    assert denial["watcher"] == "ghost" and denial["src"] == "src_agent"
    assert "ghost" not in kernel.ctx.inboxes, "the refusal still minted a queue"

    kernel.ctx.ensure_inbox("ghost")
    assert kernel.watch("src_agent", "ghost") is None
    await kernel.emit("src_agent", {"type": "token", "text": "seen"})
    assert kernel.ctx.inboxes["ghost"].get_nowait()["text"] == "seen"
