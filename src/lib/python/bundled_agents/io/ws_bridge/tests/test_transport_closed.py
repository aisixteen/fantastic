"""A ws leg reports its transport CLOSED once the remote drops.

A websockets 16 `ClientConnection` has no `closed` attribute, so
`getattr(self._ws, "closed", False)` would keep `reflect` saying `connected:true`
and `boot` answering `{already:true}` after the read loop ended (`bridge_down`).
`closed` reads the socket's own state.
"""

from __future__ import annotations

import asyncio

import websockets

from io_bridge import _engine
from ws_bridge._ws import WSTransport


class _Remote:
    """A localhost websockets server that keeps every connection open until the
    test closes it."""

    def __init__(self) -> None:
        self.conns: list = []
        self.accepted = asyncio.Event()

    async def handler(self, conn) -> None:
        self.conns.append(conn)
        self.accepted.set()
        await conn.wait_closed()

    async def drop(self, n: int) -> None:
        await self.conns[n].close()
# twin: closed_follows_the_peer_s_close
# twin: closed_follows_an_abrupt_drop


async def test_closed_follows_the_socket():
    remote = _Remote()
    async with websockets.serve(remote.handler, "127.0.0.1", 0) as server:
        port = server.sockets[0].getsockname()[1]
        t = await WSTransport.connect(f"ws://127.0.0.1:{port}/peer/ws")
        await asyncio.wait_for(remote.accepted.wait(), 2)
        assert t.closed is False
        await remote.drop(0)
        try:
            await asyncio.wait_for(t.recv(), 2)
        except Exception:  # noqa: BLE001 — the drop is what recv reports
            pass
        assert t.closed is True


# twin: reflect_connected_false_after_peer_close_and_boot_rebuilds
async def test_a_dropped_leg_reflects_down_and_boot_dials_again(seeded_kernel):
    k = seeded_kernel
    remote = _Remote()
    async with websockets.serve(remote.handler, "127.0.0.1", 0) as server:
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
                            "peer_id": "peer",
                        },
                    }
                ]
            )
        )[0]["result"]["id"]
        inbox = k.ctx.ensure_inbox("leg_watcher")
        k.ctx.agents[leg]._watcher_ids.add("leg_watcher")

        async def call(payload: dict) -> dict:
            return (await k.send([{"target": leg, "payload": payload}]))[0]["result"]

        try:
            # create_agent booted the leg; a live socket makes `boot` a no-op.
            await asyncio.wait_for(remote.accepted.wait(), 2)
            assert await call({"type": "boot"}) == {"already": True, "transport": "ws"}
            assert (await call({"type": "reflect"}))["connected"] is True

            await remote.drop(0)
            while (await asyncio.wait_for(inbox.get(), 2)).get("type") != "bridge_down":
                pass

            assert (await call({"type": "reflect"}))["connected"] is False
            fwd = await call(
                {"type": "forward", "target": "x", "payload": {"type": "reflect"}}
            )
            assert fwd.get("reason") == "not_connected", fwd

            # boot DIALS AGAIN — no `{already:true}` for a dead socket.
            assert await call({"type": "boot"}) == {"booted": True, "transport": "ws"}
            assert len(remote.conns) == 2
            assert (await call({"type": "reflect"}))["connected"] is True
        finally:
            await _engine._teardown(leg)
            _engine._bridges.pop(leg, None)
