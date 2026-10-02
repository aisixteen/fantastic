"""CROSS-KERNEL ADDRESSING (owner, 2026-09-30): an agent on a connected peer kernel
is addressed `<guid>|<agent id>`, called with an ordinary `send`, and discovered
through reflect. The KERNEL routes it — a transport only registers the peer it is
connected to (`register_peer`) and carries the frames.

These drive the kernel against a fake route; the web ws leg's real route is
covered in `web_ws/tests/test_peer_calls.py`.
"""

from __future__ import annotations

from kernel import sender_context
from kernel._kernel import PeerRoute

HOST = "h0"  # this kernel's guid, set by hand: the root is not booted here
PAGE = "g1"


def _route(k, calls: list, reply=None, secrets: list | None = None):
    secrets = [] if secrets is None else secrets

    async def call(agent_id, payload, caller, secret):
        calls.append((agent_id, payload, caller))
        secrets.append(secret)
        return reply if reply is not None else {"ok": agent_id}

    route = PeerRoute(via="leg", call=call)
    assert k.ctx.register_peer(PAGE, route) is None
    return route


async def _send(k, target, payload, *, as_=None):
    items = [{"target": target, "payload": payload}]
    if as_ is None:
        return (await k.send(items))[0]
    with sender_context(as_):
        return (await k.send(items))[0]


async def test_an_operator_send_reaches_the_peer_agent_as_this_kernel(seeded_kernel):
    k = seeded_kernel
    k._meta["guid"] = HOST
    calls: list = []
    _route(k, calls, reply={"cells": [1, 2]})
    out = await _send(
        k, f"{PAGE}|canvas", {"type": "around", "x": 1, "y": 2, "radius": 3}
    )
    assert out["status"] == "completed" and out["result"] == {"cells": [1, 2]}, out
    assert calls == [
        ("canvas", {"type": "around", "x": 1, "y": 2, "radius": 3}, f"{HOST}|kernel")
    ]


async def test_the_items_secret_marks_ride_to_the_peer(seeded_kernel):
    """The route gets the item's `secret` paths verbatim (rooted at the item, as
    the peer's item is), so the PEER masks its records; this kernel masks its own."""
    k = seeded_kernel
    k._meta["guid"] = HOST
    calls: list = []
    marks: list = []
    _route(k, calls, reply={"ok": True}, secrets=marks)
    key = "sk-peer-0123456789"
    out = (
        await k.send(
            [
                {
                    "target": f"{PAGE}|creds",
                    "payload": {"type": "set", "fields": {"api_key": key}},
                    "secret": ["payload.fields", "result"],
                }
            ]
        )
    )[0]
    assert out["status"] == "completed", out
    assert calls == [
        ("creds", {"type": "set", "fields": {"api_key": key}}, f"{HOST}|kernel")
    ]
    assert marks == [("payload.fields", "result")], marks
    assert k.ctx.rows[out["pid"]]["payload"]["fields"] == "<secret>"
    # an unmarked call hands the route no marks
    await _send(k, f"{PAGE}|creds", {"type": "reflect"})
    assert marks[-1] == (), marks


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
    assert [c[2] for c in calls] == [f"{HOST}|named", f"{HOST}|wild"]
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


async def test_one_guid_one_live_route(seeded_kernel):
    k = seeded_kernel
    first = _route(k, [])
    other = PeerRoute(via="leg2", call=first.call)
    refused = k.ctx.register_peer(PAGE, other)
    assert refused and "already connected via leg" in refused["error"], refused
    # only the registered route may unregister it
    k.ctx.unregister_peer(PAGE, other)
    assert PAGE in k.ctx.peers
    k.ctx.unregister_peer(PAGE, first)
    assert PAGE not in k.ctx.peers


async def test_a_kernel_with_no_guid_cannot_say_who_is_calling(seeded_kernel):
    k = seeded_kernel
    k._meta.pop("guid", None)
    calls: list = []
    _route(k, calls)
    out = await _send(k, f"{PAGE}|canvas", {"type": "around"})
    assert "no guid yet" in (out["result"] or {}).get("error", ""), out
    assert calls == []


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
    placement rule (a child of yourself is baseline) must not wave it through."""
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
