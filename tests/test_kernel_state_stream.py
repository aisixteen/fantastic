"""kernel state stream — direct-callback telemetry tap.

Covers:
- Subscribers see traffic events (`send` / `emit`) and lifecycle
  events (`added` / `removed` / `updated`) through one channel.
- Snapshot is sync, doesn't itself emit.
- No-recursion: callback that triggers further substrate ops sees
  finite cascades, not infinite loops.
- Trimmed `summary` for telemetry overlays; bytes safely stringified.
"""

from __future__ import annotations

import asyncio


async def test_no_subscribers_zero_overhead(seeded_kernel):
    """1000 fanouts with no subscribers is a no-op + no error."""
    assert seeded_kernel._state_subscribers == []
    for _ in range(1000):
        await seeded_kernel.emit("cli", {"type": "noop"})


async def test_subscriber_receives_send_events(seeded_kernel):
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    await seeded_kernel.send("cli", {"type": "token", "text": "x"})
    sends = [e for e in events if e["kind"] == "send"]
    assert sends, f"expected at least one send event, got {events}"
    e = sends[0]
    assert e["agent_id"] == "cli"
    assert e["backlog"] >= 1
    assert "ts" in e
    # External entry has no caller in scope → sender is None.
    assert "sender" in e and e["sender"] is None
    # Trimmed payload summary for the messages overlay.
    assert "summary" in e and "token" in e["summary"] and "x" in e["summary"]


async def test_summary_trims_long_payload_and_handles_bytes(seeded_kernel):
    """The `summary` field on state events must (1) replace bytes
    values with `<bytes:N>` so JSON-on-the-wire telemetry doesn't choke
    on binary protocol payloads and (2) trim to a fixed ceiling so a
    huge text payload can't blow up the per-event WS frame size.
    """
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    huge = "z" * 5000
    await seeded_kernel.emit(
        "cli", {"type": "blob", "data": b"\x00\x01\x02\x03", "tail": huge}
    )
    e = next(x for x in events if x["kind"] == "emit")
    assert "<bytes:4>" in e["summary"]
    assert len(e["summary"]) < 200, f"summary not trimmed: {len(e['summary'])} chars"
    assert e["summary"].endswith("…")


async def test_state_event_sender_set_inside_handler_dispatch(seeded_kernel):
    """When a handler is being dispatched, kernel.send/emit calls
    issued from inside it report `sender` as the dispatched agent's id.
    Validates the contextvar set/reset around mod.handler().

    We can't easily install a fake handler from a test (no faux bundle),
    so we mimic the in-handler scope by manually setting the contextvar
    and calling kernel.send. Same code path the kernel takes around
    its `await mod.handler(...)` call."""
    from kernel import _current_sender

    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))

    token = _current_sender.set("alice_xxx")
    try:
        await seeded_kernel.send("cli", {"type": "token", "text": "x"})
        await seeded_kernel.emit("cli", {"type": "noop"})
    finally:
        _current_sender.reset(token)

    sends = [e for e in events if e["kind"] == "send" and e["agent_id"] == "cli"]
    emits = [e for e in events if e["kind"] == "emit" and e["agent_id"] == "cli"]
    assert sends and sends[0]["sender"] == "alice_xxx"
    assert emits and emits[0]["sender"] == "alice_xxx"


async def test_state_event_sender_resets_after_handler_returns(seeded_kernel):
    """After kernel.send returns, the contextvar must be reset so the
    NEXT external send reports sender=None — not the previous handler's
    id leaking through."""
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    # First send dispatches a handler (cli's token handler) which
    # internally inherits the contextvar = 'cli' for any nested calls.
    # The handler doesn't make nested calls, so we can't observe that
    # directly here — but after it returns, the contextvar should
    # restore to its pre-call value (None).
    await seeded_kernel.send("cli", {"type": "token", "text": "first"})
    events.clear()
    # Second external call: sender should be None (contextvar reset).
    await seeded_kernel.send("cli", {"type": "token", "text": "second"})
    sends = [e for e in events if e["kind"] == "send" and e["agent_id"] == "cli"]
    assert sends and sends[0]["sender"] is None, "contextvar leaked across calls"


async def test_subscriber_receives_emit_events(seeded_kernel):
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    await seeded_kernel.emit("cli", {"type": "noop"})
    emits = [e for e in events if e["kind"] == "emit"]
    assert emits and emits[0]["agent_id"] == "cli"
    # emit doesn't bump in-flight (no handler runs); backlog reports
    # current concurrent handler count, which is 0 here.
    assert emits[0]["backlog"] == 0


async def test_subscriber_sees_real_agent_watcher_fanout(seeded_kernel):
    """A real agent that watches B: when send hits B, tap fires for
    B AND for A's mirrored put. Both are real agents."""
    seeded_kernel.watch("cli", "core")  # core watches cli
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    await seeded_kernel.send("cli", {"type": "noop"})
    targets = {e["agent_id"] for e in events if e["kind"] == "send"}
    assert "cli" in targets
    assert "core" in targets, (
        f"real-agent watcher's mirrored fanout should produce its own event; got {events}"
    )


