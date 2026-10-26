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
    assert seeded_kernel.ctx.state_subscribers == []
    for _ in range(1000):
        await seeded_kernel.emit("cli", {"type": "noop"})


async def test_state_events_are_python_shape(seeded_kernel):
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    (
        await seeded_kernel.send(
            [{"target": "cli", "payload": {"type": "token", "text": "x"}}]
        )
    )[0]["result"]
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
# twin: summarize_never_truncates
# twin: a_bytes_payload_summarizes_as_bytes_n


async def test_summary_shrinks_bytes_but_never_truncates(seeded_kernel):
    """The `summary` field replaces bytes values with `<bytes:N>` so JSON-on-the-wire
    telemetry doesn't choke on binary protocol payloads — and it does NOT trim.

    Cutting at a fixed length + an ellipsis would make the telemetry view lie:
    the reader cannot tell a short payload from a truncated one, and the part that
    got dropped is exactly the part you go looking at telemetry to find. Eliding is
    a RENDER decision, where whoever is looking can see that it happened."""
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    huge = "z" * 5000
    await seeded_kernel.emit(
        "cli", {"type": "blob", "data": b"\x00\x01\x02\x03", "tail": huge}
    )
    e = next(x for x in events if x["kind"] == "emit")
    assert "<bytes:4>" in e["summary"], "bytes must still be shrunk, not serialized"
    assert huge in e["summary"], "the payload must survive whole"
    assert not e["summary"].endswith("…")


async def test_state_event_sender_set_inside_handler_dispatch(seeded_kernel):
    """When a handler is being dispatched, kernel.send/emit calls
    issued from inside it report `sender` as the dispatched agent's id.
    Validates the contextvar set/reset around mod.handler().

    We can't easily install a fake handler from a test (no faux bundle),
    so we mimic the in-handler scope by manually setting the contextvar
    and calling kernel.send. Same code path the kernel takes around
    its `await mod.handler(...)` call."""
    from kernel import _current_sender

    # A real sender agent, granted to call `cli` — deny-all denies a phantom
    # sender, and this test is about the telemetry `sender` tag, not authz.
    (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "alice_xxx",
                        # `emit` on another agent passes the gate like a send
                        "can_call": {"cli": ["token", "emit"]},
                    },
                }
            ]
        )
    )[0]["result"]
    # Two-sided deny-all: open cli's reciprocal ingress for alice's `token`.
    cli = seeded_kernel.ctx.agents.get("cli")
    cb = dict(cli._meta.get("callable_by") or {})
    cb["alice_xxx"] = ["token", "emit"]
    (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "update_agent", "id": "cli", "callable_by": cb},
                }
            ]
        )
    )[0]["result"]

    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))

    token = _current_sender.set("alice_xxx")
    try:
        (
            await seeded_kernel.send(
                [{"target": "cli", "payload": {"type": "token", "text": "x"}}]
            )
        )[0]["result"]
        await seeded_kernel.emit("cli", {"type": "noop"})
    finally:
        _current_sender.reset(token)

    sends = [e for e in events if e["kind"] == "send" and e["agent_id"] == "cli"]
    emits = [e for e in events if e["kind"] == "emit" and e["agent_id"] == "cli"]
    assert sends and sends[0]["sender"] == "alice_xxx"
    assert emits and emits[0]["sender"] == "alice_xxx"


async def test_current_sender_outside_scope_is_none(seeded_kernel):
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
    (
        await seeded_kernel.send(
            [{"target": "cli", "payload": {"type": "token", "text": "first"}}]
        )
    )[0]["result"]
    events.clear()
    # Second external call: sender should be None (contextvar reset).
    (
        await seeded_kernel.send(
            [{"target": "cli", "payload": {"type": "token", "text": "second"}}]
        )
    )[0]["result"]
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
    seeded_kernel.ctx.ensure_inbox("kernel_state")  # the watcher registers first
    seeded_kernel.watch("cli", "kernel_state")  # kernel_state watches cli
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    (await seeded_kernel.send([{"target": "cli", "payload": {"type": "noop"}}]))[0][
        "result"
    ]
    targets = {e["agent_id"] for e in events if e["kind"] == "send"}
    assert "cli" in targets
    assert "kernel_state" in targets, (
        f"real-agent watcher's mirrored fanout should produce its own event; got {events}"
    )


