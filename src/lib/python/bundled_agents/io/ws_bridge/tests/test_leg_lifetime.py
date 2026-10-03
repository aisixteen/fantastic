"""A bridge leg across its LIFETIME: a live record change, a sender restart.

Audit 2026-10-03:

  - ARCH-2 — the leg's rules were resolved ONCE at boot and cached, so an
    operator's `update_agent ingress_rule=deny_ingress` on a LIVE leg changed what
    `reflect` reported (sealed) and nothing the gate did: inbound calls kept being
    dispatched until the transport was torn down. The opening direction (the
    readme's own `update_agent <id> ingress_rule=allow_all_ingress`) failed the
    same way. Now the leg reads its rules off the record per frame — the record is
    the truth the moment it is written.
  - IO-5 — a forward's frame id names the execution on the receiver, and it was
    `<leg>-<n>` with `n` counting from 1 in every process. A restarted sender's
    first forward reused `<leg>-1`, which the long-lived receiver had already
    settled: the new write was JOINED to the old one, answered with its recorded
    outcome, and never ran. Now every boot mints a nonce into the ids.
"""

from __future__ import annotations

import asyncio

import pytest

from _testkit import boot_root
from io_bridge import MemoryTransport
from io_bridge import _engine


async def _op(k, payload) -> dict:
    return (await k.send([{"target": k.id, "payload": payload}]))[0]["result"]


@pytest.fixture
async def leg(seeded_kernel):
    """One leg on `k`, its raw peer end in hand; `notes` (a child of the leg)
    accepts the leg — an inbound call is the leg's own send."""
    k = seeded_kernel
    k.update(k.id, guid="local-g")
    leg_id = (
        await _op(
            k,
            {
                "type": "create_agent",
                "parent_id": k.id,
                "handler_module": "ws_bridge.tools",
                "can_call": {"$children": "*", "kernel_state": "*"},
                "transport": "memory",
                "ingress_rule": "allow_all_ingress",
            },
        )
    )["id"]
    notes = await _op(
        k,
        {
            "type": "create_agent",
            "parent_id": leg_id,
            "handler_module": "file_bridge.tools",
            "id": "notes",
            "callable_by": {leg_id: "*"},
        },
    )
    assert notes.get("id") == "notes", notes
    mt_leg, mt_peer = MemoryTransport.pair()
    _engine._test_transport_inject[leg_id] = mt_leg
    r = (await k.send([{"target": leg_id, "payload": {"type": "boot"}}]))[0]["result"]
    assert r.get("booted") is True, r
    yield k, leg_id, mt_peer
    _engine._bridges.clear()
    _engine._test_transport_inject.clear()


async def _inbound(mt_peer, n: int) -> dict:
    await mt_peer.send(
        {
            "type": "call",
            "id": f"peer-call-{n}",
            "target": "notes",
            "payload": {"type": "reflect"},
        }
    )
    return (await asyncio.wait_for(mt_peer.recv(), timeout=2.0))["data"]


async def test_resealing_a_live_leg_refuses_the_next_inbound_call(leg):
    k, leg_id, mt_peer = leg
    assert (await _inbound(mt_peer, 1)).get("id") == "notes"
    sealed = await _op(
        k, {"type": "update_agent", "id": leg_id, "ingress_rule": "deny_ingress"}
    )
    assert sealed.get("updated") is True, sealed
    shown = (await k.send([{"target": leg_id, "payload": {"type": "reflect"}}]))[0]
    assert shown["result"]["sealed"] is True, shown
    out = await _inbound(mt_peer, 2)
    assert out.get("reason") == "unauthorized", out
    assert "id" not in out, "a sealed leg dispatches nothing"


async def test_opening_a_live_sealed_leg_admits_the_next_inbound_call(leg):
    k, leg_id, mt_peer = leg
    await _op(k, {"type": "update_agent", "id": leg_id, "ingress_rule": "deny_ingress"})
    assert (await _inbound(mt_peer, 1)).get("reason") == "unauthorized"
    opened = await _op(
        k, {"type": "update_agent", "id": leg_id, "ingress_rule": "allow_all_ingress"}
    )
    assert opened.get("updated") is True, opened
    assert (await _inbound(mt_peer, 2)).get("id") == "notes"


async def _forward_frame(k, leg_id, mt_peer) -> dict:
    """Run one forward and hand back the frame that went on the wire."""
    task = asyncio.create_task(
        k.send(
            [
                {
                    "target": leg_id,
                    "payload": {
                        "type": "forward",
                        "target": "x",
                        "payload": {"type": "reflect"},
                    },
                }
            ]
        )
    )
    frame = await asyncio.wait_for(mt_peer.recv(), timeout=2.0)
    await mt_peer.send({"type": "reply", "id": frame["id"], "data": {"ok": True}})
    assert (await task)[0]["result"] == {"ok": True}
    return frame


