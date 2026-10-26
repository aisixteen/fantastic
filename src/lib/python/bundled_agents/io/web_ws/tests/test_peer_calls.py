"""The web ws leg as the TRANSPORT of cross-kernel addressing: a page says
`hello {guid}`, the host kernel then reaches the page's agents as `<guid>|<agent>`,
each call rides this socket as an outbound `call` frame answered by an inbound
`reply`/`error` with the same id, and the page stops being an address the moment
its socket closes.

Driven through the real proxy loop with a fake socket, so the host can send INTO
the page from the test's own event loop.
"""

from __future__ import annotations

import asyncio
import json

from starlette.websockets import WebSocketState

from web_ws._proxy import run

HOST = "h0"
PAGE = "g1"


class _FakeSocket:
    """The page's end: frames the proxy SENDS land in `out`; what the test pushes
    into `inbox` is what the proxy RECEIVES."""

    def __init__(self):
        self.application_state = WebSocketState.CONNECTED
        self.inbox: asyncio.Queue = asyncio.Queue()
        self.out: asyncio.Queue = asyncio.Queue()

    async def receive(self):
        return await self.inbox.get()

    async def send_text(self, text):
        await self.out.put(json.loads(text))

    async def send_bytes(self, data):  # not used by these frames
        raise AssertionError("unexpected binary frame")

    def push(self, frame: dict):
        self.inbox.put_nowait({"type": "websocket.receive", "text": json.dumps(frame)})

    def close(self):
        self.inbox.put_nowait({"type": "websocket.disconnect"})

    async def next(self, ftype: str, timeout: float = 3.0) -> dict:
        while True:
            f = await asyncio.wait_for(self.out.get(), timeout)
            if f.get("type") == ftype:
                return f


async def _open(k, *, ingress="allow_all_ingress"):
    k._meta["guid"] = HOST
    k.create("web_ws.tools", id="leg", ingress_rule=ingress)
    ws = _FakeSocket()
    task = asyncio.create_task(run(ws, k, k.id, "leg"))
    return ws, task


async def _until(pred, secs=3.0):
    loop = asyncio.get_running_loop()
    end = loop.time() + secs
    while not pred():
        assert loop.time() < end, "condition not reached"
        await asyncio.sleep(0.01)


async def test_hello_makes_the_page_an_address_and_a_call_round_trips(seeded_kernel):
    k = seeded_kernel
    ws, task = await _open(k)
    try:
        ws.push({"type": "hello", "guid": PAGE})
        await _until(lambda: PAGE in k.ctx.peers)
        assert k.ctx.peer_rows() == [{"guid": PAGE, "via": "leg"}]
        send = asyncio.create_task(
            k.send(
                [
                    {
                        "target": f"{PAGE}|canvas",
                        "payload": {"type": "around", "radius": 2},
                    }
                ]
            )
        )
        call = await ws.next("call")
        assert (call["target"], call["payload"]) == (
            "canvas",
            {"type": "around", "radius": 2},
        ), call
        assert "caller" not in call, call
        ws.push({"type": "reply", "id": call["id"], "data": {"cells": ["a"]}})
        out = (await asyncio.wait_for(send, 3))[0]
        assert out["status"] == "completed" and out["result"] == {"cells": ["a"]}, out
    finally:
        ws.close()
        await asyncio.wait_for(task, 3)


async def test_a_page_side_refusal_is_the_calls_outcome(seeded_kernel):
    k = seeded_kernel
    ws, task = await _open(k)
    try:
        ws.push({"type": "hello", "guid": PAGE})
        await _until(lambda: PAGE in k.ctx.peers)
        send = asyncio.create_task(
            k.send([{"target": f"{PAGE}|canvas", "payload": {"type": "around"}}])
        )
        call = await ws.next("call")
        refusal = {
            "error": "canvas does not accept 'around' from h0|kernel",
            "reason": "unauthorized",
        }
        ws.push({"type": "error", "id": call["id"], "data": refusal})
        out = (await asyncio.wait_for(send, 3))[0]
        assert out["result"] == refusal, out
    finally:
        ws.close()
        await asyncio.wait_for(task, 3)


