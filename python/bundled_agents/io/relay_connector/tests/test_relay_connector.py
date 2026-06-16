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

from _testkit import boot_root, persist
from io_bridge import ConnectionClosed, decode_frame, encode_frame
from relay_connector import _relay as rc_relay
from relay_connector import tools as rc
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


# ─── directory identity (advertise role/owner_guid/exposes) ─────


async def test_announce_on_connect_advertises_identity():
    """A typed leg advertises its directory attrs (opaque blob) to the relay on
    connect — the relay stores them per guid for `list_peers`."""
    hub = FakeRelayHub()
    ws = hub.register("mgr")
    t = await RelayTransport.connect(
        "ws://x",
        "mgr",
        "",
        "B",
        heartbeat=0,
        reconnect=0,
        identity={"role": "manager", "exposes": ["stop", "restart"]},
        dialer=_const_dialer(ws),
    )
    assert hub.attrs.get("mgr") == {"role": "manager", "exposes": ["stop", "restart"]}
    await t.close()


async def test_untyped_leg_sends_no_announce():
    """A plain peer (no typed attrs) advertises NOTHING — byte-identical to a
    pre-typing leg, so the relay's own defaults (role=kernel) stand."""
    hub = FakeRelayHub()
    ws = hub.register("plain")
    t = await RelayTransport.connect(
        "ws://x", "plain", "", "B", heartbeat=0, reconnect=0, dialer=_const_dialer(ws)
    )
    assert "plain" not in hub.attrs
    await t.close()


async def test_set_identity_republishes_and_surfaces_peer_updated():
    """`set_identity` replaces the advertised attrs and re-announces; the relay's
    `peer_updated` event surfaces on the connector inbox (via recv→engine)."""
    hub = FakeRelayHub()
    ws = hub.register("A")
    t = RelayTransport(
        dialer=_const_dialer(ws), partner_guid="B", reconnect=0, heartbeat=0, ws=ws
    )
    events: list = []

    async def drain():
        try:
            while True:
                events.append(await t.recv())
        except ConnectionClosed:
            pass

    drain_task = asyncio.ensure_future(drain())
    try:
        await t.watch_directory(timeout=1)
        await t.set_identity({"role": "manager", "exposes": ["stop"]})
        await asyncio.sleep(0.05)
        assert hub.attrs["A"] == {"role": "manager", "exposes": ["stop"]}
        assert {
            "type": "event",
            "payload": {
                "type": "peer_updated",
                "guid": "A",
                "attrs": {"role": "manager", "exposes": ["stop"]},
                "status": "green",
            },
        } in events
    finally:
        await t.close()
        await asyncio.sleep(0)
        drain_task.cancel()


async def test_reconnect_re_announces_identity():
    """The advertised attrs ride EVERY socket: after a drop, the leg re-dials and
    re-announces (the relay drops per-connection state on disconnect)."""
    hub = FakeRelayHub()
    dials: list = []

    async def dialer():
        ws = hub.register("R")  # fresh wire each (re)dial, same guid
        dials.append(ws)
        return ws

    t = await RelayTransport.connect(
        "ws://x",
        "R",
        "",
        "B",
        heartbeat=0,
        reconnect=0.05,
        identity={"role": "manager"},
        dialer=dialer,
    )
    assert hub.attrs.get("R") == {"role": "manager"}
    hub.attrs.pop("R")  # the relay "forgot" us on the drop

    recv_task = asyncio.ensure_future(t.recv())
    await asyncio.sleep(0.01)
    await dials[0].close()
    await asyncio.sleep(0.2)  # > backoff → recv() re-dials + re-announces
    assert hub.attrs.get("R") == {"role": "manager"}
    recv_task.cancel()
    await t.close()


