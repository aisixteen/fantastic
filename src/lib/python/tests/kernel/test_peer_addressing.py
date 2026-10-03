"""CROSS-KERNEL ADDRESSING (owner, 2026-09-30): an agent on a connected peer kernel
is addressed `<guid>|<agent id>`, called with an ordinary `send`, and discovered
through reflect. The KERNEL routes it — a transport only registers the peer it is
connected to (`register_peer`) and carries the frames.

These drive the kernel against a fake route; the web ws leg's real route is
covered in `web_ws/tests/test_peer_calls.py`.
"""

from __future__ import annotations

import asyncio

from _testkit import as_agent
from kernel._kernel import PeerRoute


async def _no_emit(agent_id, payload):  # these routes carry no events
    raise AssertionError("unexpected emit")


HOST = "h0"  # this kernel's guid, set by hand: the root is not booted here
PAGE = "g1"


def _route(k, calls: list, reply=None):
    async def call(agent_id, payload):
        calls.append((agent_id, payload))
        return reply if reply is not None else {"ok": agent_id}

    route = PeerRoute(via="leg", call=call, emit=_no_emit)
    assert k.ctx.register_peer(PAGE, route) is None
    return route


async def _send(k, target, payload, *, as_=None):
    items = [{"target": target, "payload": payload}]
    if as_ is None:
        return (await k.send(items))[0]
    with as_agent(k, as_):
        return (await k.send(items))[0]


async def test_an_operator_send_reaches_the_peer_agent(seeded_kernel):
    k = seeded_kernel
    k._meta["guid"] = HOST
    calls: list = []
    _route(k, calls, reply={"cells": [1, 2]})
    out = await _send(
        k, f"{PAGE}|canvas", {"type": "around", "x": 1, "y": 2, "radius": 3}
    )
    assert out["status"] == "completed" and out["result"] == {"cells": [1, 2]}, out
    assert calls == [("canvas", {"type": "around", "x": 1, "y": 2, "radius": 3})]


async def test_an_agent_needs_the_qualified_target_in_its_can_call(seeded_kernel):
    k = seeded_kernel
    k._meta["guid"] = HOST
    calls: list = []
    _route(k, calls)
    k.create("file_bridge.tools", id="plain", allow=["."])
    k.create(
        "file_bridge.tools",
        id="named",
        allow=["."],
        can_call={f"{PAGE}|canvas": ["around"]},
    )
    k.create(
        "file_bridge.tools", id="wild", allow=["."], can_call={f"{PAGE}|*": ["around"]}
    )
    refused = await _send(k, f"{PAGE}|canvas", {"type": "around"}, as_="plain")
    r = refused["result"]
    assert (r["reason"], r["side"]) == ("unauthorized", "egress"), r
    assert f"{PAGE}|*" in r["hint"], r
    # a literal grant and the whole-kernel `<guid>|*` both reach it
    for sender in ("named", "wild"):
        ok = await _send(k, f"{PAGE}|canvas", {"type": "around"}, as_=sender)
        assert ok["status"] == "completed", (sender, ok)
    assert len(calls) == 2, calls
    # a verb the grant does not name is refused at egress too
    denied = await _send(k, f"{PAGE}|canvas", {"type": "clear"}, as_="named")
    assert denied["result"]["side"] == "egress", denied


async def test_a_guid_nobody_is_connected_as_is_an_unknown_agent(seeded_kernel):
    k = seeded_kernel
    k._meta["guid"] = HOST
    out = await _send(k, "nobody|canvas", {"type": "reflect"})
    assert out["result"]["reason"] == "unknown_agent", out


async def test_this_kernels_own_guid_names_a_local_agent(seeded_kernel):
    k = seeded_kernel
    k._meta["guid"] = HOST
    out = await _send(k, f"{HOST}|cli", {"type": "reflect"})
    assert out["status"] == "completed" and out["result"].get("id") == "cli", out
    kernel = await _send(k, f"{HOST}|kernel", {"type": "reflect", "tree": "none"})
    assert kernel["result"].get("id") == k.id, kernel


async def test_the_root_reflect_lists_connected_peers(seeded_kernel):
    k = seeded_kernel
    route = _route(k, [])
    r = (await _send(k, "kernel", {"type": "reflect", "tree": "none"}))["result"]
    assert r["peers"] == [{"guid": PAGE, "via": "leg"}], r
    k.ctx.unregister_peer(PAGE, route)
    r = (await _send(k, "kernel", {"type": "reflect", "tree": "none"}))["result"]
    assert r["peers"] == [], r