async def test_a_disconnect_settles_waiting_calls_and_drops_the_address(seeded_kernel):
    k = seeded_kernel
    ws, task = await _open(k)
    ws.push({"type": "hello", "guid": PAGE})
    await _until(lambda: PAGE in k.ctx.peers)
    send = asyncio.create_task(
        k.send([{"target": f"{PAGE}|canvas", "payload": {"type": "around"}}])
    )
    await ws.next("call")
    ws.close()
    await asyncio.wait_for(task, 3)
    out = (await asyncio.wait_for(send, 3))[0]
    assert out["result"]["reason"] == "transport_error", out
    assert "disconnected before answering" in out["result"]["error"], out
    assert PAGE not in k.ctx.peers
    gone = (
        await k.send([{"target": f"{PAGE}|canvas", "payload": {"type": "around"}}])
    )[0]
    assert gone["result"]["reason"] == "unknown_agent", gone


async def _hello_error(ws) -> dict:
    """The next refusal of a `hello` (`arg: guid`) — past the connect-time
    auto-watch's own refusal on this ungranted leg."""
    while True:
        err = await ws.next("error")
        if err.get("arg") == "guid":
            return err


async def test_one_guid_per_connection(seeded_kernel):
    k = seeded_kernel
    ws, task = await _open(k)
    try:
        ws.push({"type": "hello", "guid": PAGE})
        await _until(lambda: PAGE in k.ctx.peers)
        # the same connection cannot become someone else
        ws.push({"type": "hello", "guid": "other"})
        err = await _hello_error(ws)
        assert PAGE in err["error"], err
        # and a malformed guid is refused
        ws.push({"type": "hello", "guid": "a|b"})
        err3 = await _hello_error(ws)
        assert "without `|`" in err3["error"], err3
    finally:
        ws.close()
        await asyncio.wait_for(task, 3)


async def test_two_tabs_of_one_identity_are_one_principal(seeded_kernel):
    """Every tab says hello with the identity's guid; the host
    calls ALL of them, the first reply settles, the other is dropped; a closed tab
    leaves the guid routable through the other."""
    k = seeded_kernel
    ws, task = await _open(k)
    ws2 = _FakeSocket()
    task2 = asyncio.create_task(run(ws2, k, k.id, "leg"))
    try:
        ws.push({"type": "hello", "guid": PAGE})
        ws2.push({"type": "hello", "guid": PAGE})
        await _until(lambda: len(k.ctx.peers.get(PAGE, ())) == 2)
        assert k.ctx.peer_rows() == [{"guid": PAGE, "via": "leg"}] * 2
        send = asyncio.create_task(
            k.send([{"target": f"{PAGE}|canvas", "payload": {"type": "around"}}])
        )
        c1, c2 = await ws.next("call"), await ws2.next("call")
        for c in (c1, c2):
            assert c["target"] == "canvas" and "caller" not in c, c
        ws2.push({"type": "reply", "id": c2["id"], "data": {"tab": 2}})
        out = (await asyncio.wait_for(send, 3))[0]
        assert out["result"] == {"tab": 2}, out
        ws.push({"type": "reply", "id": c1["id"], "data": {"tab": 1}})  # dropped
        # one tab closes: the identity stays an address through the other
        ws.close()
        await asyncio.wait_for(task, 3)
        assert k.ctx.peer_rows() == [{"guid": PAGE, "via": "leg"}]
        again = asyncio.create_task(
            k.send([{"target": f"{PAGE}|canvas", "payload": {"type": "around"}}])
        )
        c3 = await ws2.next("call")
        ws2.push({"type": "reply", "id": c3["id"], "data": {"tab": 2, "n": 2}})
        out = (await asyncio.wait_for(again, 3))[0]
        assert out["result"] == {"tab": 2, "n": 2}, out
    finally:
        ws.close()
        ws2.close()
        await asyncio.wait_for(asyncio.gather(task, task2), 3)
    assert PAGE not in k.ctx.peers