async def test_a_live_egress_change_stamps_the_next_frame(leg, monkeypatch):
    k, leg_id, mt_peer = leg
    monkeypatch.setenv("FANTASTIC_GROUP_TOKEN", "grp-1")
    assert "auth_token" not in await _forward_frame(k, leg_id, mt_peer)
    await _op(
        k, {"type": "update_agent", "id": leg_id, "egress_rule": "password_egress"}
    )
    assert (await _forward_frame(k, leg_id, mt_peer))["auth_token"] == "grp-1"
    await _op(k, {"type": "update_agent", "id": leg_id, "egress_rule": "silent_egress"})
    assert "auth_token" not in await _forward_frame(k, leg_id, mt_peer)


async def test_an_unknown_egress_rule_written_live_refuses_the_forward(leg):
    """An egress rule is resolved per forward now, so one the registry does not
    know refuses that forward, loud — nothing goes on the wire."""
    k, leg_id, mt_peer = leg
    await _op(k, {"type": "update_agent", "id": leg_id, "egress_rule": "nope_egress"})
    out = (
        await k.send(
            [
                {
                    "target": leg_id,
                    "payload": {
                        "type": "forward",
                        "target": "x",
                        "payload": {"type": "reflect"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert out == {
        "error": "bridge: bad auth rule: unknown egress rule type 'nope_egress'"
    }, out


# ─── IO-5: a restarted sender never joins a stale execution ─────────────────


@pytest.fixture
def two_kernels(tmp_path, monkeypatch):
    dirs = []
    for name in ("a", "b"):
        d = tmp_path / name
        d.mkdir()
        dirs.append(d)
    monkeypatch.chdir(dirs[0])
    ka = boot_root()
    monkeypatch.chdir(dirs[1])
    kb = boot_root()
    monkeypatch.chdir(tmp_path)
    yield ka, kb
    _engine._bridges.clear()
    _engine._test_transport_inject.clear()


async def _boot_pair(ka, a_id, kb, b_id) -> None:
    mt_a, mt_b = MemoryTransport.pair()
    _engine._test_transport_inject[a_id] = mt_a
    _engine._test_transport_inject[b_id] = mt_b
    for k, i in ((ka, a_id), (kb, b_id)):
        r = (await k.send([{"target": i, "payload": {"type": "reconnect"}}]))[0]
        assert r["result"].get("booted") is True, r


async def _write(ka, a_id, text: str) -> dict:
    return (
        await ka.send(
            [
                {
                    "target": a_id,
                    "payload": {
                        "type": "forward",
                        "target": "doc",
                        "payload": {"type": "write", "path": "doc.txt", "bytes": text},
                    },
                }
            ]
        )
    )[0]["result"]


async def test_a_restarted_sender_never_joins_a_settled_execution(two_kernels):
    ka, kb = two_kernels
    ka.update(ka.id, guid="guid-a")
    kb.update(kb.id, guid="guid-b")
    a_id = (
        await _op(
            ka,
            {
                "type": "create_agent",
                "parent_id": ka.id,
                "handler_module": "ws_bridge.tools",
                "can_call": {"$children": "*", "kernel_state": "*"},
                "transport": "memory",
            },
        )
    )["id"]
    b_id = (
        await _op(
            kb,
            {
                "type": "create_agent",
                "parent_id": kb.id,
                "handler_module": "ws_bridge.tools",
                "can_call": {"$children": "*", "kernel_state": "*"},
                "transport": "memory",
                "ingress_rule": "allow_all_ingress",
            },
        )
    )["id"]
    doc = await _op(
        kb,
        {
            "type": "create_agent",
            "parent_id": b_id,
            "handler_module": "file_bridge.tools",
            "id": "doc",
            "allow": ["."],
            "ingress_rule": "allow_all_ingress",
            "callable_by": {b_id: "*"},
        },
    )
    assert doc.get("id") == "doc", doc
    await _boot_pair(ka, a_id, kb, b_id)
    first = await _write(ka, a_id, "v1")
    assert first.get("size") == 2, first
    # THE SENDER RESTARTS: its process memory (the leg's state) is gone; the
    # receiver keeps running, every settled execution still named.
    await _engine._teardown(a_id)
    _engine._bridges.pop(a_id)
    await _boot_pair(ka, a_id, kb, b_id)
    second = await _write(ka, a_id, "v2-NEW")
    assert second.get("size") == 6, second
    read = (
        await kb.send(
            [{"target": "doc", "payload": {"type": "read", "path": "doc.txt"}}]
        )
    )[0]["result"]
    assert [ln["text"] for ln in read["lines"]] == ["v2-NEW"], read
