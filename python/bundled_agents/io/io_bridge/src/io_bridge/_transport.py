"""io_bridge transports — the transport-agnostic abstraction shared by every
bridge bundle (`ws_bridge`, `cloud_bridge`, …).

A transport is the thin `send(frame) / recv()` shim the dispatch engine
(`_engine.py`) talks to. Concrete transports live in the bundles:
  - `ws_bridge._ws.WSTransport` — a `websockets` client to a remote `web_ws`.
  - `cloud_bridge._transport.CloudBridgeTransport` — dial-out relay + Noise E2E.
This module ships only the contract + the in-process `MemoryTransport` that the
whole test suite runs on (no network, no subprocess).

Every transport exposes:
    async def send(frame: dict) -> None
    async def recv() -> dict           # raises ConnectionClosed when peer closed
    async def close() -> None
    @property
    closed: bool

Wire shape is the kernel bridge protocol: `{type:'call', target, payload, id}` /
`{type:'reply', id, data}` / `{type:'watch', src}` / `{type:'event', payload}`.
"""

from __future__ import annotations

import asyncio


class ConnectionClosed(Exception):
    """Raised by recv() when the peer closed the transport."""


class _BaseTransport:
    @property
    def closed(self) -> bool:
        raise NotImplementedError

    async def send(self, frame: dict) -> None:
        raise NotImplementedError

    async def recv(self) -> dict:
        raise NotImplementedError

    async def close(self) -> None:
        raise NotImplementedError


class MemoryTransport(_BaseTransport):
    """In-process pipe — half of a peered pair. `pair()` builds two
    halves whose `send` queues feed the other's `recv` queue."""

    def __init__(
        self,
        out_q: asyncio.Queue,
        in_q: asyncio.Queue,
        peer_close: asyncio.Event,
        own_close: asyncio.Event,
    ) -> None:
        self._out = out_q
        self._in = in_q
        self._peer_close = peer_close
        self._own_close = own_close

    @classmethod
    def pair(cls) -> tuple["MemoryTransport", "MemoryTransport"]:
        """Two halves: A's `send` reaches B's `recv` and vice versa."""
        q_ab: asyncio.Queue = asyncio.Queue()
        q_ba: asyncio.Queue = asyncio.Queue()
        close_a = asyncio.Event()
        close_b = asyncio.Event()
        # A: send→q_ab; recv←q_ba; A's own close→close_a; peer close→close_b
        a = cls(q_ab, q_ba, peer_close=close_b, own_close=close_a)
        b = cls(q_ba, q_ab, peer_close=close_a, own_close=close_b)
        return a, b

    @property
    def closed(self) -> bool:
        return self._own_close.is_set() or self._peer_close.is_set()

    async def send(self, frame: dict) -> None:
        if self.closed:
            raise ConnectionClosed("MemoryTransport closed")
        await self._out.put(frame)

    async def recv(self) -> dict:
        if self.closed and self._in.empty():
            raise ConnectionClosed("MemoryTransport closed")
        # Race: queue.get() vs peer-close event. Whichever fires first.
        get_task = asyncio.create_task(self._in.get())
        close_task = asyncio.create_task(self._peer_close.wait())
        try:
            done, pending = await asyncio.wait(
                {get_task, close_task},
                return_when=asyncio.FIRST_COMPLETED,
            )
            for t in pending:
                t.cancel()
            if get_task in done:
                return get_task.result()
            raise ConnectionClosed("peer closed MemoryTransport")
        finally:
            for t in (get_task, close_task):
                if not t.done():
                    t.cancel()

    async def close(self) -> None:
        self._own_close.set()
