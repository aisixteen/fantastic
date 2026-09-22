"""A minimal WebSocket client — just enough to WATCH an agent's events.

Why hand-rolled: the suite takes no third-party dependency, and what is needed
here is small and fixed — one upgrade handshake, masked text frames out,
unmasked text frames in. RFC 6455 §5.2 framing, no extensions, no compression,
no fragmentation beyond what a kernel event needs.

What it is FOR: a failed pass announces itself as a `status` event with
`detail.error` on the agent's own stream. In process the connector taps
`root.ctx.inboxes`; over a socket the same events arrive here. Without it a
chain that died mid-flight is indistinguishable from a model that chose to
stop — which is exactly how `agentic.build:swift` read: seven good turns, then
silence, and no way to say why.
"""

from __future__ import annotations

import asyncio
import base64
import json
import os
import struct


class WSWatcher:
    """Watch one agent's event stream over `ws://host/<web_id>/ws`."""

    def __init__(self, port: int, agent_id: str, web_id: str = "web"):
        self.port, self.agent_id, self.web_id = port, agent_id, web_id
        self._r: asyncio.StreamReader | None = None
        self._w: asyncio.StreamWriter | None = None

    async def connect(self) -> None:
        self._r, self._w = await asyncio.open_connection("127.0.0.1", self.port)
        key = base64.b64encode(os.urandom(16)).decode()
        self._w.write(
            f"GET /{self.web_id}/ws HTTP/1.1\r\n"
            f"Host: 127.0.0.1:{self.port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
            .encode())
        await self._w.drain()
        head = await self._r.readuntil(b"\r\n\r\n")
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise ConnectionError(f"ws upgrade refused: {head[:120]!r}")
        # REGISTER as a synthetic watcher of the agent — the frame the kernel's
        # ws face answers with `{type:"event", payload}` pushes.
        await self._send(json.dumps({"type": "watch", "src": self.agent_id}))

    async def _send(self, text: str | bytes, opcode: int = 0x1) -> None:
        body = text.encode() if isinstance(text, str) else text
        n = len(body)
        head = bytearray([0x80 | opcode])  # FIN + opcode (text by default)
        # A CLIENT FRAME IS ALWAYS MASKED (RFC 6455 §5.3) — a server must drop
        # an unmasked one, so this is not optional.
        if n < 126:
            head.append(0x80 | n)
        elif n < (1 << 16):
            head.append(0x80 | 126)
            head += struct.pack(">H", n)
        else:
            head.append(0x80 | 127)
            head += struct.pack(">Q", n)
        mask = os.urandom(4)
        head += mask
        assert self._w is not None
        self._w.write(bytes(head) + bytes(b ^ mask[i % 4] for i, b in enumerate(body)))
        await self._w.drain()

    async def _frame(self) -> str | None:
        """One text frame's payload, or None when the socket closes."""
        r = self._r
        assert r is not None
        try:
            h = await r.readexactly(2)
        except (asyncio.IncompleteReadError, ConnectionError):
            return None
        opcode = h[0] & 0x0F
        masked = bool(h[1] & 0x80)
        n = h[1] & 0x7F
        if n == 126:
            n = struct.unpack(">H", await r.readexactly(2))[0]
        elif n == 127:
            n = struct.unpack(">Q", await r.readexactly(8))[0]
        mask = await r.readexactly(4) if masked else b""
        body = await r.readexactly(n) if n else b""
        if masked:
            body = bytes(b ^ mask[i % 4] for i, b in enumerate(body))
        if opcode == 0x8:  # close
            return None
        if opcode == 0x9:
            # A PING MUST BE ANSWERED (RFC 6455 §5.5.2). Every kernel's ws face
            # pings each 20 s and closes 20 s later with no pong: this watcher
            # used to ignore pings, so it went deaf 40 s into every run and the
            # probes scored only the events of the first 40 s (measured
            # 2026-09-20: rust emitted 7 `compacted`, the probe counted 1).
            await self._send(body, opcode=0xA)
            return ""
        if opcode != 0x1:  # pong/binary — not what we watch for
            return ""
        return body.decode(errors="replace")

    async def events(self):
        """Yield each `{type:"event", payload}` frame's PAYLOAD."""
        while True:
            raw = await self._frame()
            if raw is None:
                return
            if not raw:
                continue
            try:
                frame = json.loads(raw)
            except Exception:
                continue
            if isinstance(frame, dict) and frame.get("type") == "event":
                yield frame.get("payload") or {}

    async def close(self) -> None:
        if self._w is not None:
            self._w.close()
            try:
                await self._w.wait_closed()
            except Exception:
                pass