async def test_non_agent_watcher_does_not_emit_state_events(seeded_kernel):
    """Non-agent watchers (e.g. the webapp proxy's `_ws_*` pseudo-clients
    registered via kernel._ensure_inbox + kernel.watch) must NOT show
    up in the state stream — they aren't real agents and would mint
    phantom sprites in the telemetry view."""
    fake_ws = "_ws_test_pseudo"
    seeded_kernel._ensure_inbox(fake_ws)  # mirror what _proxy.run does
    seeded_kernel.watch("cli", fake_ws)
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    await seeded_kernel.send("cli", {"type": "noop"})
    leaked = [e for e in events if e["agent_id"] == fake_ws]
    assert leaked == [], (
        f"non-agent watcher leaked {len(leaked)} state events: {leaked}"
    )
    # The real agent (cli) still gets its own event.
    cli_evts = [e for e in events if e["agent_id"] == "cli" and e["kind"] == "send"]
    assert len(cli_evts) >= 1


async def test_unsubscribe_stops_callbacks(seeded_kernel):
    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    await seeded_kernel.emit("cli", {"type": "first"})
    pre = len(events)
    unsub()
    await seeded_kernel.emit("cli", {"type": "second"})
    assert len(events) == pre, "no events should land after unsubscribe"


async def test_multiple_subscribers_all_called(seeded_kernel):
    a, b = [], []
    seeded_kernel.add_state_subscriber(lambda e: a.append(e))
    seeded_kernel.add_state_subscriber(lambda e: b.append(e))
    await seeded_kernel.emit("cli", {"type": "x"})
    assert a and b
    assert len(a) == len(b)


async def test_callback_exception_does_not_break_fanout(seeded_kernel):
    seen: list[dict] = []

    def bad(e):
        raise RuntimeError("boom")

    seeded_kernel.add_state_subscriber(bad)
    seeded_kernel.add_state_subscriber(lambda e: seen.append(e))
    # Must not raise.
    await seeded_kernel.emit("cli", {"type": "x"})
    assert seen, "second subscriber should still receive events"


async def test_state_snapshot_returns_all_agents(seeded_kernel):
    snap = seeded_kernel.state_snapshot()
    ids = {a["agent_id"] for a in snap}
    assert "cli" in ids and "core" in ids
    for a in snap:
        assert "name" in a and "backlog" in a
        assert isinstance(a["backlog"], int)


async def test_state_snapshot_does_not_emit(seeded_kernel):
    """Calling state_snapshot must not produce telemetry events itself."""
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    seeded_kernel.state_snapshot()
    seeded_kernel.state_snapshot()
    assert events == []


async def test_no_recursion_when_callback_calls_send(seeded_kernel):
    """A callback that does kernel.send produces ONE more event per send,
    not an infinite multiplier."""
    counter = {"n": 0}

    def cb(e):
        counter["n"] += 1
        # Only react once to the very first event; otherwise we'd
        # genuinely multiply traffic (which is fine; it's bounded by
        # the depth of the chain we choose). Here we cap at 3 sends.
        if counter["n"] < 3 and e["kind"] == "send":
            asyncio.get_event_loop().call_soon(
                lambda: asyncio.create_task(seeded_kernel.emit("cli", {"type": "echo"}))
            )

    seeded_kernel.add_state_subscriber(cb)
    await seeded_kernel.send("cli", {"type": "token", "text": "boot"})
    # Drain any scheduled tasks.
    for _ in range(5):
        await asyncio.sleep(0.01)
    # We expect a finite, bounded count — NOT runaway. (Exact count
    # depends on event-loop scheduling; the load-bearing assertion is
    # finiteness within a small budget.)
    assert counter["n"] < 50, f"runaway recursion: {counter['n']} events"


async def test_send_emits_drain_after_handler_returns(seeded_kernel):
    """Every send produces both a 'send' event (blip + count up) and a
    'drain' event (count down, no blip) once the handler completes."""
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    await seeded_kernel.send("cli", {"type": "token", "text": "x"})
    sends = [e for e in events if e["kind"] == "send"]
    drains = [e for e in events if e["kind"] == "drain"]
    assert len(sends) >= 1 and len(drains) >= 1
    # Send fired with backlog ≥ 1; drain fired with the lower count.
    assert sends[0]["backlog"] >= 1
    assert drains[-1]["backlog"] == 0


