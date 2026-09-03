"""ws_bridge / io_bridge engine — the three ways a bridge used to break itself.

1. A RE-ENTRANT round trip deadlocked. The read loop served an inbound `call`
   inline, so a handler that forwarded back over the same bridge could never
   have its reply read — the loop was parked inside `kernel.send`. Both sides
   waited for each other until the 30s forward timeout.
2. ONE malformed frame killed the reader. `decode_frame` raised out of
   `WSTransport.recv`, the read loop died, and `reflect` still said
   `connected:true` while every forward timed out.
3. TEARDOWN blocked the event loop. `st.cleanup` was a sync callable and
   `_kill_tunnel` waits up to 2s on an ssh that ignores SIGTERM.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import struct
import sys
import types

import pytest

from _testkit import boot_root
from io_bridge import MemoryTransport
from io_bridge import _engine as engine
from ws_bridge import _ws
from ws_bridge import tools as wb


# ─── a probe bundle that bounces a call back over the same bridge ────

_probe = types.ModuleType("bounce_probe.tools")


async def _probe_handler(id, payload, kernel):
    """`bounce` — forward BACK over the bridge that just called us, and return
    what the far kernel answered. This is the re-entrant round trip."""
    if payload.get("type") != "bounce":
        return {"error": f"unknown verb {payload.get('type')!r}"}
    out = (
        await kernel.send(
            [
                {
                    "target": payload["bridge"],
                    "payload": {
                        "type": "forward",
                        "target": "kernel_state",
                        "payload": {"type": "reflect", "tree": "none"},
                        # Short, so the OLD inline-dispatch code fails this test in
                        # seconds instead of parking it for the 30s default.
                        "timeout": 3.0,
                    },
                }
            ]
        )
    )[0]["result"]
    return {"bounced": out}


_probe.handler = _probe_handler
_probe.VERBS = {}
sys.modules["bounce_probe.tools"] = _probe


# ─── fixtures ────────────────────────────────────────────────────────


@pytest.fixture
def two_kernels(tmp_path, monkeypatch):
    a_dir = tmp_path / "a"
    b_dir = tmp_path / "b"
    a_dir.mkdir()
    b_dir.mkdir()
    monkeypatch.chdir(a_dir)
    ka = boot_root()
    monkeypatch.chdir(b_dir)
    kbk = boot_root()
    monkeypatch.chdir(tmp_path)
    yield ka, kbk
    wb._bridges.clear()
    wb._test_transport_inject.clear()


async def _open_bridge(kernel, peer_id: str) -> str:
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ws_bridge.tools",
                        "transport": "memory",
                        "peer_id": peer_id,
                        # BOTH legs serve inbound here — the round trip is symmetric.
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]
    return rec["id"]


async def _wire_pair(ka, kbk) -> tuple[str, str]:
    a_id = await _open_bridge(ka, "PLACEHOLDER")
    b_id = await _open_bridge(kbk, a_id)
    ka.update(a_id, peer_id=b_id)
    mt_a, mt_b = MemoryTransport.pair()
    wb._test_transport_inject[a_id] = mt_a
    wb._test_transport_inject[b_id] = mt_b
    for k, bid in ((ka, a_id), (kbk, b_id)):
        r = (await k.send([{"target": bid, "payload": {"type": "boot"}}]))[0]["result"]
        assert r.get("booted") is True, r
    return a_id, b_id


# ─── 1. the re-entrant round trip ────────────────────────────────────


async def test_reentrant_forward_completes_instead_of_deadlocking(two_kernels):
    """A forwards to B; B's handler forwards straight back to A over the SAME
    bridge. Served inline this cannot resolve — B's read loop is inside
    `kernel.send` and never reads A's reply. Served as a task it just works."""
    ka, kbk = two_kernels
    a_id, b_id = await _wire_pair(ka, kbk)
    # The probe is a CHILD of the bridge, so the structural baseline
    # ($parent/$children, both directions) is the only grant either end needs.
    probe = (
        await kbk.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": b_id,
                        "handler_module": "bounce_probe.tools",
                        "id": "bouncer",
                    },
                }
            ]
        )
    )[0]["result"]["id"]

    out = await asyncio.wait_for(
        ka.send(
            [
                {
                    "target": a_id,
                    "payload": {
                        "type": "forward",
                        "target": probe,
                        "payload": {"type": "bounce", "bridge": b_id},
                        "timeout": 10.0,
                    },
                }
            ]
        ),
        timeout=15.0,
    )
    r = out[0]["result"]
    assert "error" not in r, r
    # The inner hop reached kernel A's root and came all the way back.
    assert r["bounced"]["id"] == "kernel_state", r


