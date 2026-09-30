"""The web ws leg as the TRANSPORT of cross-kernel addressing: a page says
`hello {guid}`, the host kernel then reaches the page's agents as `<guid>|<agent>`,
each call rides this socket as an outbound `call` frame answered by an inbound
`reply`/`error` with the same id, and the page stops being an address the moment
its socket closes (owner, 2026-09-30).

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
        assert (call["target"], call["payload"], call["caller"]) == (
            "canvas",
            {"type": "around", "radius": 2},
            f"{HOST}|kernel",
        ), call
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


async def test_one_guid_per_connection_and_per_kernel(seeded_kernel):
    k = seeded_kernel
    ws, task = await _open(k)
    ws2 = _FakeSocket()
    task2 = asyncio.create_task(run(ws2, k, k.id, "leg"))
    try:
        ws.push({"type": "hello", "guid": PAGE})
        await _until(lambda: PAGE in k.ctx.peers)
        # the same connection cannot become someone else
        ws.push({"type": "hello", "guid": "other"})
        err = await ws.next("error")
        assert err["arg"] == "guid" and PAGE in err["error"], err
        # another connection cannot take a guid that is live
        ws2.push({"type": "hello", "guid": PAGE})
        err2 = await ws2.next("error")
        assert "already connected via leg" in err2["error"], err2
        # and a malformed guid is refused
        ws2.push({"type": "hello", "guid": "a|b"})
        err3 = await ws2.next("error")
        assert err3["arg"] == "guid", err3
    finally:
        ws.close()
        ws2.close()
        await asyncio.wait_for(asyncio.gather(task, task2), 3)


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
    route = k.ctx.peers[PAGE]  # resolved BEFORE the close, used after it
    ws.close()
    await asyncio.wait_for(task, 3)
    out = await asyncio.wait_for(
        route.call("canvas", {"type": "around"}, f"{HOST}|kernel"), 3
    )
    assert out["reason"] == "transport_error", out
