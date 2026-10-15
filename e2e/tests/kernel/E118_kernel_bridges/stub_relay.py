"""A stub relay for E118: the relay kernel's peer routing, nothing else.

A peer dials `ws://127.0.0.1:<port>/<guid>` with the group password in
`X-Fantastic-Auth` and the `fantastic.relay.v1` subprotocol (refused 401 without
the password). `{type:"send", target:<guid>, payload}` is delivered to that guid's
socket as `{type:"event", source:<sender guid>, payload}` — a TEXT frame stays text,
a BINARY frame `[4B BE len | JSON header | raw body]` stays binary with the same
body (its `_binary_path` starts at `payload` in both envelopes). `keepalive` and
directory frames are ignored. It records every dial for the probe.
"""

from __future__ import annotations

import asyncio
import json
import struct
import threading

from websockets.asyncio.server import serve

SUBPROTOCOL = "fantastic.relay.v1"


class StubRelay:
    def __init__(self, password: str):
        self.password = password
        self.binary_routed = 0
        self._dials: list[dict] = []
        self._socks: dict[str, object] = {}
        self._lock = threading.Lock()
        self._loop = asyncio.new_event_loop()
        self._ready = threading.Event()
        self._stop: asyncio.Future | None = None
        self.port = 0
        threading.Thread(target=self._loop.run_until_complete, args=(self._main(),),
                         daemon=True).start()
        if not self._ready.wait(10):
            raise RuntimeError("stub relay did not start")

    def dials(self) -> list[dict]:
        with self._lock:
            return list(self._dials)

    def close(self) -> None:
        if self._stop is not None:
            self._loop.call_soon_threadsafe(self._stop.set_result, None)

    async def _main(self) -> None:
        self._stop = self._loop.create_future()

        def check(conn, request):
            ok = request.headers.get("X-Fantastic-Auth") == self.password
            with self._lock:
                self._dials.append({"guid": request.path.strip("/"), "auth_ok": ok,
                                    "subprotocol": None})
            if not ok:
                return conn.respond(401, "bad password\n")
            return None

        async with serve(self._peer, "127.0.0.1", 0, subprotocols=[SUBPROTOCOL],
                         process_request=check, max_size=2**24) as srv:
            self.port = srv.sockets[0].getsockname()[1] if hasattr(srv, "sockets") else \
                next(iter(srv.server.sockets)).getsockname()[1]
            self._ready.set()
            await self._stop

    async def _peer(self, ws) -> None:
        guid = ws.request.path.strip("/")
        with self._lock:
            for d in reversed(self._dials):
                if d["guid"] == guid and d["subprotocol"] is None:
                    d["subprotocol"] = ws.subprotocol
                    break
        self._socks[guid] = ws
        try:
            async for raw in ws:
                await self._route(guid, raw)
        except Exception:  # noqa: BLE001, S110 — a peer dropping mid-frame ends its route
            pass
        finally:
            if self._socks.get(guid) is ws:
                self._socks.pop(guid, None)

    async def _route(self, sender: str, raw) -> None:
        if isinstance(raw, bytes):
            (n,) = struct.unpack(">I", raw[:4])
            head = json.loads(raw[4:4 + n])
            body = raw[4 + n:]
            if head.get("type") != "send":
                return
            out = json.dumps({"type": "event", "source": sender,
                              "payload": head.get("payload"),
                              "_binary_path": head.get("_binary_path")}).encode()
            dest = self._socks.get(head.get("target"))
            if dest is not None:
                self.binary_routed += 1
                await dest.send(struct.pack(">I", len(out)) + out + body)
            return
        msg = json.loads(raw)
        if msg.get("type") != "send":
            return
        dest = self._socks.get(msg.get("target"))
        if dest is not None:
            await dest.send(json.dumps({"type": "event", "source": sender,
                                        "payload": msg.get("payload")}))
