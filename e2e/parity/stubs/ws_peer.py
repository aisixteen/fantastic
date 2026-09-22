"""A LIVE peer kernel for `ws_bridge` — the thing round 1 and round 2 never had.

Both rounds pointed every bridge at `ws://127.0.0.1:9/nowhere`, so every A7 item
was measured on the SAME answer: connect refused. Everything past a successful
boot — the correlation id shape, a malformed frame mid-stream, a reply that is
an `error` with no error, an inbound call arriving FROM the peer, a socket the
peer closes under a pending forward — was unmeasured.

This serves `ws://127.0.0.1:<port>/<peer_id>/ws`, the path
`PB/io/ws_bridge/src/ws_bridge/_ws.py:172-178` builds, and answers a SCRIPT: one
entry per inbound frame, in order. Every frame in and out is recorded raw, with
the binary header decoded, because the codec (`[4-byte BE len | header | body]`)
is itself an item.
"""

from __future__ import annotations

import asyncio
import json
import struct
import threading
import time


def decode(data) -> dict:
    """A wire frame as DATA. Never raises: a frame the harness cannot parse is a
    recorded shape, not a crashed stub."""
    if isinstance(data, str):
        try:
            return {"kind": "text", "text": data, "json": json.loads(data)}
        except ValueError:
            return {"kind": "text", "text": data, "json": None}
    raw = bytes(data)
    d = {"kind": "binary", "len": len(raw), "hex": raw.hex()[:4096]}
    if len(raw) >= 4:
        hl = struct.unpack(">I", raw[:4])[0]
        d["head_len"] = hl
        if 4 + hl <= len(raw):
            try:
                d["head"] = json.loads(raw[4:4 + hl].decode("utf-8"))
            except Exception as e:  # noqa: BLE001 - recorded, never raised
                d["head_error"] = repr(e)
            d["body_len"] = len(raw) - 4 - hl
            d["body_hex"] = raw[4 + hl:].hex()[:4096]
    return d


class WSPeer:
    """Script entries, matched against the inbound frame's `type`:

      `reply`        answer `{type:'reply', id, data}` with the given data
      `error_no_err` answer `{type:'error', id}` with NO error field  (A7.34)
      `malformed`    answer `{not json` then a valid reply              (A7.19)
      `silent`       answer nothing at all                     (A7.15/A7.26)
      `close`        close the socket under the caller         (A7.17/A7.30)
      `push_event`   push `{type:'event', src, payload}` before replying (A7.3)
      `push_call`    push an inbound `{type:'call', …}` at the kernel  (A7.4/A7.32)
    """

    def __init__(self, rec, rel: str, port: int, peer_id: str = "peer") -> None:
        self.rec = rec
        self.rel = rel
        self.port = port
        self.peer_id = peer_id
        self.script: list[dict] = []
        self.frames: list[dict] = []
        self.connections = 0
        self.lock = threading.Lock()
        self.t0 = time.monotonic()
        self._loop = None
        self._thread = None
        self._server = None

    def url(self) -> str:
        return f"ws://127.0.0.1:{self.port}/{self.peer_id}/ws"

    def set_script(self, script: list[dict]) -> None:
        with self.lock:
            self.script = list(script)
            self._n = 0

    def log(self, **kw) -> None:
        with self.lock:
            self.frames.append({"t": round(time.monotonic() - self.t0, 3), **kw})

    # ── server ───────────────────────────────────────────────────────────────
    def start(self) -> str:
        self._n = 0
        ready = threading.Event()

        def run() -> None:
            self._loop = asyncio.new_event_loop()
            asyncio.set_event_loop(self._loop)
            self._loop.run_until_complete(self._serve(ready))
            self._loop.run_forever()

        self._thread = threading.Thread(target=run, daemon=True)
        self._thread.start()
        ready.wait(10)
        return self.url()

    async def _serve(self, ready: threading.Event) -> None:
        import websockets

        async def handler(ws):
            with self.lock:
                self.connections += 1
                n = self.connections
            self.log(event="open", conn=n, path=getattr(ws, "request", None)
                     and ws.request.path, headers=dict(getattr(ws, "request", None).headers)
                     if getattr(ws, "request", None) else None)
            try:
                async for message in ws:
                    self.log(event="in", conn=n, frame=decode(message))
                    await self._answer(ws, message, n)
            except Exception as e:  # noqa: BLE001 - a peer closing is data
                self.log(event="read_end", conn=n, error=repr(e))
            self.log(event="close", conn=n)

        self._server = await websockets.serve(handler, "127.0.0.1", self.port, max_size=None)
        ready.set()

    async def _answer(self, ws, message, conn: int) -> None:
        with self.lock:
            step = self._n
            self._n += 1
            plan = self.script[step] if step < len(self.script) else {"do": "reply", "data": {"ok": True}}
        try:
            env = json.loads(message) if isinstance(message, str) else decode(message).get("head") or {}
        except ValueError:
            env = {}
        fid = env.get("id")
        do = plan.get("do")

        for push in plan.get("push_before", []):
            await self._send(ws, conn, push)

        if do == "silent":
            return
        if do == "close":
            await ws.close(code=plan.get("code", 1000), reason=plan.get("reason", "stub close"))
            return
        if do == "malformed":
            await self._send_raw(ws, conn, "{not json")
            await self._send(ws, conn, {"type": "reply", "id": fid, "data": plan.get("data", {"ok": True})})
            return
        if do == "error_no_err":
            await self._send(ws, conn, {"type": "error", "id": fid})
            return
        if do == "error":
            await self._send(ws, conn, {"type": "error", "id": fid, "error": plan.get("error", "stub error")})
            return
        await self._send(ws, conn, {"type": "reply", "id": fid, "data": plan.get("data", {"ok": True})})

    async def _send(self, ws, conn: int, envelope: dict) -> None:
        text = json.dumps(envelope)
        self.log(event="out", conn=conn, frame=decode(text))
        await ws.send(text)

    async def _send_raw(self, ws, conn: int, text: str) -> None:
        self.log(event="out", conn=conn, frame={"kind": "text", "text": text, "json": None})
        await ws.send(text)

    def flush(self) -> None:
        self.rec.json(f"{self.rel}/peer_frames", self.frames)

    def stop(self) -> None:
        if self._loop is not None:
            self._loop.call_soon_threadsafe(self._loop.stop)
