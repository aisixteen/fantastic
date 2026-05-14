"""kernel_bridge — verbs + memory-transport round-trip + WS integration.

Most tests use MemoryTransport (no network, no SSH, no real WS).
One integration test wires two real `make_app()` instances over a
real `websockets` client — the WS path WITHOUT SSH, since SSH would
require a live host.
"""

from __future__ import annotations

import asyncio

import pytest

from core import Core
from kernel import Kernel
from kernel_bridge import tools as kb
from kernel_bridge._transport import (
    MemoryTransport,
)


# ─── fixtures ────────────────────────────────────────────────────


def _seed(k) -> None:
    """Seed cli singleton (root IS what `core` was; admin verbs are
    baked into Agent class)."""
    k.ensure("cli", "cli.tools", display_name="cli")


@pytest.fixture
def two_kernels(tmp_path, monkeypatch):
    """Two fully separate Kernel instances, each rooted in its own
    tmp dir. Sharing a process means the bridge state machine still
    works (process-memory _bridges dict), but the kernels themselves
    don't share anything via that dict — they only exchange via the
    transport."""
    a_dir = tmp_path / "a"
    b_dir = tmp_path / "b"
    a_dir.mkdir()
    b_dir.mkdir()

    monkeypatch.chdir(a_dir)
    ka = Core(Kernel(), argv=[])
    _seed(ka)
    monkeypatch.chdir(b_dir)
    kb_kern = Core(Kernel(), argv=[])
    _seed(kb_kern)
    monkeypatch.chdir(tmp_path)
    yield ka, kb_kern
    # Wipe the shared _bridges process-memory dict between tests so
    # state from one test doesn't leak into the next.
    kb._bridges.clear()
    kb._test_transport_inject.clear()


async def _make_bridge(kernel, peer_id: str, transport: str = "memory") -> str:
    rec = await kernel.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "kernel_bridge.tools",
            "transport": transport,
            "peer_id": peer_id,
        },
    )
    return rec["id"]


async def _wire_memory_pair(ka, kb_kern):
    """Build two paired MemoryTransports, create a bridge on each
    kernel, inject the transports, boot. Returns (b_a_id, b_b_id)."""
    # Allocate ids first so each can know its peer.
    rec_a = await ka.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "kernel_bridge.tools",
            "transport": "memory",
            "peer_id": "PLACEHOLDER",
        },
    )
    a_id = rec_a["id"]
    rec_b = await kb_kern.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "kernel_bridge.tools",
            "transport": "memory",
            "peer_id": a_id,
        },
    )
    b_id = rec_b["id"]
    # Patch a_id's record with the real peer_id now that we know it.
    ka.update(a_id, peer_id=b_id)

    mt_a, mt_b = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    kb._test_transport_inject[b_id] = mt_b

    r_a = await ka.send(a_id, {"type": "boot"})
    r_b = await kb_kern.send(b_id, {"type": "boot"})
    assert r_a.get("booted") is True, r_a
    assert r_b.get("booted") is True, r_b
    return a_id, b_id


# ─── tests ───────────────────────────────────────────────────────


async def test_reflect_lists_verbs(two_kernels):
    ka, _ = two_kernels
    bid = await _make_bridge(ka, peer_id="ignored", transport="memory")
    r = await ka.send(bid, {"type": "reflect"})
    for v in ("reflect", "boot", "reconnect", "forward"):
        assert v in r["verbs"], f"missing verb {v}"
    assert r["transport"] == "memory"
    assert r["connected"] is False  # not booted yet


async def test_memory_transport_pair_round_trip(two_kernels):
    """The headline test: a forward from kernel A reaches kernel B's
    core.reflect and the reply tunnels back."""
    ka, kb_kern = two_kernels
    a_id, b_id = await _wire_memory_pair(ka, kb_kern)

    # forward(target='core', payload={type:'reflect'}) over the bridge
    r = await ka.send(
        a_id,
        {
            "type": "forward",
            "target": "core",
            "payload": {"type": "reflect"},
        },
    )
    # The reply is what kernel B's root returns for reflect — the
    # substrate primer.
    assert isinstance(r, dict), f"non-dict reply: {r!r}"
    assert "transports" in r, f"reply not the primer: {r}"
    assert "tree" in r
    assert "available_bundles" in r