async def test_a_sealed_leg_carries_no_peer(seeded_kernel):
    k = seeded_kernel
    ws, task = await _open(k, ingress="deny_ingress")
    try:
        ws.push({"type": "hello", "guid": PAGE})
        await asyncio.sleep(0.2)
        assert PAGE not in k.ctx.peers
    finally:
        ws.close()
        await asyncio.wait_for(task, 3)


async def test_a_call_racing_the_disconnect_settles_instead_of_hanging(seeded_kernel):
    k = seeded_kernel
    ws, task = await _open(k)
    ws.push({"type": "hello", "guid": PAGE})
    await _until(lambda: PAGE in k.ctx.peers)
    [route] = k.ctx.peers[PAGE]  # resolved BEFORE the close, used after it
    ws.close()
    await asyncio.wait_for(task, 3)
    out = await asyncio.wait_for(route.call("canvas", {"type": "around"}), 3)
    assert out["reason"] == "transport_error", out


async def test_an_emit_to_the_identity_reaches_every_tab(seeded_kernel):
    """An emit on `<guid>|<agent>` goes to EVERY live tab of
    that guid as an `emit` frame."""
    k = seeded_kernel
    ws, task = await _open(k)
    ws2 = _FakeSocket()
    task2 = asyncio.create_task(run(ws2, k, k.id, "leg"))
    try:
        ws.push({"type": "hello", "guid": PAGE})
        ws2.push({"type": "hello", "guid": PAGE})
        await _until(lambda: len(k.ctx.peers.get(PAGE, ())) == 2)
        await k.emit(f"{PAGE}|canvas", {"type": "ping", "n": 1})
        for sock in (ws, ws2):
            assert await sock.next("emit") == {
                "type": "emit",
                "target": "canvas",
                "payload": {"type": "ping", "n": 1},
            }
    finally:
        ws.close()
        ws2.close()
        await asyncio.wait_for(asyncio.gather(task, task2), 3)


async def test_a_reply_is_mirrored_to_the_sibling_tab(seeded_kernel):
    """Tab A's call gets its `reply`; tab B (same hello guid)
    gets `reply_mirror` with the same id and data; a tab of another guid nothing."""
    k = seeded_kernel
    ws, task = await _open(k)
    ws2, ws3 = _FakeSocket(), _FakeSocket()
    task2 = asyncio.create_task(run(ws2, k, k.id, "leg"))
    task3 = asyncio.create_task(run(ws3, k, k.id, "leg"))
    try:
        ws.push({"type": "hello", "guid": PAGE})
        ws2.push({"type": "hello", "guid": PAGE})
        ws3.push({"type": "hello", "guid": "other"})
        await _until(
            lambda: len(k.ctx.peers.get(PAGE, ())) == 2 and "other" in k.ctx.peers
        )
        ws.push(
            {"type": "call", "id": "a-1", "target": "kernel", "payload": {"type": "ps"}}
        )
        reply = await ws.next("reply")
        mirror = await ws2.next("reply_mirror")
        assert reply["id"] == "a-1" and "data" in reply, reply
        assert mirror == {"type": "reply_mirror", "id": "a-1", "data": reply["data"]}
        await asyncio.sleep(0.05)
        rest = [ws3.out.get_nowait() for _ in range(ws3.out.qsize())]
        assert not [f for f in rest if f.get("type") == "reply_mirror"], rest
        rest = [ws.out.get_nowait() for _ in range(ws.out.qsize())]
        assert not [f for f in rest if f.get("type") == "reply_mirror"], rest
    finally:
        for s in (ws, ws2, ws3):
            s.close()
        await asyncio.wait_for(asyncio.gather(task, task2, task3), 3)