async def test_set_identity_verb_persists_and_announces(tmp_path, monkeypatch):
    """End-to-end through the handler: a manager leg announces role on connect, then
    `set_identity` publishes a control surface — persisted into the record (so it
    re-announces next boot) and pushed to the relay live."""
    monkeypatch.chdir(tmp_path)
    hub = FakeRelayHub()

    def fake_default_dialer(relay_url, guid, token):
        ws = hub.register(guid)

        async def dial():
            return ws

        return dial

    monkeypatch.setattr(rc_relay, "_default_dialer", fake_default_dialer)

    k = boot_root()
    rec = await k.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "relay_connector.tools",
            "transport": "relay",
            "relay_url": "ws://relay",
            "partner_guid": "B",
            "guid": "mgr",
            "role": "manager",
            "heartbeat": 0,
            "reconnect": 0,
        },
    )
    aid = rec["id"]
    await k.send(aid, {"type": "boot"})
    assert hub.attrs.get("mgr") == {"role": "manager"}  # announced on connect

    r = await k.send(aid, {"type": "set_identity", "exposes": ["stop", "restart"]})
    assert r.get("ok") is True
    assert hub.attrs["mgr"] == {"role": "manager", "exposes": ["stop", "restart"]}
    assert k.get(aid)["exposes"] == ["stop", "restart"]  # persisted for re-announce


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


async def test_guid_persists_across_reboot(tmp_path, monkeypatch):
    """The connector's identity (`guid`) is created ONCE at create_agent and
    survives a kernel reboot. It's a plain record meta field — `build_transport`
    reads it with `rec.get("guid")`, no auto-generation — so the standard
    `.fantastic/agents/<id>/agent.json` persistence carries it. After a reboot in
    the same dir the rehydrated record dials the SAME `ws://host/<guid>` path."""
    monkeypatch.chdir(tmp_path)
    k1 = boot_root()
    rec = await k1.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "relay_connector.tools",
            "transport": "relay",
            "relay_url": "ws://relay:9443",
            "guid": "kernel-A",
            "partner_guid": "kernel-B",
        },
    )
    aid = rec["id"]
    persist(k1)

    # Drop k1, re-bootstrap from the same .fantastic/ dir.
    del k1
    k2 = boot_root()
    assert aid in k2.ctx.agents
    r2 = k2.get(aid)
    assert r2["guid"] == "kernel-A"  # same identity, not regenerated
    assert r2["partner_guid"] == "kernel-B"
    assert r2["relay_url"] == "ws://relay:9443"

    # The rehydrated agent reflects the persisted guid (boot would re-dial it).
    reflected = await k2.send(aid, {"type": "reflect"})
    assert reflected["guid"] == "kernel-A"


async def test_guid_auto_mints_once_and_persists(tmp_path, monkeypatch):
    """A connector created WITHOUT a `guid` auto-mints one on first boot, persists
    it into the record, and re-dials the SAME id on every later hydration — never
    regenerating. (The dialer is stubbed over the fake hub, so no network.)"""
    monkeypatch.chdir(tmp_path)
    hub = FakeRelayHub()

    def fake_default_dialer(relay_url, guid, token):
        ws = hub.register(guid)  # dial succeeds over the in-memory hub

        async def dial():
            return ws

        return dial

    monkeypatch.setattr(rc_relay, "_default_dialer", fake_default_dialer)

    k1 = boot_root()
    rec = await k1.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "relay_connector.tools",
            "transport": "relay",
            "relay_url": "ws://relay",
            "partner_guid": "B",  # the peer address stays explicit (required)
            "heartbeat": 0,
            "reconnect": 0,
            # NO guid — it must be minted.
        },
    )
    aid = rec["id"]
    assert "guid" not in rec or not rec.get("guid")
    # create_agent auto-boots the leg (so the mint already fired); an explicit boot
    # is idempotent. Either path means the transport built and minted our guid.
    booted = await k1.send(aid, {"type": "boot"})
    assert booted.get("booted") or booted.get("already"), booted

    minted = k1.get(aid).get("guid")
    assert minted and len(minted) == 32 and all(c in "0123456789abcdef" for c in minted)
    persist(k1)

    # Reboot from the same dir: the persisted guid is reused verbatim, not regenerated.
    del k1
    k2 = boot_root()
    assert k2.get(aid)["guid"] == minted


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
