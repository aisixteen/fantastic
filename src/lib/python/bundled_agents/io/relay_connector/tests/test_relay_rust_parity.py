"""Python twins of the Rust relay_connector tests (#170): each is named
`test_<rust test name>` and asserts what the Rust test asserts. Python is the spec."""

from __future__ import annotations

import asyncio
import json

import pytest
import websockets

from _testkit_relay import FakeRelayHub, FakeWS
from io_bridge import ConnectionClosed, MemoryTransport, _engine
from relay_connector._relay import (
    SUBPROTOCOL,
    RelayTransport,
    _identity_from_record,
)


@pytest.fixture(autouse=True)
def _clean_bridges():
    yield
    _engine._bridges.clear()
    _engine._test_transport_inject.clear()


async def _no_password() -> str:
    return ""


async def _op(k, payload, target=None) -> dict:
    return (await k.send([{"target": target or k.id, "payload": payload}]))[0]["result"]


async def _leg(k, **meta) -> str:
    rec = await _op(
        k,
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "relay_connector.tools",
            **meta,
        },
    )
    assert rec.get("id"), rec
    return rec["id"]


# ─── the one frame cap on a dialled relay ────────────────────────


def _of_len(envelope, n: int) -> str:
    base = len(json.dumps(envelope(""), separators=(",", ":")))
    out = json.dumps(envelope("x" * (n - base)), separators=(",", ":"))
    assert len(out) == n
    return out


def _relayed_reply(pad: str) -> dict:
    return {
        "type": "event",
        "source": "B",
        "payload": {"type": "reply", "id": "A:1", "data": {"pad": pad}},
    }


async def _fragmenting_relay(msg: str):
    async def handler(ws):
        half = len(msg) // 2
        await ws.send([msg[:half], msg[half:]])
        try:
            await ws.wait_closed()
        except Exception:
            pass

    server = await websockets.serve(
        handler, "127.0.0.1", 0, compression=None, subprotocols=[SUBPROTOCOL]
    )
    return server, server.sockets[0].getsockname()[1]


async def _one_shot_leg(port: int) -> RelayTransport:
    """A one-shot leg (reconnect 0) dialling `port` as `A`, paired with `B`: a drop
    is terminal, so the end of its socket is observable."""
    return await RelayTransport.connect(
        f"ws://127.0.0.1:{port}", "A", _no_password, "B", heartbeat=0, reconnect=0
    )


async def test_the_relay_transport_bounds_a_fragmented_message_by_the_one_frame_cap():
    # under the cap: reassembled, the partner's reply delivered
    server, port = await _fragmenting_relay(_of_len(_relayed_reply, 4096))
    t = await _one_shot_leg(port)
    frame = await asyncio.wait_for(t.recv(), 20)
    assert frame["id"] == "A:1"
    await t.close()
    server.close()
    await server.wait_closed()

    # one byte over: the socket ends on the cap, nothing is delivered
    server, port = await _fragmenting_relay(_of_len(_relayed_reply, 2**24 + 1))
    t = await _one_shot_leg(port)
    with pytest.raises(ConnectionClosed) as e:
        await asyncio.wait_for(t.recv(), 20)
    assert str(e.value) == "relay_connector: connection dropped"
    await t.close()
    server.close()
    await server.wait_closed()


# ─── the advertised identity ─────────────────────────────────────


def test_identity_from_meta_builds_directory_attrs_blob():
    # well-known keys present → blob
    attrs = _identity_from_record(
        {"role": "manager", "owner_guid": "mgrA", "exposes": ["stop", "restart"]}
    )
    assert attrs == {
        "role": "manager",
        "owner_guid": "mgrA",
        "exposes": ["stop", "restart"],
    }
    # defaults (role=kernel / no owner / empty exposes) are OMITTED: a plain peer
    # advertises nothing
    assert _identity_from_record({}) == {}
    assert _identity_from_record({"role": "kernel", "exposes": []}) == {}


# ─── boot ────────────────────────────────────────────────────────


async def test_relay_boot_missing_partner_mints_no_guid(kernel):
    relay = await _leg(kernel, transport="relay", relay_url="ws://127.0.0.1:1")
    assert await _op(kernel, {"type": "boot"}, relay) == {
        "error": "bridge: relay boot failed: relay_connector requires relay_url, "
        "partner_guid"
    }
    assert (await _op(kernel, {"type": "reflect"}, relay))["guid"] is None, (
        "a refused boot leaves no identity behind"
    )

    # A bad `role` fails the boot with the ROLES text. The guid is minted BEFORE that
    # check, through `kernel.update` — so the `updated` state event the root loader
    # persists from is published.
    seen: list[dict] = []
    kernel.add_state_subscriber(seen.append)
    bad_role = await _leg(
        kernel,
        transport="relay",
        relay_url="ws://127.0.0.1:1",
        partner_guid="B",
        role="boss",
        credentials={"store": "creds", "ref": "relay/group"},
    )
    assert await _op(kernel, {"type": "boot"}, bad_role) == {
        "error": "bridge: relay boot failed: relay_connector role must be one of "
        "('manager', 'kernel'), got 'boss'"
    }
    assert any(
        e.get("agent_id") == bad_role
        and e.get("kind") == "updated"
        and e.get("changed") == ["guid"]
        for e in seen
    ), "the guid mint published no `updated` event"