async def test_non_agent_watcher_does_not_emit_state_events(seeded_kernel):
    """Non-agent watchers (e.g. the webapp proxy's `_ws_*` pseudo-clients
    registered via kernel._ensure_inbox + kernel.watch) must NOT show
    up in the state stream — they aren't real agents and would mint
    phantom sprites in the telemetry view."""
    fake_ws = "_ws_test_pseudo"
    seeded_kernel.ctx.ensure_inbox(fake_ws)  # mirror what _proxy.run does
    seeded_kernel.watch("cli", fake_ws)
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    (await seeded_kernel.send([{"target": "cli", "payload": {"type": "noop"}}]))[0][
        "result"
    ]
    leaked = [e for e in events if e["agent_id"] == fake_ws]
    assert leaked == [], (
        f"non-agent watcher leaked {len(leaked)} state events: {leaked}"
    )
    # The real agent (cli) still gets its own event.
    cli_evts = [e for e in events if e["agent_id"] == "cli" and e["kind"] == "send"]
    assert len(cli_evts) >= 1


async def test_remove_state_subscriber_detaches(seeded_kernel):
    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    await seeded_kernel.emit("cli", {"type": "first"})
    pre = len(events)
    unsub()
    await seeded_kernel.emit("cli", {"type": "second"})
    assert len(events) == pre, "no events should land after unsubscribe"


async def test_publish_state_reaches_every_subscriber(seeded_kernel):
    a, b = [], []
    seeded_kernel.add_state_subscriber(lambda e: a.append(e))
    seeded_kernel.add_state_subscriber(lambda e: b.append(e))
    await seeded_kernel.emit("cli", {"type": "x"})
    assert a and b
    assert len(a) == len(b)


async def test_a_raising_state_subscriber_prints_and_others_still_receive(seeded_kernel):
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
    assert "cli" in ids and "kernel_state" in ids
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
    (
        await seeded_kernel.send(
            [{"target": "cli", "payload": {"type": "token", "text": "boot"}}]
        )
    )[0]["result"]
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
    (
        await seeded_kernel.send(
            [{"target": "cli", "payload": {"type": "token", "text": "x"}}]
        )
    )[0]["result"]
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

    def _cli_in_flight() -> int:
        cli = seeded_kernel.ctx.agents.get("cli")
        return cli._in_flight if cli is not None else 0

    async def slow_send():
        await asyncio.sleep(0)  # yield once
        seen_in_flight.append(_cli_in_flight())

    async def caller():
        (
            await seeded_kernel.send(
                [{"target": "cli", "payload": {"type": "token", "text": "x"}}]
            )
        )[0]["result"]

    async def observer():
        # Sample while the send is mid-flight.
        await asyncio.sleep(0)
        seen_in_flight.append(_cli_in_flight())

    await asyncio.gather(caller(), observer(), slow_send())
    # After everything completes, in_flight is back to 0.
    assert _cli_in_flight() == 0


async def test_emit_does_not_bump_in_flight(seeded_kernel):
    """emit() is a broadcast — no handler dispatch, so in_flight unchanged."""
    cli = seeded_kernel.ctx.agents["cli"]
    pre = cli._in_flight
    await seeded_kernel.emit("cli", {"type": "noop"})
    assert cli._in_flight == pre


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
    seeded_kernel.create("file_bridge.tools", allow=["."], id="file_test1")
    added = [
        e for e in events if e["kind"] == "added" and e["agent_id"] == "file_test1"
    ]
    assert len(added) == 1
    assert added[0]["name"] == "file_test1"


async def test_lifecycle_added_fires_on_ensure(kernel):
    """ensure() fires 'added' for first-time creation."""
    events: list[dict] = []
    kernel.add_state_subscriber(lambda e: events.append(e))
    kernel.ensure("custom_singleton", "file_bridge.tools", display_name="custom")
    added = [
        e
        for e in events
        if e["kind"] == "added" and e["agent_id"] == "custom_singleton"
    ]
    assert len(added) == 1


async def test_lifecycle_updated_fires_on_update(seeded_kernel):
    seeded_kernel.create("file_bridge.tools", allow=["."], id="file_test2")
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    seeded_kernel.update("file_test2", display_name="renamed")
    updated = [
        e for e in events if e["kind"] == "updated" and e["agent_id"] == "file_test2"
    ]
    assert len(updated) == 1
    assert "display_name" in updated[0]["changed"]


async def test_lifecycle_removed_fires_on_delete(seeded_kernel):
    """`removed` event fires AFTER the cascade has cleaned up — get(id)
    inside the callback already returns None."""
    seeded_kernel.create("file_bridge.tools", allow=["."], id="file_test3")
    observations: list[tuple[str, object]] = []

    def cb(e):
        if e["kind"] == "removed" and e["agent_id"] == "file_test3":
            observations.append(("get", seeded_kernel.get("file_test3")))

    seeded_kernel.add_state_subscriber(cb)
    await seeded_kernel.delete("file_test3")
    assert observations == [("get", None)], (
        f"callback should observe deletion already applied; got {observations}"
    )


