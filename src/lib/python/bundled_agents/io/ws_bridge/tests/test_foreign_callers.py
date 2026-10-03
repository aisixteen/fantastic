"""A call relayed by a bridge leg is judged by its FOREIGN caller, never the leg.

Audit 2026-09-28: the frame's `caller` was taken on trust (only this kernel's own
guid was refused), so a peer could pose as any third kernel's agent, and a
memory store keyed that tenant's rows by the claim. Now:

  - the leg PINS one peer (`peer_guid`); a caller from any other guid is refused;
  - every inbound call names its caller; no pin, no inbound call;
  - the TARGET's `callable_by` must name the qualified `<guid>|<agent>` (or
    `<guid>|*`) — the structural tokens never match a foreign id, so the
    baseline gives foreign agents nothing, and accepting the LEG is not enough.
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
                        "peer_guid": "peer-g",
                        "ingress_rule": "allow_all_ingress",
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
                        "parent_id": leg_id,  # a CHILD of the leg: baseline accepts the leg
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


async def _call(mt_peer, caller, target="notes", verb="reflect"):
    frame = {"type": "call", "id": "c", "target": target, "payload": {"type": verb}}
    if caller is not None:
        frame["caller"] = caller
    await mt_peer.send(frame)
    return (await asyncio.wait_for(mt_peer.recv(), timeout=2.0))["data"]


def _accept(k, agent_id, key):
    cb = dict(k.ctx.agents[agent_id]._meta.get("callable_by") or {})
    k.update(agent_id, callable_by={**cb, key: "*"})


async def test_accepting_the_leg_is_not_accepting_its_peer(leg):
    k, leg_id, target, mt_peer = leg
    out = await _call(mt_peer, "peer-g|ai")
    assert out.get("reason") == "unauthorized" and out.get("side") == "ingress", out
    assert "peer-g|ai" in out["error"] and "callable_by" in out["hint"], out


async def test_the_target_names_the_foreign_agent(leg):
    k, leg_id, target, mt_peer = leg
    _accept(k, target, "peer-g|ai")
    assert (await _call(mt_peer, "peer-g|ai")).get("id") == "notes"
    other = await _call(mt_peer, "peer-g|other")
    assert other.get("reason") == "unauthorized", other


async def test_guid_star_names_every_agent_of_that_one_kernel(leg):
    k, leg_id, target, mt_peer = leg
    _accept(k, target, "peer-g|*")
    assert (await _call(mt_peer, "peer-g|anyone")).get("id") == "notes"


async def test_a_caller_from_another_kernel_never_reaches_the_gate(leg):
    k, leg_id, target, mt_peer = leg
    _accept(k, target, "third-g|*")  # even a target that accepts it
    out = await _call(mt_peer, "third-g|ai")
    assert out.get("reason") == "unauthorized" and "pinned peer" in out["error"], out


async def test_every_inbound_call_names_its_caller(leg):
    k, leg_id, target, mt_peer = leg
    _accept(k, target, "peer-g|*")
    out = await _call(mt_peer, None)
    assert out.get("reason") == "unauthorized", out


async def test_the_pin_is_not_this_kernels_own_guid(leg):
    k, leg_id, target, mt_peer = leg
    k.update(leg_id, peer_guid="local-g")
    _accept(k, target, "local-g|*")
    out = await _call(mt_peer, "local-g|ai")
    assert out.get("reason") == "unauthorized" and "own guid" in out["error"], out
