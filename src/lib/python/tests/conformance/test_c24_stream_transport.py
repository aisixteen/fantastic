"""C24 across a transport — the streaming subscriber the production doc named.

`test_stream_semantics.py` pins C24 in-proc: order preserved, terminal closes,
every subscriber sees the identical sequence. This file pins the SAME properties
after the events cross a wire: a remote stream arriving through a ws_bridge
(io_bridge `{type:'event'}` frames over MemoryTransport) must reach local
subscribers in emission order, terminal last, identically fanned out — the
invariant a WS session hub builds on when it subscribes browsers to a kernel's
activity. Every event names its `src` on the wire and arrives as its payload plus
that `src`; only a subscribed `src` (`watch_remote`) flows at all.

Scope note: the SERVING half of a real deployment (`web_ws` answering the
`{type:'watch'}` frame) has its own suite; here the peer transport plays that
role verbatim, which is exactly how the bridge's own unit tests stand it in.
Zero kernel changes — this asserts what already exists.
"""

from __future__ import annotations

import asyncio

from _testkit import boot_root
from io_bridge import MemoryTransport
from io_bridge import _engine

N_EVENTS = 30


async def _bridge(kernel):
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ws_bridge.tools",
                        "transport": "memory",
                        "peer_id": "serving_side",
                    },
                }
            ]
        )
    )[0]["result"]
    a_id = rec["id"]
    mt_a, mt_peer = MemoryTransport.pair()
    _engine._test_transport_inject[a_id] = mt_a
    booted = (await kernel.send([{"target": a_id, "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    assert booted.get("booted") is True
    return a_id, mt_peer


def _events():
    return [{"type": "tick", "n": i} for i in range(N_EVENTS)] + [{"type": "done"}]


async def _drain(kernel, inbox_name, want, timeout=5.0):
    q = kernel.ctx.inboxes[inbox_name]
    out = []
    deadline = asyncio.get_event_loop().time() + timeout
    while len(out) < want:
        left = deadline - asyncio.get_event_loop().time()
        assert left > 0, f"{inbox_name}: got {len(out)}/{want}: {out[-3:]}"
        try:
            out.append(await asyncio.wait_for(q.get(), timeout=left))
        except asyncio.TimeoutError:
            continue
    return out


async def test_a_remote_stream_arrives_in_order_terminal_last(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    k = boot_root()
    a_id, mt_peer = await _bridge(k)

    # the watch handshake goes OUT on the wire first
    r = (
        await k.send(
            [{"target": a_id, "payload": {"type": "watch_remote", "target": "src"}}]
        )
    )[0]["result"]
    assert not (isinstance(r, dict) and r.get("error")), r
    frame = await asyncio.wait_for(mt_peer.recv(), timeout=2)
    assert frame == {"type": "watch", "src": "src"}

    # TWO local subscribers — C24's fan-out claim, across the wire
    for name in ("c24_sub1", "c24_sub2"):
        k.ctx.ensure_inbox(name)
        k.watch(a_id, name)

    # the serving side streams: rapid-fire, no pacing — order must survive
    for ev in _events():
        await mt_peer.send({"type": "event", "src": "src", "payload": ev})

    want = N_EVENTS + 1
    seq1 = await _drain(k, "c24_sub1", want)
    seq2 = await _drain(k, "c24_sub2", want)

    assert seq1 == [{**e, "src": "src"} for e in _events()], (
        "order or content changed crossing the transport"
    )
    assert seq2 == seq1, "two subscribers must see the identical sequence"
    assert seq1[-1] == {"type": "done", "src": "src"}, "terminal must arrive last"
    # nothing follows the terminal within the drained window
    assert all(e["type"] == "tick" for e in seq1[:-1])


async def test_transport_stream_matches_the_inproc_reference(tmp_path, monkeypatch):
    """The C21 method applied to EVENTS: the same emission sequence delivered
    in-proc (kernel.emit on a local agent) and across the bridge must read
    identically to a subscriber — the wire adds exactly the `src` it came from."""
    monkeypatch.chdir(tmp_path)
    k = boot_root()

    # in-proc reference: emit the sequence on a plain agent's inbox
    k.create("cli.tools", id="c24_local")
    k.ctx.ensure_inbox("c24_ref")
    k.watch("c24_local", "c24_ref")
    for ev in _events():
        await k.ctx.agents["c24_local"].emit("c24_local", ev)
    ref = await _drain(k, "c24_ref", N_EVENTS + 1)

    # transport leg
    a_id, mt_peer = await _bridge(k)
    await k.send(
        [{"target": a_id, "payload": {"type": "watch_remote", "target": "c24_local"}}]
    )
    assert (await asyncio.wait_for(mt_peer.recv(), timeout=2))["type"] == "watch"
    k.ctx.ensure_inbox("c24_wire")
    k.watch(a_id, "c24_wire")
    for ev in _events():
        await mt_peer.send({"type": "event", "src": "c24_local", "payload": ev})
    wire = await _drain(k, "c24_wire", N_EVENTS + 1)

    assert wire == [{**e, "src": "c24_local"} for e in ref], (
        "the wire changed what a subscriber sees"
    )
