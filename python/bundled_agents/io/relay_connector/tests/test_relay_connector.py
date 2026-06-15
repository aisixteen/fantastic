"""relay_connector unit tests — the relay-tunnel transport + the shared engine.

No real relay/network: a `FakeRelayHub` emulates a binary-capable relay's routing
(a `send` to a target GUID is delivered to that GUID's socket as `{type:"event",
source, payload}`, in the SAME WS frame kind — text or raw binary), and two
`RelayTransport`s are cross-wired over it + injected into the engine. This
exercises the REAL codec wrap/unwrap (text frames + native raw-byte binary
frames, no base64) and the symmetric forward→reply path, transport aside. The
live-relay matrix lives in `integration_tests/relay_e2e`.
"""

from __future__ import annotations

import asyncio
import json

import pytest

from _testkit import boot_root
from io_bridge import ConnectionClosed, decode_frame, encode_frame
from relay_connector import tools as rc
from relay_connector._relay import RelayTransport


class FakeRelayHub:
    """Emulates the relay kernel's routing: a peer's `send` to a target GUID is
    delivered to that GUID's socket as a `source`-tagged `event` (preserving the
    WS frame kind via the shared codec, so raw-byte chunks stay raw). Also
    emulates the DIRECTORY agent (`target:"relay"`): `call list_peers` → a peers
    snapshot reply, `watch` → an `{ok}` ack + a `peer_*` event feed."""

    def __init__(self) -> None:
        self.inboxes: dict[str, asyncio.Queue] = {}
        self.dir_watchers: set[str] = set()
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


async def test_transport_wraps_in_relay_send_and_unwraps_event():
    a, b = _pair("A", "B")
    await a.send(
        {"type": "call", "id": "A:1", "target": "x", "payload": {"type": "reflect"}}
    )
    frame = await b.recv()
    assert frame == {
        "type": "call",
        "id": "A:1",
        "target": "x",
        "payload": {"type": "reflect"},
    }


async def test_transport_carries_raw_bytes_over_json_relay():
    """A bridge frame with raw `bytes` (a read_stream chunk) rides the relay as a
    native BINARY WS frame `[len|header|body]` — bytes stay RAW, round-trip
    intact, no base64."""
    a, b = _pair("A", "B")
    blob = bytes(range(256)) * 4
    await a.send({"type": "reply", "id": "A:7", "data": {"eof": False}, "bytes": blob})
    frame = await b.recv()
    assert frame["bytes"] == blob
    assert frame["data"] == {"eof": False}


async def test_transport_skips_foreign_peer_events():
    """recv() unwraps deliveries FROM the partner (and surfaces directory events
    separately); a FOREIGN peer's event is skipped, not mis-routed as the partner's."""
    hub = FakeRelayHub()
    ws_a = hub.register("A")
    hub.register("B")
    a = RelayTransport(
        dialer=_const_dialer(ws_a), partner_guid="B", reconnect=0, heartbeat=0, ws=ws_a
    )
    # A foreign-peer event (NOT our partner), THEN the real partner delivery.
    await hub.inboxes["A"].put(
        json.dumps(
            {"type": "event", "source": "C", "payload": {"type": "reply", "id": "C:9"}}
        )
    )
    await hub.inboxes["A"].put(
        json.dumps(
            {
                "type": "event",
                "source": "B",
                "payload": {"type": "reply", "id": "A:1", "data": {"ok": True}},
            }
        )
    )
    frame = await a.recv()
    assert frame == {"type": "reply", "id": "A:1", "data": {"ok": True}}


async def test_recv_reconnects_after_drop():
    """A self-healing leg: when the socket drops, `recv()` re-dials with the
    `reconnect` backoff and resumes — a partner delivery on the NEW connection
    completes the pending `recv()`. `connected` tracks the live socket."""
    hub = FakeRelayHub()
    hub.register("B")
    dials: list = []

    async def dialer():
        ws = hub.register("A")  # a FRESH A wire each (re)dial
        dials.append(ws)
        return ws

    a = await RelayTransport.connect(
        "ws://x", "A", "", "B", heartbeat=0, reconnect=0.05, dialer=dialer
    )
    assert len(dials) == 1 and a.is_live  # eager initial dial

    recv_task = asyncio.ensure_future(a.recv())
    await asyncio.sleep(0.01)
    await dials[0].close()  # drop the connection mid-recv
    await asyncio.sleep(0.2)  # > reconnect backoff → recv() re-dials
    assert len(dials) == 2, "recv() should have re-dialed after the drop"
    assert a.is_live

    # a partner delivery on the NEW wire resolves the still-pending recv().
    await hub.inboxes["A"].put(
        json.dumps(
            {"type": "event", "source": "B", "payload": {"type": "reply", "id": "x"}}
        )
    )
    frame = await asyncio.wait_for(recv_task, 1.0)
    assert frame == {"type": "reply", "id": "x"}
    await a.close()
    assert a.is_live is False


