"""relay_connector — the two ways a self-healing leg stopped healing.

1. An ANNOUNCE FAILURE right after a (re)dial ended the loop. `_announce` nulls
   `_ws` when its first send fails; `_reconnect_ws` then returned that None,
   `recv()` read None as "explicitly closed", and the leg went permanently
   `connected:false` on a record that says `reconnect:10`. Only `_closed`
   produces None now.
2. Two verbs sharing ONE precondition answered it in two shapes — `list_peers`
   with `reason:"not_connected"`, `watch_directory`/`set_identity` with a bare
   error string. A caller branching on the code handled one and not the other.
"""

from __future__ import annotations

import asyncio

import pytest

from _testkit_relay import FakeRelayHub
from kernel._refusal import NOT_CONNECTED
from io_bridge import _engine
from relay_connector import tools as rc
from relay_connector._relay import RelayTransport


async def test_announce_failure_after_redial_keeps_healing():
    """A relay that closes the socket between the upgrade and the first frame (a
    restart flap, an auth window) must not end the leg — the next dial heals it."""
    hub = FakeRelayHub()
    hub.register("B")
    dials: list = []

    async def dialer():
        ws = hub.register("A")
        dials.append(ws)
        # The first TWO sockets die the instant they open, so the announce that
        # `_on_socket_open` sends fails and nulls `_ws`.
        if len(dials) <= 2:
            await ws.close()
        return ws

    t = RelayTransport(
        dialer=dialer,
        partner_guid="B",
        reconnect=0.02,
        heartbeat=0,
        identity={"role": "manager"},  # non-empty ⇒ an announce IS sent
    )
    recv_task = asyncio.ensure_future(t.recv())
    # Give the loop enough backoff windows to burn both dead sockets and land a
    # live one. Under the old code the FIRST dead socket ended it for good.
    for _ in range(100):
        await asyncio.sleep(0.02)
        if len(dials) >= 3 and t.is_live:
            break
    assert len(dials) >= 3, f"loop stopped re-dialing after {len(dials)} dial(s)"
    assert t.is_live, "leg never healed onto a live socket"
    assert not recv_task.done(), "recv() gave up instead of waiting on the new wire"

    # The healed wire actually carries a partner delivery.
    await hub._deliver(
        "A", {"type": "event", "source": "B", "payload": {"type": "reply", "id": "x"}}
    )
    frame = await asyncio.wait_for(recv_task, 2.0)
    assert frame == {"type": "reply", "id": "x"}
    await t.close()


async def test_one_shot_leg_still_fails_loud_when_the_socket_dies_on_open():
    """`reconnect<=0` is the explicit one-shot mode — it must RAISE, not spin."""
    hub = FakeRelayHub()

    async def dialer():
        ws = hub.register("A")
        await ws.close()
        return ws

    t = RelayTransport(
        dialer=dialer,
        partner_guid="B",
        reconnect=0,
        heartbeat=0,
        identity={"role": "manager"},
    )
    with pytest.raises(Exception):
        await asyncio.wait_for(t.recv(), 2.0)


@pytest.mark.parametrize(
    "verb", ["list_peers", "watch_directory", "set_identity", "forward"]
)
async def test_every_not_connected_refusal_shares_one_reason_code(verb, tmp_path):
    """One precondition (no transport) ⇒ one machine-readable code, whichever
    verb hit it — plus the hint that repairs it."""
    _engine._bridges.clear()
    payload = {"type": verb}
    if verb == "forward":
        payload = {"type": "forward", "target": "x", "payload": {"type": "reflect"}}
    out = await rc.handler("relay_probe", payload, kernel=None)
    assert out["reason"] == NOT_CONNECTED, out
    assert out["hint"] == "call boot first", out
    _engine._bridges.clear()


@pytest.mark.parametrize("verb", ["watch_remote", "unwatch_remote"])
async def test_engine_watch_verbs_share_the_code_too(verb):
    _engine._bridges.clear()
    out = await rc.handler("relay_probe", {"type": verb, "target": "x"}, kernel=None)
    assert out["reason"] == NOT_CONNECTED, out
    assert out["hint"] == "call boot first", out
    _engine._bridges.clear()