async def test_a_minted_relay_guid_is_a_uuid4_hex(kernel):
    seen = set()
    for _ in range(4):
        # a bad role fails the boot AFTER the guid is minted
        relay = await _leg(
            kernel,
            transport="relay",
            relay_url="ws://127.0.0.1:1",
            partner_guid="B",
            role="boss",
            credentials={"store": "creds", "ref": "relay/group"},
        )
        await _op(kernel, {"type": "boot"}, relay)
        g = (await _op(kernel, {"type": "reflect"}, relay))["guid"]
        assert isinstance(g, str), f"no guid minted on {relay}"
        assert len(g) == 32, g
        assert all(c in "0123456789abcdef" for c in g), g
        assert g[12] == "4", f"uuid4 version nibble: {g}"
        assert g[16] in "89ab", f"variant: {g}"
        seen.add(g)
    assert len(seen) == 4


async def test_directory_replies_match_python(kernel):
    relay = await _leg(kernel, transport="memory")
    # pre-boot: unwatch_directory is simply ok; the others refuse
    assert await _op(kernel, {"type": "unwatch_directory"}, relay) == {"ok": True}
    r = await _op(kernel, {"type": "list_peers"}, relay)
    assert r["error"] == "relay_connector.list_peers: not connected"
    assert r["reason"] == "not_connected"
    assert r["hint"] == "call boot first"
    # a bad role is a bad_arg refusal naming the arg — the CONNECTION is checked
    # first, so the leg has to be up to reach it
    mt_leg, _mt_peer = MemoryTransport.pair()
    _engine._test_transport_inject[relay] = mt_leg
    assert (await _op(kernel, {"type": "boot"}, relay)).get("booted") is True
    r = await _op(kernel, {"type": "set_identity", "role": "boss"}, relay)
    assert r["error"] == "set_identity: role must be one of ('manager', 'kernel')", r
    assert r["reason"] == "bad_arg"
    assert r["arg"] == "role"


# ─── the transport ───────────────────────────────────────────────


async def test_a_failed_dial_starts_no_heartbeat():
    spawned: list[str] = []

    def spawn(name, coro):
        spawned.append(name)
        return asyncio.create_task(coro)

    async def failing():
        raise OSError("refused")

    t = await RelayTransport.connect(
        "ws://x",
        "A",
        _no_password,
        "B",
        heartbeat=60.0,
        reconnect=1.0,
        dialer=failing,
        spawn=spawn,
    )
    assert spawned == [], f"a beat was spawned with no socket: {spawned}"
    assert not t.is_live

    # …and it starts only on socket open
    hub = FakeRelayHub()

    async def dial():
        return hub.register("A")

    t2 = await RelayTransport.connect(
        "ws://x",
        "A",
        _no_password,
        "B",
        heartbeat=60.0,
        reconnect=1.0,
        dialer=dial,
        spawn=spawn,
    )
    assert spawned == ["relay_heartbeat"]
    assert t2.is_live
    await t2.close()


async def test_a_failed_eager_dial_redials_at_once_and_directory_ids_start_at_one():
    hub = FakeRelayHub()
    hub.register("B")
    dials = 0
    requests: list[dict] = []
    route = hub.route

    async def spy(sender, raw):
        from io_bridge import decode_frame

        msg = decode_frame(raw)
        if msg.get("target") == "relay" and msg.get("type") == "call":
            requests.append(msg)
        await route(sender, raw)

    hub.route = spy

    async def dialer():
        nonlocal dials
        dials += 1
        if dials == 1:
            raise OSError("the eager dial fails")
        return hub.register("A")

    t = await RelayTransport.connect(
        "ws://x", "A", _no_password, "B", heartbeat=0, reconnect=60.0, dialer=dialer
    )
    assert dials == 1 and not t.is_live
    reader = asyncio.ensure_future(t.recv())
    for _ in range(150):
        if t.is_live:
            break
        await asyncio.sleep(0.02)
    assert t.is_live, "the leg waited out its backoff before re-dialling"
    assert dials == 2
    peers = await t.list_peers(5.0)
    assert peers == {"peers": hub.peers}, peers
    assert [r["id"] for r in requests] == ["dir_1"]
    await t.close()
    reader.cancel()


async def test_send_failure_texts_are_pythons():
    async def dialer():
        raise AssertionError("never dialled")

    # no socket
    t = RelayTransport(dialer=dialer, partner_guid="B", reconnect=0, heartbeat=0)
    with pytest.raises(ConnectionClosed) as e:
        await t.send({"type": "ping"})
    assert str(e.value) == "relay_connector: not connected"

    # a failed write: the library's text, no prefix
    class Broken:
        state = None

        async def send(self, raw):
            raise RuntimeError("write blew up")

    t = RelayTransport(
        dialer=dialer, partner_guid="B", reconnect=0, heartbeat=0, ws=Broken()
    )
    with pytest.raises(ConnectionClosed) as e:
        await t.send({"type": "ping"})
    assert str(e.value) == "write blew up"


async def test_close_returns_and_aborts_the_owners_of_the_sink_lock():
    hub = FakeRelayHub()
    ws = hub.register("A")
    assert isinstance(ws, FakeWS)

    async def dialer():
        return ws

    t = RelayTransport(
        dialer=dialer, partner_guid="B", reconnect=0, heartbeat=30.0, ws=ws
    )
    hb = t._hb_task
    assert hb is not None and not hb.done()
    fut = asyncio.get_event_loop().create_future()
    t._relay_pending["dir_1"] = fut
    await asyncio.wait_for(t.close(), 2.0)
    assert t.closed
    await asyncio.sleep(0)
    assert hb.cancelled() or hb.done(), "the heartbeat task must end on close"
    assert fut.done(), "a pending directory request must settle"
    assert fut.result()["reason"] == "transport_closed"
    assert t._relay_pending == {}