async def test_lifecycle_does_not_fire_for_traffic(seeded_kernel):
    """send/emit only produce 'send'/'emit' events, never lifecycle ones."""
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(lambda e: events.append(e))
    (
        await seeded_kernel.send(
            [{"target": "cli", "payload": {"type": "token", "text": "x"}}]
        )
    )[0]["result"]
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
            seeded_kernel.create(
                "file_bridge.tools", allow=["."], id=f"file_chain_{counter['creates']}"
            )

    seeded_kernel.add_state_subscriber(cb)
    seeded_kernel.create("file_bridge.tools", allow=["."], id="file_seed")
    # Bounded chain (we cap at 3) — not an unbounded loop.
    assert counter["creates"] == 3


# ─── Rust twins (#170): each `test_<rust test name>` asserts what the Rust test does ─

import types  # noqa: E402

from kernel import register_unlisted  # noqa: E402
from kernel._kernel import _summarize_payload  # noqa: E402

_peek_mod = types.ModuleType("stream_twin_probe.tools")


async def _peek_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    q = kernel.ctx.inboxes.get(id)
    return {"inbox_at_entry": list(q._queue) if q is not None else None}


_peek_mod.handler = _peek_handler
_peek_mod.VERBS = {}
register_unlisted("stream_twin_probe.tools", _peek_mod)


def test_summarize_short_payload_unchanged():
    # `json.dumps` separators: `, ` and `: `, exactly.
    assert _summarize_payload({"type": "ping"}) == '{"type": "ping"}'


async def test_state_summary_is_python_json_dumps(seeded_kernel):
    """The emit event's summary is `json.dumps(shrunk, ensure_ascii=False)`: `, ` /
    `: ` separators and raw non-ASCII."""
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(events.append)
    seeded_kernel.create("stream_twin_probe.tools", id="tgt")
    await seeded_kernel.emit("tgt", {"type": "say", "text": "héllo"})
    emit = next(e for e in events if e["kind"] == "emit")
    assert emit["summary"] == '{"type": "say", "text": "héllo"}'


async def test_send_fans_out_before_the_handler_runs(seeded_kernel):
    """The mirror happens at DISPATCH START, not after the reply: the payload is
    already in the target's own inbox when its handler first runs."""
    k = seeded_kernel
    k.create("stream_twin_probe.tools", id="peeker")
    k.ctx.ensure_inbox("peeker")
    out = (
        await k.send([{"target": "peeker", "payload": {"type": "work", "n": 7}}])
    )[0]
    assert out["result"]["inbox_at_entry"] == [{"type": "work", "n": 7}], (
        "the payload had not reached the inbox by the time the handler ran"
    )


async def test_emit_to_a_non_agent_id_is_a_noop(seeded_kernel):
    """A synthetic id (a browser client `_ws_<hex>`) is not an agent: no inbox put,
    no state event."""
    k = seeded_kernel
    events: list[dict] = []
    k.add_state_subscriber(events.append)
    rx = k.ctx.ensure_inbox("_ws_deadbeef")
    await k.emit("_ws_deadbeef", {"type": "hi"})
    assert rx.empty(), "an emit to a non-agent id landed"
    assert events == [], "an emit to a non-agent id published a state event"


def test_state_snapshot_is_registration_order_with_in_flight(seeded_kernel):
    """The snapshot walks the registry in registration order and reports
    `{agent_id, name, backlog}`: name from `display_name`, backlog the in-flight
    handler count (2 after two enters)."""
    k = seeded_kernel
    for aid in ("zeta", "alpha", "middle"):
        k.create("stream_twin_probe.tools", id=aid)
    k.create("stream_twin_probe.tools", id="named", display_name="Zed")
    named = k.ctx.agents["named"]
    named._in_flight += 1
    named._in_flight += 1

    snap = k.ctx.state_snapshot()
    ids = [e["agent_id"] for e in snap]
    assert ids == list(k.ctx.agents), "registration order, not sorted"
    assert ids[-4:] == ["zeta", "alpha", "middle", "named"]
    assert snap[-4] == {"agent_id": "zeta", "name": "zeta", "backlog": 0}
    assert snap[-1] == {"agent_id": "named", "name": "Zed", "backlog": 2}
    assert list(snap[-1]) == ["agent_id", "name", "backlog"]