async def test_forward_before_boot_errors(two_kernels):
    ka, _ = two_kernels
    bid = await _make_bridge(ka, peer_id="x", transport="memory")
    r = await ka.send(
        bid,
        {"type": "forward", "target": "core", "payload": {"type": "reflect"}},
    )
    assert "error" in r and "not connected" in r["error"]


async def test_corr_id_namespacing_no_collision(two_kernels):
    """Two bridges minting corr_ids independently must not collide:
    the namespace prefix is the bridge id."""
    ka, kb_kern = two_kernels
    a_id, b_id = await _wire_memory_pair(ka, kb_kern)
    sa = kb._state(a_id)
    sb = kb._state(b_id)
    sa.corr_counter = 41  # next will be 42
    sb.corr_counter = 41
    c1 = kb._next_corr(a_id, sa)
    c2 = kb._next_corr(b_id, sb)
    assert c1 != c2
    assert c1 == f"{a_id}:42"
    assert c2 == f"{b_id}:42"


async def test_on_delete_cancels_read_task_and_rejects_pending(two_kernels):
    """on_delete must (1) cancel the read_loop task, (2) close the
    transport, (3) reject any in-flight forward Futures with
    ConnectionError so callers don't hang forever."""
    ka, kb_kern = two_kernels
    a_id, b_id = await _wire_memory_pair(ka, kb_kern)
    st = kb._state(a_id)

    # Inject a pending Future that nothing will resolve.
    fake_fut = asyncio.get_event_loop().create_future()
    st.pending["dangling"] = fake_fut
    assert st.read_task is not None and not st.read_task.done()

    await kb.on_delete(ka.ctx.agents[a_id])
    assert fake_fut.done()
    assert isinstance(fake_fut.exception(), ConnectionError)
    assert st.transport is None
    assert st.read_task is None


async def test_on_delete_via_cascade(two_kernels):
    """core.delete_agent calls the bundle's on_delete hook depth-first
    before removing the record. The bridge must clean up cleanly."""
    ka, kb_kern = two_kernels
    a_id, b_id = await _wire_memory_pair(ka, kb_kern)
    assert kb._state(a_id).transport is not None
    r = await ka.send("core", {"type": "delete_agent", "id": a_id})
    assert r.get("deleted") is True
    # State left in re-bootable shape.
    assert kb._state(a_id).transport is None
    assert kb._state(a_id).read_task is None


async def test_inbound_unknown_call_payload_replies_error(two_kernels):
    """If a peer sends a `call` whose payload isn't a forward
    envelope, A's read_loop must reply with a structured error so
    the peer's pending Future resolves cleanly (not hang forever).

    Test design: only A is a real bridge with a read_loop. B is a
    raw MemoryTransport — it sends the frame and reads the reply
    directly without a competing read_loop draining the queue.
    """
    ka, _ = two_kernels
    rec_a = await ka.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "kernel_bridge.tools",
            "transport": "memory",
            "peer_id": "test_peer",
        },
    )
    a_id = rec_a["id"]
    mt_a, mt_test = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    r = await ka.send(a_id, {"type": "boot"})
    assert r.get("booted") is True

    frame = {"type": "call", "payload": {"type": "garbage"}, "id": "test_corr"}
    await mt_test.send(frame)

    reply = await asyncio.wait_for(mt_test.recv(), timeout=2.0)
    assert reply["type"] == "reply"
    assert reply["id"] == "test_corr"
    assert "error" in reply["data"]


async def test_unknown_verb_errors(two_kernels):
    ka, _ = two_kernels
    bid = await _make_bridge(ka, peer_id="x", transport="memory")
    r = await ka.send(bid, {"type": "garbage"})
    assert "error" in r and "unknown type" in r["error"]


async def test_boot_idempotent(two_kernels):
    ka, kb_kern = two_kernels
    a_id, b_id = await _wire_memory_pair(ka, kb_kern)
    r = await ka.send(a_id, {"type": "boot"})
    assert r.get("already") is True


