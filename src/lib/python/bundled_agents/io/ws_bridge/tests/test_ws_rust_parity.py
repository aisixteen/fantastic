"""Python twins of the Rust ws_bridge tests (#170): each is named
`test_<rust test name>` and asserts what the Rust test asserts. Python is the spec;
these pin the behaviour the Rust port copies."""

from __future__ import annotations

import asyncio
import json

import pytest
import websockets

from io_bridge import ConnectionClosed, MemoryTransport, _engine
from ws_bridge._ws import WSTransport


@pytest.fixture(autouse=True)
def _clean_bridges():
    yield
    _engine._bridges.clear()
    _engine._test_transport_inject.clear()


async def _op(k, payload, target=None) -> dict:
    return (await k.send([{"target": target or k.id, "payload": payload}]))[0]["result"]


async def _leg(k, **meta) -> str:
    """One ws_bridge leg under the root; its id."""
    rec = await _op(
        k,
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "ws_bridge.tools",
            "can_call": {"$subtree": "*", "kernel_state": "*"},
            **meta,
        },
    )
    assert rec.get("id"), rec
    return rec["id"]


# ─── the one frame cap on a dialled ws ───────────────────────────


def _of_len(envelope, n: int) -> str:
    """`envelope(pad)` padded to EXACTLY `n` bytes of compact JSON."""
    base = len(json.dumps(envelope(""), separators=(",", ":")))
    out = json.dumps(envelope("x" * (n - base)), separators=(",", ":"))
    assert len(out) == n
    return out


def _reply(pad: str) -> dict:
    return {"type": "reply", "id": "A:1", "data": {"pad": pad}}


async def _fragmenting_peer(msg: str):
    """A ws server that sends `msg` as two fragments (a text frame, then its final
    continuation) and holds the socket open."""

    async def handler(ws):
        half = len(msg) // 2
        await ws.send([msg[:half], msg[half:]])
        try:
            await ws.wait_closed()
        except Exception:
            pass

    server = await websockets.serve(handler, "127.0.0.1", 0, compression=None)
    return server, server.sockets[0].getsockname()[1]


async def test_the_ws_transport_bounds_a_fragmented_message_by_the_one_frame_cap():
    # under the cap: reassembled, delivered whole
    server, port = await _fragmenting_peer(_of_len(_reply, 4096))
    t = await WSTransport.connect(f"ws://127.0.0.1:{port}/peer/ws")
    frame = await asyncio.wait_for(t.recv(), 20)
    assert frame["id"] == "A:1"
    await t.close()
    server.close()
    await server.wait_closed()

    # one byte over: the socket ends on the cap, nothing is delivered
    server, port = await _fragmenting_peer(_of_len(_reply, 2**24 + 1))
    t = await WSTransport.connect(f"ws://127.0.0.1:{port}/peer/ws")
    with pytest.raises(ConnectionClosed) as e:
        await asyncio.wait_for(t.recv(), 20)
    assert "message too big" in str(e.value), str(e.value)
    await t.close()
    server.close()
    await server.wait_closed()


# ─── verbs before boot ───────────────────────────────────────────


async def test_watch_remote_before_boot_errors(kernel):
    leg = await _leg(kernel, transport="memory", peer_id="peer_x")
    r = await _op(kernel, {"type": "watch_remote", "target": "kernel_state"}, leg)
    assert "not connected" in r["error"], r


async def test_unknown_type_is_pythons_reply_with_the_envelope_hint(kernel):
    leg = await _leg(kernel)
    r = await _op(kernel, {"type": "garbage"}, leg)
    assert r["error"] == "bridge: unknown type 'garbage'"
    assert r["hint"] == (
        'every bridge frame needs a "type"; to reach a remote agent use '
        '{"type":"forward","target":"<remote_id>","payload":{"type":"<verb>",...}}'
        ' — the inner payload is itself a verb frame and needs its own "type".'
    )
    # `shutdown` is not in the table either — the teardown lives in on_delete.
    r = await _op(kernel, {"type": "shutdown"}, leg)
    assert r["error"] == "bridge: unknown type 'shutdown'"
    # the directory verbs belong to relay_connector only
    for verb in ("list_peers", "watch_directory", "unwatch_directory", "set_identity"):
        r = await _op(kernel, {"type": verb}, leg)
        assert r["error"] == f"bridge: unknown type '{verb}'", (verb, r)


