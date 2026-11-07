"""THE AUTOMATIC REDIAL (issue #493), on a real socket: a `ws` leg whose link
dropped dials its known address again by itself — no `reconnect` — keeps the
watches it held, and answers the next call. The far end here is a `websockets`
server the test drops a connection of. Nothing waits on a clock: the test awaits
the leg's own `bridge_down` / `bridge_up` and the next connection, each bounded.
The twin of rust `fantastic-bridge/src/tests_redial.rs`.
"""

from __future__ import annotations

import asyncio
import json

import websockets

from io_bridge import _engine


async def _frame(conn) -> dict:
    return json.loads(await asyncio.wait_for(conn.recv(), 20))


async def test_a_dropped_link_is_redialled_by_the_leg_with_its_watches_and_answers_again(
    seeded_kernel,
):
    k = seeded_kernel
    k.update(k.id, guid="near-g")
    conns: asyncio.Queue = asyncio.Queue()

    async def handler(conn) -> None:
        await conns.put(conn)
        await conn.wait_closed()

    refuse = {"next": False}
    refused = asyncio.Event()

    def process_request(conn, request):
        if refuse["next"]:
            refuse["next"] = False
            refused.set()
            return conn.respond(503, "refused\n")
        return None

    async with websockets.serve(
        handler, "127.0.0.1", 0, process_request=process_request
    ) as server:
        port = server.sockets[0].getsockname()[1]
        leg = (
            await k.send(
                [
                    {
                        "target": k.id,
                        "payload": {
                            "type": "create_agent",
                            "parent_id": k.id,
                            "handler_module": "ws_bridge.tools",
                            "host": "127.0.0.1",
                            "port": port,
                            "peer_id": "far",
                        },
                    }
                ]
            )
        )[0]["result"]["id"]
        inbox = k.ctx.ensure_inbox("leg_watcher")
        k.ctx.agents[leg]._watcher_ids.add("leg_watcher")

        async def call(payload: dict) -> dict:
            return (await k.send([{"target": leg, "payload": payload}]))[0]["result"]

        async def until(kind: str, src: str | None = None) -> dict:
            while True:
                m = await asyncio.wait_for(inbox.get(), 20)
                if m.get("type") == kind and (src is None or m.get("src") == src):
                    return m

        hello = {"type": "hello", "guid": "near-g", "leg": leg}
        try:
            far = await asyncio.wait_for(conns.get(), 20)
            assert await _frame(far) == hello
            r = await call({"type": "watch_remote", "target": "far_agent"})
            assert r.get("ok") is True, r
            assert await _frame(far) == {"type": "watch", "src": "far_agent"}

            # the link drops; the leg's first redial is refused, so it is backing
            # off when the next dial comes
            refuse["next"] = True
            await far.close()
            await until("bridge_down")
            await asyncio.wait_for(refused.wait(), 20)

            # no `reconnect` is sent: the leg dials in by itself, says hello first,
            # and asks again for the agent it was watching
            far = await asyncio.wait_for(conns.get(), 20)
            assert await _frame(far) == hello
            assert await _frame(far) == {"type": "watch", "src": "far_agent"}
            await until("bridge_up")

            # the watch made before the drop keeps delivering
            await far.send(
                json.dumps(
                    {
                        "type": "event",
                        "src": "far_agent",
                        "payload": {"type": "tick", "n": 1},
                    }
                )
            )
            assert (await until("tick", "far_agent"))["n"] == 1

            # and the next call answers
            fwd = asyncio.create_task(
                call({"type": "forward", "target": "x", "payload": {"type": "reflect"}})
            )
            c = await _frame(far)
            assert c["type"] == "call", c
            await far.send(
                json.dumps({"type": "reply", "id": c["id"], "data": {"ok": True}})
            )
            assert await asyncio.wait_for(fwd, 20) == {"ok": True}
        finally:
            await _engine._teardown(leg)
            _engine._bridges.pop(leg, None)


def test_the_redial_backoff_starts_at_once_doubles_and_is_capped():
    secs = [_engine.redial_delay(n) for n in range(9)]
    assert secs == [0, 1, 2, 4, 8, 16, 30, 30, 30]
    assert _engine.redial_delay(10**6) == _engine.REDIAL_MAX_DELAY