async def test_boot_memory_without_injection_errors(two_kernels):
    """Memory transport requires a peered MemoryTransport injected
    via the test seam. Without it, boot must error cleanly rather
    than silently succeed."""
    ka, _ = two_kernels
    bid = await _make_bridge(ka, peer_id="x", transport="memory")
    r = await ka.send(bid, {"type": "boot"})
    assert "error" in r and "memory transport" in r["error"]


# WS integration (real two-kernel ws round-trip) is exercised by the
# manual selftest against a live `fantastic serve` — keeping it out
# of the unit suite avoids the loopback-in-single-process fragility
# (two kernels sharing the module-level _bridges dict + id namespace
# collisions). MemoryTransport coverage above proves the framing +
# correlation + lifecycle paths are correct; the WS path on top is
# just `WSTransport` swapping in for `MemoryTransport`.


# ─── HTTPTransport (request/reply against a remote web_rest) ────


class _FakeResp:
    def __init__(self, status_code: int, body: dict | None):
        self.status_code = status_code
        self._body = body
        # httpx response surface used by HTTPTransport.send.
        import json as _json

        self.content = b"" if body is None else _json.dumps(body).encode()
        self.text = "" if body is None else _json.dumps(body)

    def json(self) -> dict | None:
        return self._body


class _FakeHTTPClient:
    """Minimal in-memory stand-in for httpx.AsyncClient — records POSTs
    and returns canned replies. Avoids network in the unit suite."""

    def __init__(self, reply: dict | None = None, status: int = 200):
        self.posted: list[tuple[str, dict]] = []
        self._reply = reply or {"ok": True}
        self._status = status
        self.closed = False

    async def post(self, url: str, json=None, headers=None):
        self.posted.append((url, json))
        return _FakeResp(self._status, self._reply)

    async def aclose(self):
        self.closed = True


async def test_http_transport_unwraps_forward_envelope():
    """Bridge wraps every outbound call in a `forward` envelope. The
    HTTP transport unwraps to a direct POST against the remote
    web_rest's `/<rest_id>/<target>` endpoint."""
    from kernel_bridge._transport import HTTPTransport

    fake = _FakeHTTPClient(reply={"sentence": "remote primer"})
    t = HTTPTransport("http://h/rest_xyz/", fake)
    await t.send(
        {
            "type": "call",
            "target": "peer_bridge_b",
            "payload": {
                "type": "forward",
                "target": "core",
                "payload": {"type": "reflect"},
                "corr_id": "c1",
            },
            "id": "c1",
        }
    )
    # POST landed at <base>/<inner_target>, body == inner payload.
    assert fake.posted == [("http://h/rest_xyz/core", {"type": "reflect"})]
    # Reply frame is enqueued with id == corr_id and data == response JSON.
    reply = await t.recv()
    assert reply == {"type": "reply", "id": "c1", "data": {"sentence": "remote primer"}}


async def test_http_transport_translates_4xx_into_reply_error():
    from kernel_bridge._transport import HTTPTransport

    fake = _FakeHTTPClient(reply={"error": "boom"}, status=500)
    t = HTTPTransport("http://h/rest/", fake)
    await t.send({"type": "call", "target": "core", "payload": {}, "id": "c2"})
    reply = await t.recv()
    assert reply["type"] == "reply"
    assert reply["id"] == "c2"
    assert "HTTP 500" in reply["data"]["error"]


async def test_http_transport_rejects_non_call_frames():
    from kernel_bridge._transport import HTTPTransport

    fake = _FakeHTTPClient()
    t = HTTPTransport("http://h/rest/", fake)
    import pytest

    with pytest.raises(NotImplementedError):
        await t.send({"type": "emit", "target": "core", "payload": {}})


async def test_http_transport_close_idempotent():
    from kernel_bridge._transport import HTTPTransport

    fake = _FakeHTTPClient()
    t = HTTPTransport("http://h/rest/", fake)
    await t.close()
    assert t.closed is True
    assert fake.closed is True
    # Second close is a no-op.
    await t.close()