async def test_directory_list_peers_and_watch():
    """The directory surface: `list_peers` is a relay-LEVEL call to `target:"relay"`
    (not the partner) returning the peers snapshot; `watch_directory` subscribes and
    `peer_*` events surface via recv() as bridge `event` frames (which the engine
    re-emits on the connector inbox). A background drain drives recv() (the engine's
    read loop in production)."""
    hub = FakeRelayHub()
    ws_a = hub.register("A")
    a = RelayTransport(
        dialer=_const_dialer(ws_a), partner_guid="B", reconnect=0, heartbeat=0, ws=ws_a
    )
    events: list = []

    async def drain():
        try:
            while True:
                events.append(await a.recv())
        except ConnectionClosed:
            pass

    drain_task = asyncio.ensure_future(drain())
    try:
        # list_peers → routed to the directory, reply-correlated.
        snap = await a.list_peers(timeout=1)
        assert snap.get("peers") and snap["peers"][0]["guid"] == "A"

        # watch the directory → ack, then a live peer_status event re-emits via recv.
        ack = await a.watch_directory(timeout=1)
        assert ack == {"ok": True}
        await hub.push_directory(
            {"type": "peer_status", "guid": "C", "status": "yellow"}
        )
        await asyncio.sleep(0.05)
        assert {
            "type": "event",
            "payload": {"type": "peer_status", "guid": "C", "status": "yellow"},
        } in events
    finally:
        await a.close()
        await asyncio.sleep(0)
        drain_task.cancel()


# ─── engine integration: forward through relay_connector.handler ─


@pytest.fixture
def two_kernels(tmp_path, monkeypatch):
    a_dir, b_dir = tmp_path / "a", tmp_path / "b"
    a_dir.mkdir()
    b_dir.mkdir()
    monkeypatch.chdir(a_dir)
    ka = boot_root()
    ka.ensure("cli", "cli.tools", display_name="cli")
    monkeypatch.chdir(b_dir)
    kb = boot_root()
    kb.ensure("cli", "cli.tools", display_name="cli")
    monkeypatch.chdir(tmp_path)
    yield ka, kb
    rc._bridges.clear()
    rc._test_transport_inject.clear()


async def _make_leg(kernel, *, partner, ingress=None):
    rec_payload = {
        "type": "create_agent",
        "handler_module": "relay_connector.tools",
        "transport": "memory",  # the test injects the transport directly
        "relay_url": "ws://relay",
        "guid": "self",
        "partner_guid": partner,
    }
    if ingress is not None:
        rec_payload["ingress_rule"] = ingress
    rec = await kernel.send("kernel_state", rec_payload)
    return rec["id"]


async def test_forward_round_trips_through_relay_tunnel(two_kernels):
    """A's relay_connector forwards to kernel B's root reflect; the reply tunnels
    back over the fake relay — proving relay_connector.handler runs on the shared
    io_bridge engine with the relay transport, symmetric callee role and all."""
    ka, kb = two_kernels
    a_id = await _make_leg(ka, partner="B")
    b_id = await _make_leg(kb, partner="A", ingress="allow_all")

    mt_a, mt_b = _pair("A", "B")
    rc._test_transport_inject[a_id] = mt_a
    rc._test_transport_inject[b_id] = mt_b
    assert (await ka.send(a_id, {"type": "boot"})).get("booted") is True
    assert (await kb.send(b_id, {"type": "boot"})).get("booted") is True

    r = await ka.send(
        a_id,
        {"type": "forward", "target": "kernel_state", "payload": {"type": "reflect"}},
    )
    assert isinstance(r, dict), r
    assert r["id"] == "kernel_state", r
    assert r["tree"]["id"] == "kernel_state"


async def test_forward_denied_when_partner_leg_sealed(two_kernels):
    """B's leg is sealed by default (no ingress_rule) → the tunneled inbound call
    is refused with the teaching denial; A's forward resolves to unauthorized
    rather than dispatching on B."""
    ka, kb = two_kernels
    a_id = await _make_leg(ka, partner="B")
    b_id = await _make_leg(kb, partner="A")  # sealed

    mt_a, mt_b = _pair("A", "B")
    rc._test_transport_inject[a_id] = mt_a
    rc._test_transport_inject[b_id] = mt_b
    await ka.send(a_id, {"type": "boot"})
    await kb.send(b_id, {"type": "boot"})

    r = await ka.send(
        a_id,
        {"type": "forward", "target": "kernel_state", "payload": {"type": "reflect"}},
    )
    assert r.get("reason") == "unauthorized", r


async def test_reflect_fields(two_kernels):
    ka, _ = two_kernels
    rec = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "relay_connector.tools",
            "transport": "relay",
            "relay_url": "ws://relay:9443",
            "guid": "A",
            "partner_guid": "B",
        },
    )
    r = await ka.send(rec["id"], {"type": "reflect"})
    assert r["transport"] == "relay"
    assert r["connected"] is False
    assert r["relay_url"] == "ws://relay:9443"
    assert r["guid"] == "A"
    assert r["partner_guid"] == "B"
    assert r["sealed"] is True  # no ingress_rule ⇒ deny_inbound
    for v in (
        "boot",
        "forward",
        "watch_remote",
        "unwatch_remote",
        "reconnect",
        "reflect",
    ):
        assert v in r["verbs"]