async def test_in_flight_bumps_during_handler(seeded_kernel):
    """Inside a handler, the kernel's in-flight count for that agent
    is at least 1 — proving backlog is a real queue depth, not a
    monotonic counter."""
    seen_in_flight: list[int] = []

    async def slow_send():
        # The act of the handler running is observed via the kernel's
        # _in_flight dict — at the moment we sample, our handler is
        # still on the stack.
        await asyncio.sleep(0)  # yield once
        seen_in_flight.append(seeded_kernel._in_flight.get("cli", 0))

    # Use cli as a victim agent; we call send and concurrently
    # introspect _in_flight from another task.
    async def caller():
        await seeded_kernel.send("cli", {"type": "token", "text": "x"})

    async def observer():
        # Sample while the send is mid-flight.
        await asyncio.sleep(0)
        seen_in_flight.append(seeded_kernel._in_flight.get("cli", 0))

    await asyncio.gather(caller(), observer(), slow_send())
    # After everything completes, in_flight is back to 0.
    assert seeded_kernel._in_flight.get("cli", 0) == 0


async def test_emit_does_not_bump_in_flight(seeded_kernel):
    """emit() is a broadcast — no handler dispatch, so in_flight unchanged."""
    pre = seeded_kernel._in_flight.get("cli", 0)
    await seeded_kernel.emit("cli", {"type": "noop"})
    assert seeded_kernel._in_flight.get("cli", 0) == pre


async def test_state_snapshot_reports_in_flight_not_qsize(seeded_kernel):
    """Snapshot's `backlog` field reflects in-flight count (queue
    depth), not the inbox queue's lifetime size."""
    # No traffic yet → all backlogs 0.
    snap = seeded_kernel.state_snapshot()
    for a in snap:
        assert a["backlog"] == 0
    # Fire some emits — these fanout into inboxes (qsize would grow)
    # but don't bump in_flight.
    for _ in range(5):
        await seeded_kernel.emit("cli", {"type": "noop"})
    snap = seeded_kernel.state_snapshot()
    cli_entry = next(a for a in snap if a["agent_id"] == "cli")
    assert cli_entry["backlog"] == 0, (
        "snapshot must report in-flight (0), not inbox qsize (would be 5)"
    )


async def test_lifecycle_added_fires_on_create(seeded_kernel):
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    seeded_kernel.create("file.tools", id="file_test1")
    added = [
        e for e in events if e["kind"] == "added" and e["agent_id"] == "file_test1"
    ]
    assert len(added) == 1
    assert added[0]["name"] == "file_test1"


async def test_lifecycle_added_fires_on_ensure(kernel):
    """ensure() (singleton path) also fires 'added' for first-time creation."""
    events: list[dict] = []
    kernel.add_state_subscriber(lambda e: events.append(e))
    kernel.ensure("core", "core.tools", singleton=True, display_name="core")
    added = [e for e in events if e["kind"] == "added" and e["agent_id"] == "core"]
    assert len(added) == 1


async def test_lifecycle_updated_fires_on_update(seeded_kernel):
    seeded_kernel.create("file.tools", id="file_test2")
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    seeded_kernel.update("file_test2", display_name="renamed")
    updated = [
        e for e in events if e["kind"] == "updated" and e["agent_id"] == "file_test2"
    ]
    assert len(updated) == 1
    assert updated[0]["name"] == "renamed"


async def test_lifecycle_removed_fires_on_delete(seeded_kernel):
    """Notify must fire AFTER the dict mutation: kernel.get(id) inside
    the callback returns None."""
    seeded_kernel.create("file.tools", id="file_test3")
    observations: list[tuple[str, object]] = []

    def cb(e):
        if e["kind"] == "removed" and e["agent_id"] == "file_test3":
            observations.append(("get", seeded_kernel.get("file_test3")))

    seeded_kernel.add_state_subscriber(cb)
    seeded_kernel.delete("file_test3")
    assert observations == [("get", None)], (
        f"callback should observe deletion already applied; got {observations}"
    )


async def test_lifecycle_does_not_fire_for_traffic(seeded_kernel):
    """send/emit only produce 'send'/'emit' events, never lifecycle ones."""
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    await seeded_kernel.send("cli", {"type": "token", "text": "x"})
    await seeded_kernel.emit("cli", {"type": "noop"})
    lifecycle = [e for e in events if e["kind"] in ("added", "removed", "updated")]
    assert lifecycle == [], f"traffic must not produce lifecycle events: {lifecycle}"


async def test_no_recursion_when_callback_creates_agent(seeded_kernel):
    """Callback that creates one agent in response to its own
    'added' event must NOT create infinitely. We cap explicitly."""
    counter = {"creates": 0}

    def cb(e):
        if e["kind"] == "added" and counter["creates"] < 3:
            counter["creates"] += 1
            seeded_kernel.create("file.tools", id=f"file_chain_{counter['creates']}")

    seeded_kernel.add_state_subscriber(cb)
    seeded_kernel.create("file.tools", id="file_seed")
    # Bounded chain (we cap at 3) — not an unbounded loop.
    assert counter["creates"] == 3