async def test_one_guid_holds_a_set_of_live_routes(seeded_kernel):
    """The tabs of one identity are ONE principal (owner, 2026-10-03): a second
    route for a live guid is ADDED, and a disconnect removes only its own."""
    k = seeded_kernel
    first = _route(k, [])
    other = PeerRoute(via="leg2", call=first.call, emit=_no_emit)
    assert k.ctx.register_peer(PAGE, other) is None
    k.ctx.register_peer(PAGE, other)  # the same route twice stays one
    assert k.ctx.peer_rows() == [
        {"guid": PAGE, "via": "leg"},
        {"guid": PAGE, "via": "leg2"},
    ]
    k.ctx.unregister_peer(PAGE, other)
    assert k.ctx.peers[PAGE] == [first]
    k.ctx.unregister_peer(PAGE, first)
    assert PAGE not in k.ctx.peers


def _tab(k, name: str, calls: list, answer: asyncio.Future) -> PeerRoute:
    """One TAB of the identity PAGE: records the call, answers when `answer` does."""

    async def call(agent_id, payload):
        calls.append((name, agent_id, payload))
        return await answer

    route = PeerRoute(via=f"leg_{name}", call=call, emit=_no_emit)
    k.ctx.register_peer(PAGE, route)
    return route


async def test_a_call_reaches_every_tab_and_settles_with_the_first_reply(
    seeded_kernel,
):
    k = seeded_kernel
    k._meta["guid"] = HOST
    loop = asyncio.get_running_loop()
    calls: list = []
    a, b = loop.create_future(), loop.create_future()
    _tab(k, "a", calls, a)
    _tab(k, "b", calls, b)
    send = asyncio.create_task(_send(k, f"{PAGE}|canvas", {"type": "around"}))
    while len(calls) < 2:
        await asyncio.sleep(0)
    assert sorted(calls) == [
        ("a", "canvas", {"type": "around"}),
        ("b", "canvas", {"type": "around"}),
    ], calls
    b.set_result({"from": "b"})
    out = await asyncio.wait_for(send, 3)
    assert out["status"] == "completed" and out["result"] == {"from": "b"}, out
    # the slower tab's call is dropped, not left waiting
    await asyncio.sleep(0)
    assert a.cancelled(), a


async def test_a_disconnected_tab_leaves_the_identity_routable(seeded_kernel):
    k = seeded_kernel
    k._meta["guid"] = HOST
    loop = asyncio.get_running_loop()
    calls: list = []
    gone = _tab(k, "a", calls, loop.create_future())
    live = loop.create_future()
    live.set_result({"from": "b"})
    _tab(k, "b", calls, live)
    k.ctx.unregister_peer(PAGE, gone)
    out = await _send(k, f"{PAGE}|canvas", {"type": "around"})
    assert out["result"] == {"from": "b"}, out
    assert [c[0] for c in calls] == ["b"], calls


async def test_a_call_to_a_peer_needs_no_guid_of_this_kernel(seeded_kernel):
    """The frame names no caller: the peer judges the call as the leg that serves
    it there, so this kernel's own guid is not part of the call."""
    k = seeded_kernel
    k._meta.pop("guid", None)
    calls: list = []
    _route(k, calls)
    out = await _send(k, f"{PAGE}|canvas", {"type": "around"})
    assert out["status"] == "completed", out
    assert calls == [("canvas", {"type": "around"})]


async def test_a_reflect_of_a_peer_returns_that_kernels_own_answer(seeded_kernel):
    """Discovery by reflect: the peer's reply comes back as it answered it — it is
    NOT re-rendered here (there is no local agent to render it from)."""
    k = seeded_kernel
    k._meta["guid"] = HOST
    calls: list = []
    _route(k, calls, reply={"id": "kernel_state", "tree": [{"id": "canvas"}]})
    out = await _send(k, f"{PAGE}|kernel", {"type": "reflect"})
    assert out["status"] == "completed", out
    assert out["result"] == {"id": "kernel_state", "tree": [{"id": "canvas"}]}, out
    assert calls[0][:2] == ("kernel", {"type": "reflect"})


async def test_a_create_on_a_peer_is_gated_on_the_peer_target(seeded_kernel):
    """`create_agent` sent to `<guid>|kernel` places nothing in THIS tree: it is an
    ordinary call to the peer, gated on egress like any other — the local
    mutator rule (a child of yourself) must not wave it through."""
    k = seeded_kernel
    k._meta["guid"] = HOST
    calls: list = []
    _route(k, calls)
    k.create("file_bridge.tools", id="maker", allow=["."])
    out = await _send(
        k,
        f"{PAGE}|kernel",
        {"type": "create_agent", "parent_id": "maker", "handler_module": "x.tools"},
        as_="maker",
    )
    assert out["result"]["side"] == "egress", out
    assert calls == []