async def test_boot_error_texts_match_python(kernel):
    # memory with no injection
    mem = await _leg(kernel, transport="memory")
    assert await _op(kernel, {"type": "boot"}, mem) == {
        "error": "bridge: memory transport requires test injection"
    }
    # ws with no peer_id / no port — the ValueError texts, wrapped
    ws = await _leg(kernel)
    assert await _op(kernel, {"type": "boot"}, ws) == {
        "error": "bridge: ws boot failed: peer_id required for the ws transport"
    }
    ws2 = await _leg(kernel, peer_id="p")
    assert await _op(kernel, {"type": "boot"}, ws2) == {
        "error": "bridge: ws boot failed: ws transport requires port"
    }
    # an unknown kind on the ws derivation
    odd = await _leg(kernel, transport="carrier-pigeon", peer_id="p")
    assert await _op(kernel, {"type": "boot"}, odd) == {
        "error": "bridge: carrier-pigeon boot failed: unknown transport 'carrier-pigeon'"
    }
    # an egress rule the registry does not know refuses the boot, before dialing
    bad = await _leg(kernel, transport="memory", egress_rule="nope_egress")
    mt_a, _mt_b = MemoryTransport.pair()
    _engine._test_transport_inject[bad] = mt_a
    r = await _op(kernel, {"type": "boot"}, bad)
    assert r == {
        "error": "bridge: bad auth rule: unknown egress rule type 'nope_egress'"
    }, r


async def test_forward_error_texts_match_python(kernel):
    leg = await _leg(kernel, transport="memory", peer_id="peer")
    # validation
    r = await _op(kernel, {"type": "forward", "target": "x"}, leg)
    assert r == {"error": "bridge.forward: target (str) + payload (dict) required"}
    # pre-boot
    fwd = {"type": "forward", "target": "x", "payload": {"type": "reflect"}}
    r = await _op(kernel, fwd, leg)
    assert r["error"] == "bridge.forward: not connected"
    assert r["reason"] == "not_connected"
    assert r["hint"] == "call boot first"

    mt_leg, mt_peer = MemoryTransport.pair()
    _engine._test_transport_inject[leg] = mt_leg
    assert (await _op(kernel, {"type": "boot"}, leg)).get("booted") is True

    # timeout — the float renders (`0.05s`)
    r = await _op(kernel, {**fwd, "forward_timeout": 0.05}, leg)
    assert r == {"error": "bridge.forward: timeout after 0.05s"}
    await asyncio.wait_for(mt_peer.recv(), 2.0)  # the timed-out call

    # an inbound `error` frame RESOLVES the forward with the remote's text
    async def answer():
        frame = await asyncio.wait_for(mt_peer.recv(), 2.0)
        await mt_peer.send({"type": "error", "id": frame["id"], "error": "boom"})

    answering = asyncio.create_task(answer())
    r = await _op(kernel, {**fwd, "forward_timeout": 5.0}, leg)
    await answering
    assert r == {"error": "remote error: boom"}


async def test_booted_bridge_ps_rows_match_python(kernel):
    leg = await _leg(kernel, transport="memory", peer_id="peer")
    mt_leg, _mt_peer = MemoryTransport.pair()
    _engine._test_transport_inject[leg] = mt_leg
    assert (await _op(kernel, {"type": "boot"}, leg)).get("booted") is True
    await asyncio.sleep(0.1)
    table = await _op(kernel, {"type": "ps"}, "kernel")
    mine = [r for r in table["tasks"] if r["agent"] == leg]
    assert [r["verb"] for r in mine] == ["read_loop"], table
    # the fork is a MESSAGE: one `task` row on the leg, and the pid in `ps` is its id
    rows = [
        r
        for r in kernel.ctx.rows.values()
        if r.get("agent") == leg and r.get("kind") == "task"
    ]
    assert [r["verb"] for r in rows] == ["read_loop"], rows
    assert mine[0]["id"] == rows[0]["id"]


