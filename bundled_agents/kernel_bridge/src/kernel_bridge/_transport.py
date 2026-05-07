"""kernel_bridge transports — abstract `send(frame) / recv()` shim.

Two implementations:
  - WSTransport: real `websockets` client connection (used for `ws`
    and `ssh+ws` bridges; for ssh+ws the SSH tunnel is set up
    separately by the bridge state machine before this opens).
  - MemoryTransport: in-process asyncio.Queue pair, two halves
    cross-wired via `MemoryTransport.pair()`. Lets unit tests cover
    the whole forward round-trip without touching network or
    subprocesses.

Both expose:
    async def send(frame: dict) -> None
    async def recv() -> dict           # raises ConnectionClosed when peer closed
    async def close() -> None
    @property
    closed: bool

The shape of `frame` matches the existing webapp/_proxy.py wire
protocol — `{type:'call', target, payload, id}` and
`{type:'reply', id, data}` — so a WSTransport against a real
fantastic webapp's `/<id>/ws` endpoint just works.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any


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


class WSTransport(_BaseTransport):
    """websockets client connection wrapper. Frames serialize as
    JSON text (matches webapp/_proxy.py default mode — binary path
    is reserved for byte-heavy payloads via the kernel's
    binary_protocol; bridges don't currently mint binary)."""

    def __init__(self, ws: Any) -> None:
        self._ws = ws

    @classmethod
    async def connect(cls, url: str) -> "WSTransport":
        # Local import so MemoryTransport-only test environments don't
        # require websockets installed (it IS in the bundle deps, but
        # this keeps the import surface small for the in-memory path).
        import websockets

        ws = await websockets.connect(url, max_size=2**24)
        return cls(ws)

    @property
    def closed(self) -> bool:
        # websockets >= 12 exposes State, but `.closed` is the stable
        # cross-version flag.
        return getattr(self._ws, "closed", False)

    async def send(self, frame: dict) -> None:
        try:
            await self._ws.send(json.dumps(frame, default=str))
        except Exception as e:
            raise ConnectionClosed(str(e)) from e

    async def recv(self) -> dict:
        import websockets

        try:
            raw = await self._ws.recv()
        except websockets.ConnectionClosed as e:
            raise ConnectionClosed(str(e)) from e
        if isinstance(raw, bytes):
            raw = raw.decode("utf-8")
        return json.loads(raw)

    async def close(self) -> None:
        try:
            await self._ws.close()
        except Exception:
            pass
