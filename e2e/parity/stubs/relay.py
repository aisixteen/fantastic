"""A LIVE relay for `relay_connector` — same gap as `ws_peer`, same fix.

Accepts `ws://127.0.0.1:<port>/<guid>` with `X-Fantastic-Auth` and the
`fantastic.relay.v1` subprotocol (`PB/io/relay_connector/src/relay_connector/
_relay.py:63-72`), records the `announce` a connector sends on every successful
dial, answers the directory verbs (`list_peers`, `watch`, `unwatch`) and
delivers a `send` envelope back as the partner's `event` — the shape
`RelayTransport.recv` treats as a tunnel delivery.

The whole point is the frames nobody had: an announce, a `watch` reply with NO
data (A7.7), a non-object directory event (A7.3), a heartbeat at 1 s (A7.18/
A7.31) and a deliberate DROP so `reflect` has to say `connected:false` (A7.22).
"""

from __future__ import annotations

import asyncio
import json
import threading
import time

from .ws_peer import decode

SUBPROTOCOL = "fantastic.relay.v1"


class Relay:
    def __init__(self, rec, rel: str, port: int) -> None:
        self.rec = rec
        self.rel = rel
        self.port = port
        self.frames: list[dict] = []
        self.connections = 0
        self.lock = threading.Lock()
        self.t0 = time.monotonic()
        #: Set by the family: after this many inbound frames on a connection the
        #: relay DROPS it, so the reconnect path and `connected:false` are real.
        self.drop_after: int | None = None
        self.partner_events: list[dict] = []
        self._loop = None
        self._server = None

    def url(self) -> str:
        return f"ws://127.0.0.1:{self.port}"

    def log(self, **kw) -> None:
        with self.lock:
            self.frames.append({"t": round(time.monotonic() - self.t0, 3), **kw})

    def start(self) -> str:
        ready = threading.Event()

        def run() -> None:
            self._loop = asyncio.new_event_loop()
            asyncio.set_event_loop(self._loop)
            self._loop.run_until_complete(self._serve(ready))
            self._loop.run_forever()

        threading.Thread(target=run, daemon=True).start()
        ready.wait(10)
        return self.url()

    async def _serve(self, ready: threading.Event) -> None:
        import websockets

        async def handler(ws):
            with self.lock:
                self.connections += 1
                conn = self.connections
            req = getattr(ws, "request", None)
            self.log(event="open", conn=conn, path=req and req.path,
                     headers=dict(req.headers) if req else None,
                     subprotocol=str(getattr(ws, "subprotocol", None)))
            n_in = 0
            try:
                async for message in ws:
                    n_in += 1
                    frame = decode(message)
                    self.log(event="in", conn=conn, frame=frame)
                    env = frame.get("json") or frame.get("head") or {}
                    if self.drop_after is not None and n_in >= self.drop_after:
                        self.log(event="drop", conn=conn, after=n_in)
                        await ws.close(code=1001, reason="stub relay drop")
                        return
                    await self._answer(ws, conn, env)
            except Exception as e:  # noqa: BLE001 - a drop is data
                self.log(event="read_end", conn=conn, error=repr(e))
            self.log(event="close", conn=conn)

        self._server = await websockets.serve(
            handler, "127.0.0.1", self.port, subprotocols=[SUBPROTOCOL], max_size=None)
        ready.set()

    async def _answer(self, ws, conn: int, env: dict) -> None:
        t = env.get("type")
        rid = env.get("id")
        if t == "announce":
            # Fire-and-forget on the connector's side: acknowledged with a
            # DIRECTORY EVENT, plus a deliberately non-object one right after —
            # a peer that only ever sends well-formed events proves nothing.
            await self._send(ws, conn, {"type": "event", "source": "relay",
                                        "payload": {"type": "peer_joined", "guid": env.get("guid")}})
            await self._send(ws, conn, {"type": "event", "source": "relay", "payload": "not-an-object"})
            return
        if t == "call":
            inner = env.get("payload") or {}
            if inner.get("type") == "list_peers":
                await self._send(ws, conn, {"type": "reply", "id": rid, "data": {
                    "peers": [{"guid": "stub-partner", "attrs": {"role": "stub"}}]}})
                return
            await self._send(ws, conn, {"type": "reply", "id": rid,
                                        "data": {"error": f"stub relay: no verb {inner.get('type')!r}"}})
            return
        if t == "watch":
            # NO `data` KEY — A7.7's subject is exactly this reply shape.
            await self._send(ws, conn, {"type": "reply", "id": rid})
            await self._send(ws, conn, {"type": "event", "source": "relay",
                                        "payload": {"type": "dir_1", "note": "stub directory event"}})
            return
        if t == "unwatch":
            await self._send(ws, conn, {"type": "reply", "id": rid, "data": {"ok": True}})
            return
        if t == "send":
            # A tunnel delivery: the partner's answer comes back as an `event`
            # whose source is the partner guid, which is the only shape
            # `RelayTransport.recv` unwraps.
            inner = env.get("payload") or {}
            self.partner_events.append(inner)
            await self._send(ws, conn, {"type": "event", "source": "stub-partner", "payload": {
                "type": "reply", "id": inner.get("id"), "data": {"stub_partner": True}}})
            return
        if t in ("ping", "heartbeat"):
            await self._send(ws, conn, {"type": "pong"})
            return
        await self._send(ws, conn, {"type": "error", "id": rid,
                                    "error": f"stub relay: unknown frame {t!r}"})

    async def _send(self, ws, conn: int, envelope: dict) -> None:
        text = json.dumps(envelope)
        self.log(event="out", conn=conn, frame=decode(text))
        await ws.send(text)

    def flush(self) -> None:
        self.rec.json(f"{self.rel}/relay_frames", self.frames)

    def stop(self) -> None:
        if self._loop is not None:
            self._loop.call_soon_threadsafe(self._loop.stop)
