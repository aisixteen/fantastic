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

The shape of `frame` matches the existing web/_proxy.py wire
protocol — `{type:'call', target, payload, id}` and
`{type:'reply', id, data}` — so a WSTransport against a real
fantastic web's `/<id>/ws` endpoint just works.
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
    JSON text (matches web/_proxy.py default mode — binary path
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


class HTTPTransport(_BaseTransport):
    """Request/reply transport against a remote `web_rest` surface.

    The remote URL has the shape `http://<host>/<rest_id>/` (trailing
    slash). For each outbound `call` frame `{type:'call', target,
    payload, id}`, we POST `payload` to `<base_url><target>` and enqueue
    the JSON response as a `reply` frame on our internal recv queue.

    Asymmetric by design: there is no server→client push over plain
    HTTP. `emit`, `watch`, and `state_subscribe` frames raise — use
    `WSTransport` if you need full duplex. HTTPTransport is for
    diagnostics + stateless forwarding.

    Weak binding: this transport addresses the remote surface only by
    URL + path. No shared types with the remote kernel.
    """

    def __init__(self, base_url: str, client: Any) -> None:
        # Normalize trailing slash so `base_url + target` is clean.
        if not base_url.endswith("/"):
            base_url = base_url + "/"
        self._base = base_url
        self._client = client
        self._in: asyncio.Queue = asyncio.Queue()
        self._closed = False

    @classmethod
    async def connect(cls, base_url: str) -> "HTTPTransport":
        # Local import so memory/ws-only test envs don't require httpx
        # (it IS a transitive dep via FastAPI's test client, but keep
        # the import surface small).
        import httpx

        client = httpx.AsyncClient(timeout=30.0)
        return cls(base_url, client)

    @property
    def closed(self) -> bool:
        return self._closed

    async def send(self, frame: dict) -> None:
        if self._closed:
            raise ConnectionClosed("HTTPTransport closed")
        kind = frame.get("type")
        if kind != "call":
            raise NotImplementedError(
                f"HTTPTransport supports only 'call' frames (got {kind!r}); "
                "use WSTransport for emit/watch/state_subscribe."
            )
        # kernel_bridge wraps every outbound call in a `forward` envelope
        # addressed at the peer bridge: frame = {type:call, target:peer,
        # payload:{type:forward, target:<inner>, payload:<inner_payload>}}.
        # HTTP has no peer bridge — the remote `web_rest` dispatches
        # directly. Unwrap the forward to a one-shot REST POST.
        payload = frame.get("payload") or {}
        if isinstance(payload, dict) and payload.get("type") == "forward":
            target = payload.get("target")
            body = payload.get("payload") or {}
        else:
            target = frame.get("target")
            body = payload
        if not target:
            raise ValueError("HTTPTransport.send: target required")
        url = self._base + str(target)
        try:
            resp = await self._client.post(
                url,
                json=body,
                headers={"content-type": "application/json"},
            )
        except Exception as e:
            raise ConnectionClosed(str(e)) from e
        # Translate the HTTP response into a `reply` frame matching the
        # WS protocol shape — the bridge read loop routes it to the
        # pending Future by `id` (which equals corr_id).
        corr_id = frame.get("id")
        if resp.status_code >= 400:
            await self._in.put(
                {
                    "type": "reply",
                    "id": corr_id,
                    "data": {"error": f"HTTP {resp.status_code}: {resp.text[:200]}"},
                }
            )
            return
        try:
            data = resp.json() if resp.content else None
        except json.JSONDecodeError:
            data = {"raw": resp.text}
        await self._in.put({"type": "reply", "id": corr_id, "data": data})

    async def recv(self) -> dict:
        if self._closed and self._in.empty():
            raise ConnectionClosed("HTTPTransport closed")
        return await self._in.get()

    async def close(self) -> None:
        self._closed = True
        try:
            await self._client.aclose()
        except Exception:
            pass
