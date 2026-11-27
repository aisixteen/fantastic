"""A minimal WebSocket client — just enough to WATCH an agent's events.

Why hand-rolled: the suite takes no third-party dependency, and what is needed
here is small and fixed — one upgrade handshake, masked text frames out,
unmasked text frames in. RFC 6455 §5.2 framing, no extensions, no compression,
no fragmentation beyond what a kernel event needs.

What it is FOR: a failed pass announces itself as a `status` event with
`detail.error` on the agent's own stream. In process the connector taps
`root.ctx.inboxes`; over a socket the same events arrive here. Without it a
chain that died mid-flight is indistinguishable from a model that chose to
stop — which is exactly how `agentic.build:rust` read: seven good turns, then
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
            # pings each 20 s and closes 20 s later with no pong: a watcher that
            # ignores pings goes deaf 40 s into every run.
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
            elif isinstance(frame, dict) and frame.get("type") == "error":
                # A REFUSED watch arrives as an error frame and nothing else ever
                # will — skipping it left a watcher silent for a whole run.
                print(f"  ** ws watch {self.agent_id}: {frame.get('error')} **", flush=True)

    async def close(self) -> None:
        if self._w is not None:
            self._w.close()
            try:
                await self._w.wait_closed()
            except Exception:
                pass


class StateWatcher(WSWatcher):
    """The kernel's STATE STREAM over the same ws face: every send's `sender`,
    target (`agent_id`) and full `payload`, for the agents under `node` (`rest`
    unless named). Forensics only — it needs `ws`'s `watch` on `node` (on `rest`
    its seeded `rest: call *`; on the root `HostKernel(watch_root=True)`)."""

    node = "rest"

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
        await self._send(json.dumps({"type": "state_subscribe", "id": "state", "src": self.node}))

    async def state_events(self):
        """Yield each `state_event` frame; a refusal (`error`) is raised."""
        while True:
            raw = await self._frame()
            if raw is None:
                return
            if not raw:
                continue
            try:
                frame = json.loads(raw)
            except json.JSONDecodeError:
                frame = None  # not a json frame — nothing to report
            # ITS OWN refusal only: the socket's connect-time auto-watch of the web
            # host is refused too (the leg holds no line on its parent), as an error
            # frame with no id, and says nothing about the subscription.
            if isinstance(frame, dict) and frame.get("type") == "error" and frame.get("id") == "state":
                raise ConnectionError(f"state_subscribe refused: {frame}")
            if isinstance(frame, dict) and frame.get("type") == "state_event":
                yield frame


class SettleFollower:
    """THE REST LEG'S SETTLES, followed over the ws face (#795, owner 2026-11-27).

    A REST POST answers only its ACK (`{pid, status: "accepted"}`); the leg tells the
    call's result on its OWN watch stream as `{type: "settle", re: <pid>, status,
    data}` (a reply body rides as a binary event, at its slot of `data`). One blocking
    socket per kernel, on a daemon thread: it watches the leg and files every settle
    by `re`; `wait(pid)` takes it — whether it came before or after the ask.

    READY BEFORE THE FIRST POST. The connection's frames take effect in order (the
    `web_ws` order), so an `emit` to the leg sent after the `watch` comes back on the
    watch only once the watch is live: that echo is the ready signal, no sleep.
    """

    def __init__(self, port: int, leg: str = "rest", web_id: str = "web"):
        import socket
        import threading

        self.leg = leg
        self._settles: dict = {}
        self._cond = threading.Condition()
        self._closed = False
        self._sock = socket.create_connection(("127.0.0.1", port), timeout=30)
        key = base64.b64encode(os.urandom(16)).decode()
        self._sock.sendall(
            f"GET /{web_id}/ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self._sock.recv(4096)
            if not chunk:
                raise ConnectionError("ws upgrade: the socket closed")
            head += chunk
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise ConnectionError(f"ws upgrade refused: {head[:120]!r}")
        self._buf = head.split(b"\r\n\r\n", 1)[1]
        self._sock.settimeout(None)
        self._wlock = threading.Lock()
        self._ready = threading.Event()
        self._marker = f"settle-follower-{os.urandom(6).hex()}"
        threading.Thread(target=self._pump, daemon=True).start()
        self._send(json.dumps({"type": "watch", "src": leg}))
        self._send(json.dumps({"type": "emit", "target": leg, "payload": {"type": self._marker}}))
        if not self._ready.wait(30):
            raise ConnectionError(f"the watch on {leg!r} did not come live in 30 s")

    def _send(self, text: str | bytes, opcode: int = 0x1) -> None:
        body = text.encode() if isinstance(text, str) else text
        n = len(body)
        head = bytearray([0x80 | opcode])
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
        with self._wlock:
            self._sock.sendall(bytes(head) + bytes(b ^ mask[i % 4] for i, b in enumerate(body)))

    def _read(self, n: int) -> bytes:
        while len(self._buf) < n:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise ConnectionError("closed")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _frame(self) -> tuple[int, bytes]:
        h = self._read(2)
        opcode, n = h[0] & 0x0F, h[1] & 0x7F
        if n == 126:
            n = struct.unpack(">H", self._read(2))[0]
        elif n == 127:
            n = struct.unpack(">Q", self._read(8))[0]
        mask = self._read(4) if h[1] & 0x80 else b""
        body = self._read(n) if n else b""
        if mask:
            body = bytes(b ^ mask[i % 4] for i, b in enumerate(body))
        return opcode, body

    def _pump(self) -> None:
        try:
            while True:
                opcode, body = self._frame()
                if opcode == 0x8:
                    break
                if opcode == 0x9:
                    self._send(body, opcode=0xA)
                    continue
                if opcode == 0x1:
                    frame = json.loads(body.decode(errors="replace"))
                elif opcode == 0x2:
                    frame = decode_binary(body)
                else:
                    continue
                if not isinstance(frame, dict) or frame.get("type") != "event" or frame.get("src") != self.leg:
                    continue
                p = frame.get("payload") or {}
                if p.get("type") == self._marker:
                    self._ready.set()
                elif p.get("type") == "settle":
                    with self._cond:
                        self._settles[p.get("re")] = p
                        self._cond.notify_all()
        except (ConnectionError, OSError, ValueError):
            pass
        finally:
            with self._cond:
                self._closed = True
                self._cond.notify_all()

    def wait(self, pid, timeout: float) -> dict:
        """The settle of the call `pid` (`{type, re, status, data}`). A HANG GUARD,
        not a pass: past `timeout`, or with the socket gone, it raises."""
        with self._cond:
            ok = self._cond.wait_for(lambda: pid in self._settles or self._closed, timeout)
            if pid in self._settles:
                return self._settles.pop(pid)
        raise TimeoutError(f"no settle for call {pid} in {timeout}s" if not ok else
                           f"the ws face closed before call {pid} settled")

    def close(self) -> None:
        try:
            self._sock.close()
        except OSError:
            pass


def decode_binary(buf: bytes) -> dict:
    """A binary frame `[4B BE head length | head JSON | body]`: the head, with the
    body placed (as `bytes`) at its `_binary_path`."""
    n = struct.unpack(">I", buf[:4])[0]
    head = json.loads(buf[4:4 + n].decode())
    body = buf[4 + n:]
    path = head.pop("_binary_path", None) or []
    if path:
        node = head
        for step in path[:-1]:
            node = node[step]
        node[path[-1]] = body
    return head


def call_answer(frame: dict, fid) -> tuple[bool, object]:
    """One frame read on a socket where the `call` `fid` is open: `(True, answer)`
    when this frame answers it, else `(False, None)`. The `reply` carries the ACK
    (#795) and answers only when it is a refusal at launch (or an `error` frame);
    the answer is the `settle` event that follows: its `data`."""
    if not isinstance(frame, dict):
        return False, None
    p = frame.get("payload") or {}
    if frame.get("type") == "event" and isinstance(p, dict) and p.get("type") == "settle" and p.get("re") == fid:
        return True, p.get("data")
    if frame.get("type") in ("reply", "error") and frame.get("id") == fid:
        d = frame.get("data", frame)
        if frame.get("type") == "reply" and isinstance(d, dict) and d.get("status") == "accepted" and "error" not in d:
            return False, None
        return True, d
    return False, None
