"""A call a bridge leg serves is judged as THE LEG (owner 2026-10-04).

Identity comes only from dispatch: the leg's read loop is a task its `boot`
spawned, so every inbound call it serves is the leg's own send — the leg's
`can_call` and the target's `callable_by` decide. Whoever called on the far
kernel is not an identity here: a frame's `caller` field (an old sender kept
sending it) changes nothing, and a `<guid>|…` key in `callable_by` can never
match a caller, so it is refused at the write.
"""

from __future__ import annotations

import asyncio

import pytest

from io_bridge import MemoryTransport
from io_bridge import _engine


@pytest.fixture
async def leg(seeded_kernel):
    k = seeded_kernel
    k.update(k.id, guid="local-g")
    rec = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "handler_module": "ws_bridge.tools",
                        "transport": "memory",
                        "peer_id": "peer",
                        "ingress_rule": "allow_all_ingress",
                        # the leg's calls land on its own children
                        "can_call": {"$children": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    leg_id = rec["id"]
    target = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": leg_id,  # a CHILD of the leg
                        "handler_module": "file_bridge.tools",
                        "id": "notes",
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    mt_leg, mt_peer = MemoryTransport.pair()
    _engine._test_transport_inject[leg_id] = mt_leg
    r = (await k.send([{"target": leg_id, "payload": {"type": "boot"}}]))[0]["result"]
    assert r.get("booted") is True, r
    yield k, leg_id, target, mt_peer
    _engine._bridges.clear()
    _engine._test_transport_inject.clear()


async def _call(mt_peer, caller=None, target="notes", verb="reflect"):
    frame = {"type": "call", "id": "c", "target": target, "payload": {"type": verb}}
    if caller is not None:
        frame["caller"] = caller
    await mt_peer.send(frame)
    return (await asyncio.wait_for(mt_peer.recv(), timeout=2.0))["data"]


def _accept(k, agent_id, key):
    cb = dict(k.ctx.agents[agent_id]._meta.get("callable_by") or {})
    k.update(agent_id, callable_by={**cb, key: "*"})


async def test_an_inbound_call_is_gated_as_the_leg(leg):
    k, leg_id, target, mt_peer = leg
    out = await _call(mt_peer)
    assert out.get("reason") == "unauthorized" and out.get("side") == "ingress", out
    assert leg_id in out["error"], out
    _accept(k, target, leg_id)
    assert (await _call(mt_peer)).get("id") == "notes"


async def test_a_frames_caller_field_changes_nothing(leg):
    k, leg_id, target, mt_peer = leg
    _accept(k, target, leg_id)
    assert (await _call(mt_peer, caller="peer-g|ai")).get("id") == "notes"
    assert (await _call(mt_peer, caller="third-g|x")).get("id") == "notes"


async def test_the_legs_own_can_call_bounds_what_its_peer_reaches(leg):
    k, leg_id, target, mt_peer = leg
    _accept(k, target, leg_id)
    k.update(leg_id, can_call={"$self": "*"})  # the leg no longer reaches its children
    out = await _call(mt_peer)
    assert out.get("reason") == "unauthorized" and out.get("side") == "egress", out


async def test_a_guid_key_in_callable_by_is_refused_at_the_write(leg):
    k, leg_id, target, mt_peer = leg
    out = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "update_agent",
                        "id": target,
                        "callable_by": {"$self": "*", "peer-g|ai": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert out.get("reason") == "invalid_meta", out
    assert out["error"] == (
        "callable_by['peer-g|ai'] is not a grant key — legal keys: an agent id, "
        "$self, $parent, $children"
    ), out
