"""Shared FAKE RELAY for tests — the relay kernel's routing emulated in-process.

Extracted from `relay_connector/tests/test_relay_connector.py` (2026-08-25) so the
C21 transport-symmetry suite can drive the SAME cases through the relay tunnel
without importing a test module (xdist double-imports those — measured on
`test_batch_send`, whose cross-test import made every wait time out).
"""

from __future__ import annotations

import asyncio

from io_bridge import decode_frame, encode_frame
from relay_connector._relay import RelayTransport


class FakeRelayHub:
    """Emulates the relay kernel's routing: a peer's `send` to a target GUID is
    delivered to that GUID's socket as a `source`-tagged `event` (preserving the
    WS frame kind via the shared codec, so raw-byte chunks stay raw). Also
    emulates the DIRECTORY agent (`target:"relay"`): `call list_peers` → a peers
    snapshot reply, `watch` → an `{ok}` ack + a `peer_*` event feed, `announce` →
    stores the peer's opaque attrs blob + emits a `peer_updated` event (no reply)."""

    def __init__(self) -> None:
        self.inboxes: dict[str, asyncio.Queue] = {}
        self.dir_watchers: set[str] = set()
        self.attrs: dict[str, dict] = {}  # per-guid advertised directory attrs
        self.peers: list[dict] = [
            {"guid": "A", "status": "green", "last_seen": 1.0, "since": 0.0}
        ]

    def register(self, guid: str) -> "FakeWS":
        self.inboxes[guid] = asyncio.Queue()
        return FakeWS(self, guid)

    async def _deliver(self, guid: str, obj: dict) -> None:
        q = self.inboxes.get(guid)
        if q is not None:
            wire, is_binary = encode_frame(obj)
            await q.put(wire if is_binary else wire.decode("utf-8"))

    async def route(self, sender: str, raw) -> None:
        msg = decode_frame(raw)  # str ⇒ JSON text; bytes ⇒ [len|header|body]
        mtype, target = msg.get("type"), msg.get("target")
        if mtype == "send":
            await self._deliver(
                target,
                {"type": "event", "source": sender, "payload": msg.get("payload")},
            )
        elif target == "relay" and mtype == "call":
            if (msg.get("payload") or {}).get("type") == "list_peers":
                await self._deliver(
                    sender,
                    {
                        "type": "reply",
                        "id": msg.get("id"),
                        "data": {"peers": self.peers},
                    },
                )
        elif target == "relay" and mtype == "watch":
            self.dir_watchers.add(sender)
            await self._deliver(
                sender, {"type": "reply", "id": msg.get("id"), "data": {"ok": True}}
            )
        elif target == "relay" and mtype == "unwatch":
            self.dir_watchers.discard(sender)
        elif target == "relay" and mtype == "announce":
            # Store the opaque attrs blob + broadcast a peer_updated (no reply).
            self.attrs[sender] = msg.get("attrs") or {}
            await self.push_directory(
                {
                    "type": "peer_updated",
                    "guid": sender,
                    "attrs": self.attrs[sender],
                    "status": "green",
                }
            )

    async def push_directory(self, payload: dict) -> None:
        for w in list(self.dir_watchers):
            await self._deliver(
                w, {"type": "event", "source": "relay", "payload": payload}
            )


class _State:
    def __init__(self, name: str) -> None:
        self.name = name


class FakeWS:
    def __init__(self, hub: FakeRelayHub, guid: str) -> None:
        self._hub = hub
        self._guid = guid
        self.state = _State("OPEN")
        self._closed = asyncio.Event()

    async def send(self, raw) -> None:  # raw: str (text) | bytes (binary)
        if self._closed.is_set():
            raise RuntimeError("send on closed ws")
        await self._hub.route(self._guid, raw)

    async def recv(self):
        get = asyncio.ensure_future(self._hub.inboxes[self._guid].get())
        clo = asyncio.ensure_future(self._closed.wait())
        done, pending = await asyncio.wait(
            {get, clo}, return_when=asyncio.FIRST_COMPLETED
        )
        for t in pending:
            t.cancel()
        if get in done:
            return get.result()
        raise RuntimeError("recv on closed ws")  # → transport treats as a drop

    async def close(self) -> None:
        self.state = _State("CLOSED")
        self._closed.set()


def _const_dialer(ws):
    async def dial():
        return ws

    return dial


def _pair(a_guid: str, b_guid: str) -> tuple[RelayTransport, RelayTransport]:
    hub = FakeRelayHub()
    ws_a, ws_b = hub.register(a_guid), hub.register(b_guid)
    # ws preset + reconnect=0 ⇒ a stable one-shot transport over the fake hub.
    return (
        RelayTransport(
            dialer=_const_dialer(ws_a),
            partner_guid=b_guid,
            reconnect=0,
            heartbeat=0,
            ws=ws_a,
        ),
        RelayTransport(
            dialer=_const_dialer(ws_b),
            partner_guid=a_guid,
            reconnect=0,
            heartbeat=0,
            ws=ws_b,
        ),
    )


# ─── transport: wrap/unwrap (text + raw bytes) ──────────────────