async def test_forward_timeout_is_python_float(kernel):
    leg = await _leg(kernel, transport="memory")

    def fwd(timeout):
        return {
            "type": "forward",
            "target": "x",
            "payload": {"type": "reflect"},
            "forward_timeout": timeout,
        }

    out = (await kernel.send([{"target": leg, "payload": fwd(None)}]))[0]
    assert out["status"] == "failed", out
    assert out["result"]["error"] == (
        "TypeError: float() argument must be a string or a real number, not 'NoneType'"
    )
    out = (await kernel.send([{"target": leg, "payload": fwd("soon")}]))[0]
    assert out["result"]["error"] == (
        "ValueError: could not convert string to float: 'soon'"
    )
    # a numeric string passes the coercion and reaches the connection check
    r = await _op(kernel, fwd("5"), leg)
    assert r["reason"] == "not_connected", r


# ─── a binary call over the bridge ───────────────────────────────


async def test_binary_call_round_trips_raw_bytes_over_the_bridge(kernel, tmp_path):
    fb = await _op(
        kernel,
        {
            "type": "create_agent",
            "parent_id": kernel.id,
            "handler_module": "file_bridge.tools",
            "id": "fb",
            "allow": ["."],
            "root": ".",
            "ingress_rule": "allow_all_ingress",
        },
    )
    assert fb.get("id") == "fb", fb
    leg = await _leg(
        kernel, transport="memory", peer_id="stand_in", ingress_rule="allow_all_ingress"
    )
    # the leg serves the inbound calls AS ITSELF: `fb` accepts the leg
    kernel.update(leg, can_call={"$subtree": "*", "kernel_state": "*", "fb": "*"})
    kernel.update("fb", callable_by={leg: "*"})
    mt_leg, peer = MemoryTransport.pair()
    _engine._test_transport_inject[leg] = mt_leg
    assert (await _op(kernel, {"type": "boot"}, leg)).get("booted") is True

    # Non-UTF-8 bytes — proves it is raw, not text/base64.
    payload = bytes([0x00, 0xFF, 0xCA, 0xFE, 0xBA, 0xBE, 0x10, 0x80])

    def write(body: bytes) -> dict:
        return {
            "type": "call",
            "id": "w1",
            "target": "fb",
            "payload": {
                "type": "write_stream",
                "path": "blob.bin",
                "truncate": True,
                "bytes": body,
            },
        }

    await peer.send(write(payload))
    w = await asyncio.wait_for(peer.recv(), 5.0)
    assert w["type"] == "reply" and w["id"] == "w1"
    assert w["data"]["written"] == len(payload), w

    await peer.send(
        {
            "type": "call",
            "id": "r1",
            "target": "fb",
            "payload": {"type": "read_stream", "path": "blob.bin"},
        }
    )
    r = await asyncio.wait_for(peer.recv(), 5.0)
    assert r["id"] == "r1" and r["data"]["eof"] is True, r
    assert r["data"]["bytes"] == payload, "bytes must round-trip raw over the bridge"

    # A call is an ordinary execution: the frame id NAMES it, so the same id replayed
    # joins the recorded outcome and writes nothing.
    await peer.send(write(b"other"))
    again = await asyncio.wait_for(peer.recv(), 5.0)
    assert again["data"]["written"] == len(payload), again
    assert (tmp_path / "blob.bin").read_bytes() == payload
    ledger = await _op(kernel, {"type": "ledger"}, "kernel")
    assert any(
        e.get("name") == "w1" and e["target"] == "fb" for e in ledger["executions"]
    ), ledger