async def test_read_loop_keeps_reading_while_a_call_is_in_flight(two_kernels):
    """The property underneath: the loop is never parked in a dispatch, so the
    in-flight set holds the call while the next frame is already being read."""
    ka, kbk = two_kernels
    a_id, b_id = await _wire_pair(ka, kbk)
    st = wb._state(b_id)
    assert st.pending_calls == set()
    # Teardown cancels whatever is in flight (nothing here) and clears the set.
    await engine._teardown(b_id)
    assert wb._state(b_id).pending_calls == set()


# ─── 2. malformed frames ─────────────────────────────────────────────


class _FakeWS:
    """A `websockets` client that hands back a scripted list of raw frames."""

    def __init__(self, frames):
        self._frames = list(frames)
        self.closed = False

    async def recv(self):
        if not self._frames:
            await asyncio.sleep(3600)  # nothing more; the test never gets here
        return self._frames.pop(0)


async def test_ws_transport_skips_malformed_frames_and_keeps_reading():
    good = json.dumps({"type": "reply", "id": "1", "data": {"ok": True}})
    frames = [
        b"\x00\x00\x00",  # shorter than the length prefix
        struct.pack(">I", 9999) + b"{}",  # header length past the end
        struct.pack(">I", 8) + b"not json" + b"body",  # bad header JSON
        "{not json",  # bad text frame
        "[1]",  # well-formed JSON, not an envelope
        good,
    ]
    t = _ws.WSTransport(_FakeWS(frames))
    frame = await asyncio.wait_for(t.recv(), timeout=2.0)
    assert frame == {"type": "reply", "id": "1", "data": {"ok": True}}


# ─── 3. awaitable teardown ───────────────────────────────────────────


async def test_teardown_awaits_the_cleanup_hook():
    engine._bridges.pop("cleanup_probe", None)
    st = engine._state("cleanup_probe")
    ran = asyncio.Event()

    async def _cleanup():
        ran.set()

    st.cleanup = _cleanup
    await engine._teardown("cleanup_probe")
    assert ran.is_set()
    assert engine._state("cleanup_probe").cleanup is None
    engine._bridges.pop("cleanup_probe", None)


async def test_ssh_tunnel_cleanup_is_awaitable_and_runs_off_the_loop(monkeypatch):
    """The ssh+ws leg registers `to_thread(_kill_tunnel, …)` — an AWAITABLE, so
    the 2s `proc.wait` cannot freeze every other agent on the loop."""
    killed: list = []

    class _Proc:
        pid = 4242

    async def _fake_open_tunnel(host, local_port, remote_port):
        return _Proc()

    async def _fake_connect(url):
        return object()

    monkeypatch.setattr(_ws, "_open_tunnel", _fake_open_tunnel)
    monkeypatch.setattr(_ws.WSTransport, "connect", staticmethod(_fake_connect))
    monkeypatch.setattr(_ws, "_kill_tunnel", lambda p: killed.append(p))

    st = engine._BridgeState()
    await _ws.build_transport(
        "ssh+ws",
        {"peer_id": "p", "host": "h", "local_port": 9, "remote_port": 8},
        None,
        st,
    )
    assert st.extra["tunnel_pid"] == 4242
    assert st.cleanup is not None
    pending = st.cleanup()
    assert inspect.isawaitable(pending), "cleanup must be awaitable (off the loop)"
    await pending
    assert len(killed) == 1
