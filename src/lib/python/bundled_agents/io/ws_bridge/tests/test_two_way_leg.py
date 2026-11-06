"""THE TWO-WAY LEG (issue #476), on the wire: a `ws_bridge` says `hello {guid,
leg}` first at every boot, and serves an inbound `watch` of a local agent —
gated by its ingress rule and its own `watch` grant — by streaming that agent's
events back until `unwatch`. The far end here is the test, holding the memory
transport's other half. The twin of rust `fantastic-bridge/src/tests_two_way.rs`;
the whole round trip against a real `web_ws` is `web_ws/tests/test_two_way.py`.
"""

from __future__ import annotations

import asyncio

import pytest

from _testkit import boot_root
from io_bridge import MemoryTransport, _engine


@pytest.fixture
def kernel_at(tmp_path, monkeypatch):
    def make(name: str, guid: str | None):
        d = tmp_path / name
        d.mkdir()
        monkeypatch.chdir(d)
        k = boot_root()
        if guid is not None:
            k.update(k.id, guid=guid)
        monkeypatch.chdir(tmp_path)
        return k

    yield make
    _engine._bridges.clear()
    _engine._test_transport_inject.clear()


async def _ask(k, target, payload):
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


async def _next(peer: MemoryTransport) -> dict:
    return await asyncio.wait_for(peer.recv(), 2.0)


async def _until(pred, secs=2.0):
    loop = asyncio.get_running_loop()
    end = loop.time() + secs
    while not pred():
        assert loop.time() < end, "condition not reached"
        await asyncio.sleep(0)


async def _sealed_leg(k, leg="leg"):
    """A ws leg (SEALED, no grants) booted on a memory half, and a plain agent
    `notes` to watch. Returns the far end, its hello already read."""
    k.create("ws_bridge.tools", id=leg, transport="memory", peer_id="far")
    k.create("file_bridge.tools", id="notes")
    mine, peer = MemoryTransport.pair()
    _engine._test_transport_inject[leg] = mine
    assert (await _ask(k, leg, {"type": "boot"})).get("booted") is True
    assert await _next(peer) == {"type": "hello", "guid": "near-g", "leg": leg}
    return peer


async def test_hello_goes_first_at_every_boot_and_only_with_a_guid(kernel_at):
    k = kernel_at("near", "near-g")
    await _sealed_leg(k)
    # a redial says it again, on the new link
    mine, peer = MemoryTransport.pair()
    _engine._test_transport_inject["leg"] = mine
    assert (await _ask(k, "leg", {"type": "reconnect"})).get("booted") is True
    assert await _next(peer) == {"type": "hello", "guid": "near-g", "leg": "leg"}
    # a kernel with no guid has no address to give: the first frame is the call
    bare = kernel_at("bare", None)
    bare.create("ws_bridge.tools", id="leg2", transport="memory", peer_id="far")
    mine2, peer2 = MemoryTransport.pair()
    _engine._test_transport_inject["leg2"] = mine2
    assert (await _ask(bare, "leg2", {"type": "boot"})).get("booted") is True
    fwd = asyncio.create_task(
        _ask(
            bare,
            "leg2",
            {"type": "forward", "target": "x", "payload": {"type": "reflect"}},
        )
    )
    first = await _next(peer2)
    assert first["type"] == "call", first
    await peer2.send({"type": "reply", "id": first["id"], "data": {"ok": True}})
    assert await fwd == {"ok": True}


async def test_an_inbound_watch_is_gated_then_streams_until_unwatch(kernel_at):
    k = kernel_at("near", "near-g")
    peer = await _sealed_leg(k)
    watch = {"type": "watch", "src": "notes"}

    # sealed: the ingress rule refuses, and says so
    await peer.send(dict(watch))
    refused = await _next(peer)
    assert (refused["type"], refused["src"], refused["reason"]) == (
        "error",
        "notes",
        "unauthorized",
    ), refused
    # opened, but the leg holds no `watch` on the agent: the kernel's gate refuses
    k.update("leg", ingress_rule="allow_all_ingress")
    await peer.send(dict(watch))
    ungranted = await _next(peer)
    assert ungranted["reason"] == "unauthorized", ungranted
    assert ungranted["side"] == "egress", ungranted
    # a name that is no agent
    await peer.send({"type": "watch", "src": "nobody"})
    assert (await _next(peer))["reason"] == "unknown_agent"
    assert not k.has_watchers("notes")

    # granted both ends: every event of the agent streams back, stamped `src`
    k.update("leg", can_call={"notes": "*"})
    k.update("notes", callable_by={"leg": "*"})
    await peer.send(dict(watch))
    await _until(lambda: k.has_watchers("notes"))
    await k.emit("notes", {"type": "ping", "n": 1})
    assert await _next(peer) == {
        "type": "event",
        "src": "notes",
        "payload": {"type": "ping", "n": 1},
    }

    # unwatched: an event emitted before the next reply never reaches the wire
    await peer.send({"type": "unwatch", "src": "notes"})
    await peer.send(
        {
            "type": "call",
            "id": "after-unwatch",
            "target": "notes",
            "payload": {"type": "reflect"},
        }
    )
    assert (await _next(peer))["type"] == "reply"
    assert not k.has_watchers("notes")
    await k.emit("notes", {"type": "ping", "n": 2})
    await peer.send(
        {
            "type": "call",
            "id": "after-ping",
            "target": "notes",
            "payload": {"type": "reflect"},
        }
    )
    reply = await _next(peer)
    assert (reply["type"], reply["id"]) == ("reply", "after-ping"), reply


async def test_a_reply_follows_the_events_its_call_produced(kernel_at):
    k = kernel_at("near", "near-g")
    peer = await _sealed_leg(k)
    k.update("leg", ingress_rule="allow_all_ingress", can_call={"notes": "*"})
    k.update("notes", callable_by={"leg": "*"})
    await peer.send({"type": "watch", "src": "notes"})
    # the call frame follows the watch on the same link: the watch is in place
    # first, so the call's own inbound payload is mirrored — and goes out BEFORE
    # the call's reply
    await peer.send(
        {"type": "call", "id": "c1", "target": "notes", "payload": {"type": "reflect"}}
    )
    before = []
    while True:
        f = await _next(peer)
        if f["type"] == "reply":
            assert f["id"] == "c1", f
            break
        before.append(f)
    assert before[:1] == [
        {"type": "event", "src": "notes", "payload": {"type": "reflect"}}
    ], before
    assert all(f["type"] == "event" for f in before), before
    # the link goes: the far side's watch goes with it
    await peer.close()
    await _until(lambda: not k.has_watchers("notes"))
