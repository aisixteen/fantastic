"""Twins of the Rust fantastic-bridge tests `ws_boot_fails_cleanly_on_unreachable_host`
and `reconnect_emits_bridge_down_then_bridge_up`."""

from __future__ import annotations

import asyncio
import socket

from io_bridge import MemoryTransport
from io_bridge import _engine


async def _call(k, leg: str, payload: dict) -> dict:
    return (await k.send([{"target": leg, "payload": payload}]))[0]["result"]


def _dead_port() -> int:
    """A loopback port nothing listens on (refuses at once, deterministic)."""
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


async def test_ws_boot_fails_cleanly_on_unreachable_host(seeded_kernel):
    k = seeded_kernel
    # Rust dials TEST-NET-1 192.0.2.1:9; a refused loopback port is the
    # deterministic equivalent here.
    leg = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "handler_module": "ws_bridge.tools",
                        "transport": "ws",
                        "host": "127.0.0.1",
                        "port": _dead_port(),
                        "peer_id": "peer_x",
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    try:
        r = await asyncio.wait_for(_call(k, leg, {"type": "boot"}), 15)
        err = r.get("error")
        assert isinstance(err, str) and err, f"expected clean error, got: {r}"
        assert "ws" in err, r
        assert "booted" not in r, r
        ref = await _call(k, leg, {"type": "reflect"})
        assert ref["connected"] is False, f"reflect: {ref}"
        # no task is left behind
        assert _engine._bridges[leg].read_task is None
        assert not [
            t
            for t in k.ctx.tasks.values()
            if t.get("agent") == leg and t.get("verb") == "read_loop"
        ]
    finally:
        await _engine._teardown(leg)
        _engine._bridges.pop(leg, None)


async def test_reconnect_calls_shutdown_then_boot(seeded_kernel):
    k = seeded_kernel
    bid = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "handler_module": "ws_bridge.tools",
                        "can_call": {"$subtree": "*", "kernel_state": "*"},
                        "transport": "memory",
                        "peer_id": "peer",
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    try:
        mt_a, _peer1 = MemoryTransport.pair()
        _engine._test_transport_inject[bid] = mt_a
        inbox = k.ctx.ensure_inbox(bid)
        await _call(k, bid, {"type": "boot"})
        mt_b, _peer2 = MemoryTransport.pair()
        _engine._test_transport_inject[bid] = mt_b
        await _call(k, bid, {"type": "reconnect"})
        await asyncio.sleep(0.2)
        kinds = []
        while True:
            try:
                kinds.append(inbox.get_nowait().get("type"))
            except asyncio.QueueEmpty:
                break
        assert kinds == [
            "boot",
            "bridge_up",
            "reconnect",
            "bridge_down",
            "bridge_up",
        ], f"each request precedes its own events; exactly one bridge_down per teardown: {kinds}"
        assert (await _call(k, bid, {"type": "reflect"}))["connected"] is True
    finally:
        await _engine._teardown(bid)
        _engine._bridges.pop(bid, None)
        _engine._test_transport_inject.clear()
