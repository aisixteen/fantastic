"""ws_bridge — verbs + memory-transport round-trip + streaming.

Tests use MemoryTransport (no network, no SSH, no real WS). The
WS path is covered by integration tests (real Python ↔ Python and
Python ↔ Swift over real websockets) at the repo root in
`integration_tests/`. MemoryTransport coverage here proves the
framing + correlation + lifecycle paths; WSTransport on top is just
a transport swap.
"""

from __future__ import annotations

import asyncio

import pytest

from _testkit import boot_root
from io_bridge import MemoryTransport
from ws_bridge import tools as kb


# ─── fixtures ────────────────────────────────────────────────────


def _seed(k) -> None:
    """Seed cli singleton (root IS what `kernel_state` was; admin verbs are
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
    ka = boot_root()
    _seed(ka)
    monkeypatch.chdir(b_dir)
    kb_kern = boot_root()
    _seed(kb_kern)
    monkeypatch.chdir(tmp_path)
    yield ka, kb_kern
    # Wipe the shared _bridges process-memory dict between tests so
    # state from one test doesn't leak into the next.
    kb._bridges.clear()
    kb._test_transport_inject.clear()


async def _make_bridge(kernel, peer_id: str, transport: str = "memory") -> str:
    rec = await kernel.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "ws_bridge.tools",
            "transport": transport,
            "peer_id": peer_id,
        },
    )
    return rec["id"]


async def _wire_memory_pair(ka, kb_kern):
    """Build two paired MemoryTransports, create a bridge on each
    kernel, inject the transports, boot. Returns (b_a_id, b_b_id).

    The asymmetric WS model in production doesn't need a B-side
    bridge — B's `web_ws` handles inbound call frames directly. For
    memory-transport tests we still pair two bridges because there's
    no `web_ws` in-process; each bridge's `_read_loop` plays the
    "inbound call dispatcher" role that `web_ws._on_call` plays over
    a real WS.
    """
    # Allocate ids first so each can know its peer.
    rec_a = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": "PLACEHOLDER",
        },
    )
    a_id = rec_a["id"]
    rec_b = await kb_kern.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": a_id,
            # B is the SERVING leg: its read_loop dispatches the call A forwards.
            # Sealed-by-default ⇒ open it explicitly so inbound dispatch is permitted.
            "ingress_rule": "allow_all",
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
    for v in (
        "reflect",
        "boot",
        "reconnect",
        "forward",
        "watch_remote",
        "unwatch_remote",
    ):
        assert v in r["verbs"], f"missing verb {v}"
    assert r["transport"] == "memory"
    assert r["connected"] is False  # not booted yet


async def test_memory_transport_pair_round_trip(two_kernels):
    """The headline test: a forward from kernel A reaches kernel B's
    kernel_state.reflect and the reply tunnels back."""
    ka, kb_kern = two_kernels
    a_id, b_id = await _wire_memory_pair(ka, kb_kern)

    # forward(target='kernel_state', payload={type:'reflect'}) over the bridge
    r = await ka.send(
        a_id,
        {
            "type": "forward",
            "target": "kernel_state",
            "payload": {"type": "reflect"},
        },
    )
    # The reply is what kernel B's root returns for reflect — the
    # uniform identity + tree (transports moved to the readme).
    assert isinstance(r, dict), f"non-dict reply: {r!r}"
    assert r["id"] == "kernel_state", f"reply not B's root reflect: {r}"
    assert r["tree"]["id"] == "kernel_state"
    assert "transports" not in r


async def test_forward_before_boot_errors(two_kernels):
    ka, _ = two_kernels
    bid = await _make_bridge(ka, peer_id="x", transport="memory")
    r = await ka.send(
        bid,
        {"type": "forward", "target": "kernel_state", "payload": {"type": "reflect"}},
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
    """kernel_state.delete_agent calls the bundle's on_delete hook depth-first
    before removing the record. The bridge must clean up cleanly."""
    ka, kb_kern = two_kernels
    a_id, b_id = await _wire_memory_pair(ka, kb_kern)
    assert kb._state(a_id).transport is not None
    r = await ka.send("kernel_state", {"type": "delete_agent", "id": a_id})
    assert r.get("deleted") is True
    # State left in re-bootable shape.
    assert kb._state(a_id).transport is None
    assert kb._state(a_id).read_task is None


async def test_inbound_call_to_unknown_target_replies_error(two_kernels):
    """When a peer sends a `call` whose `target` doesn't exist on this
    kernel, A's read_loop must reply with a structured error so the
    peer's pending Future resolves cleanly (not hang forever).

    Test design: only A is a real bridge with a read_loop. The
    "peer" side uses a raw MemoryTransport — it sends the frame and
    reads the reply directly without a competing read_loop draining
    the queue.
    """
    ka, _ = two_kernels
    rec_a = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": "test_peer",
        },
    )
    a_id = rec_a["id"]
    mt_a, mt_test = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    r = await ka.send(a_id, {"type": "boot"})
    assert r.get("booted") is True

    frame = {
        "type": "call",
        "id": "test_corr",
        "target": "no_such_agent_xyz",
        "payload": {"type": "reflect"},
    }
    await mt_test.send(frame)

    reply = await asyncio.wait_for(mt_test.recv(), timeout=2.0)
    assert reply["type"] == "reply"
    assert reply["id"] == "test_corr"
    assert "error" in reply["data"]


async def _boot_solo_bridge(kernel, *, auth=None):
    """One real bridge (with read_loop) + a raw peer transport — same shape as
    test_inbound_call_to_unknown_target_replies_error; `auth` sets the policy."""
    meta = {
        "type": "create_agent",
        "handler_module": "ws_bridge.tools",
        "transport": "memory",
        "peer_id": "test_peer",
    }
    if auth is not None:
        meta["auth"] = auth
    rec = await kernel.send("kernel_state", meta)
    a_id = rec["id"]
    mt_a, mt_test = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    r = await kernel.send(a_id, {"type": "boot"})
    assert r.get("booted") is True, r
    return a_id, mt_test


async def test_deny_inbound_refuses_inbound_call(two_kernels):
    """auth=deny_inbound replies `unauthorized` to a peer's inbound call WITHOUT
    dispatching it locally — the one-way / hub→spoke (master ignores spoke) case.
    Targets a REAL agent (kernel_state) so allow_all would have succeeded → proves
    the gate, not a missing target."""
    ka, _ = two_kernels
    _a_id, mt_test = await _boot_solo_bridge(ka, auth="deny_inbound")
    await mt_test.send(
        {
            "type": "call",
            "id": "c1",
            "target": "kernel_state",
            "payload": {"type": "reflect"},
        }
    )
    reply = await asyncio.wait_for(mt_test.recv(), timeout=2.0)
    assert reply["type"] == "reply" and reply["id"] == "c1"
    assert reply["data"].get("reason") == "unauthorized", reply["data"]


async def test_deny_inbound_default_refuses_inbound_call(two_kernels):
    """No `auth` ⇒ deny_inbound (sealed by default): the peer's inbound call on a
    NO-RULE leg is REFUSED `unauthorized` without dispatching locally — the seal is
    the default posture; opening an edge is the control plane's explicit job."""
    ka, _ = two_kernels
    _a_id, mt_test = await _boot_solo_bridge(ka)  # no auth field ⇒ sealed
    await mt_test.send(
        {
            "type": "call",
            "id": "c1",
            "target": "kernel_state",
            "payload": {"type": "reflect"},
        }
    )
    reply = await asyncio.wait_for(mt_test.recv(), timeout=2.0)
    assert reply["data"].get("reason") == "unauthorized", reply["data"]


_PASSWORD_AUTH = {"policy": "password", "token_env": "FANTASTIC_GROUP_TOKEN"}


async def test_password_dispatches_inbound_call_with_valid_token(
    two_kernels, monkeypatch
):
    """auth=password: an inbound call carrying the matching group `auth_token`
    dispatches locally (the kernel-group member case)."""
    monkeypatch.setenv("FANTASTIC_GROUP_TOKEN", "s3cret")
    ka, _ = two_kernels
    _a_id, mt_test = await _boot_solo_bridge(ka, auth=_PASSWORD_AUTH)
    await mt_test.send(
        {
            "type": "call",
            "id": "c1",
            "target": "kernel_state",
            "payload": {"type": "reflect"},
            "auth_token": "s3cret",  # rides the frame envelope, not the payload
        }
    )
    reply = await asyncio.wait_for(mt_test.recv(), timeout=2.0)
    assert reply["data"].get("reason") != "unauthorized"
    assert reply["data"].get("id") == "kernel_state"


async def test_password_refuses_inbound_call_with_wrong_token(two_kernels, monkeypatch):
    """auth=password: a wrong/missing group token is refused `unauthorized` without
    dispatching (a non-member peer can't call us)."""
    monkeypatch.setenv("FANTASTIC_GROUP_TOKEN", "s3cret")
    ka, _ = two_kernels
    _a_id, mt_test = await _boot_solo_bridge(ka, auth=_PASSWORD_AUTH)
    await mt_test.send(
        {
            "type": "call",
            "id": "c1",
            "target": "kernel_state",
            "payload": {"type": "reflect"},
            "auth_token": "WRONG",
        }
    )
    reply = await asyncio.wait_for(mt_test.recv(), timeout=2.0)
    assert reply["data"].get("reason") == "unauthorized", reply["data"]


async def test_password_attaches_group_token_on_forward(two_kernels, monkeypatch):
    """A password leg PRESENTS its group token: every outbound `call` frame carries
    `auth_token` (the symmetric side — so a paired group member's policy accepts it).
    A non-password leg attaches nothing (wire unchanged)."""
    monkeypatch.setenv("FANTASTIC_GROUP_TOKEN", "s3cret")
    ka, _ = two_kernels
    a_id, mt_test = await _boot_solo_bridge(ka, auth=_PASSWORD_AUTH)
    fwd = asyncio.create_task(
        ka.send(
            a_id,
            {"type": "forward", "target": "remote", "payload": {"type": "reflect"}},
        )
    )
    frame = await asyncio.wait_for(mt_test.recv(), timeout=2.0)
    assert frame["type"] == "call" and frame["target"] == "remote"
    assert frame.get("auth_token") == "s3cret"  # the leg presents its group token
    # complete the forward so the task doesn't dangle
    await mt_test.send({"type": "reply", "id": frame["id"], "data": {"ok": True}})
    assert (await asyncio.wait_for(fwd, timeout=2.0)) == {"ok": True}


async def test_non_password_leg_attaches_no_token_on_forward(two_kernels):
    """An un-stamped leg's outbound frame has NO `auth_token` field — the EGRESS
    default is `silent`, so credential() is None and the wire shape is byte-identical
    to pre-auth. (Egress default `silent` is independent of the ingress SEAL — this
    leg's absent ingress rule is `deny_inbound`, but that gates only INbound calls.)"""
    ka, _ = two_kernels
    # no auth ⇒ egress silent / ingress sealed (deny_inbound)
    a_id, mt_test = await _boot_solo_bridge(ka)
    fwd = asyncio.create_task(
        ka.send(
            a_id,
            {"type": "forward", "target": "remote", "payload": {"type": "reflect"}},
        )
    )
    frame = await asyncio.wait_for(mt_test.recv(), timeout=2.0)
    assert "auth_token" not in frame
    await mt_test.send({"type": "reply", "id": frame["id"], "data": {"ok": True}})
    await asyncio.wait_for(fwd, timeout=2.0)


async def test_inbound_error_frame_fails_forward_promptly(two_kernels):
    """A remote `web_ws` emits `{type:"error", id, error}` when its
    dispatch RAISES (vs a verb-level error dict, which rides back as a
    `reply`). The bridge's read loop must fail the pending forward
    PROMPTLY with that error — not let it hang to the timeout. Regression
    for the cross-runtime error-frame gap (rust handled it; py/swift
    didn't)."""
    ka, _ = two_kernels
    rec_a = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": "stand_in",
        },
    )
    a_id = rec_a["id"]
    mt_a, mt_peer = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    await ka.send(a_id, {"type": "boot"})

    # Fire a forward with a long timeout; if the error frame isn't
    # handled, this would hang ~30s. The test's wait_for(2s) guards.
    fwd = asyncio.create_task(
        ka.send(
            a_id,
            {
                "type": "forward",
                "target": "whatever",
                "payload": {"type": "reflect"},
                "timeout": 30.0,
            },
        )
    )
    # Read A's outbound call frame to learn the corr id, then echo an
    # error frame with the same id (what a raising remote produces).
    call = await asyncio.wait_for(mt_peer.recv(), timeout=2.0)
    assert call["type"] == "call"
    await mt_peer.send({"type": "error", "id": call["id"], "error": "boom"})

    reply = await asyncio.wait_for(fwd, timeout=2.0)
    assert "error" in reply and "boom" in reply["error"], reply


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


# ─── streaming (watch_remote + event re-emit) ────────────────────


async def test_watch_remote_sends_watch_frame(two_kernels):
    """watch_remote sends `{type:'watch', src:<target>}` over the
    transport. The remote-side (in tests: a raw MemoryTransport peer
    standing in for web_ws) receives the frame as the next-out item."""
    ka, _ = two_kernels
    rec_a = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": "stand_in",
        },
    )
    a_id = rec_a["id"]
    mt_a, mt_peer = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    r = await ka.send(a_id, {"type": "boot"})
    assert r.get("booted") is True

    r = await ka.send(a_id, {"type": "watch_remote", "target": "remote_core"})
    assert r == {"ok": True, "watching": "remote_core"}

    frame = await asyncio.wait_for(mt_peer.recv(), timeout=2.0)
    assert frame == {"type": "watch", "src": "remote_core"}


async def test_event_frame_re_emits_on_bridge_inbox(two_kernels):
    """When the remote sends `{type:'event', payload}`, the bridge's
    read loop re-emits `payload` on the bridge agent's own inbox so
    local watchers see remote streams via `kernel.watch(<bridge>, ...)`."""
    ka, _ = two_kernels
    rec_a = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": "stand_in",
        },
    )
    a_id = rec_a["id"]
    mt_a, mt_peer = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    r = await ka.send(a_id, {"type": "boot"})
    assert r.get("booted") is True

    # Attach a synthetic watcher to the bridge BEFORE the event
    # arrives. `_watcher_ids` is the substrate's fanout target set —
    # any id in it gets a copy of every payload emitted on the bridge.
    ka.ctx.ensure_inbox("test_watcher")
    ka.ctx.agents[a_id]._watcher_ids.add("test_watcher")

    await mt_peer.send({"type": "event", "payload": {"type": "token", "text": "hi"}})

    watcher_inbox = ka.ctx.inboxes["test_watcher"]
    ev = await asyncio.wait_for(watcher_inbox.get(), timeout=2.0)
    assert ev.get("type") == "token" and ev.get("text") == "hi", (
        f"expected re-emitted token event, got: {ev}"
    )


async def test_forward_read_stream_cursor_round_trips(two_kernels):
    """The stream CURSOR survives a forward over the bridge transport. Kernel A
    forwards `read_stream` to an OPEN file_bridge on kernel B, threading
    `next_offset` across forwards, and reassembles a multi-chunk binary file
    byte-for-byte. The cursor is a payload field tunneled through the bridge's JSON
    frames — this proves MemoryTransport framing carries the stateless pointer; the
    WSTransport path is the same swap."""
    ka, kb_kern = two_kernels
    a_id, b_id = await _wire_memory_pair(ka, kb_kern)

    # B hosts the SOURCE: an OPEN file_bridge holding a multi-chunk blob.
    rec = await kb_kern.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "root": "bfiles",
            "ingress_rule": "allow_all",
        },
    )
    fb = rec["id"]
    blob = bytes(range(256)) * 50  # 12800 bytes, binary
    w = await kb_kern.send(
        fb,
        {
            "type": "write_stream",
            "path": "x.bin",
            "bytes": blob,
            "truncate": True,
        },
    )
    assert w["size"] == len(blob)

    # A pulls it over the bridge, threading the cursor across forwards. The chunk is
    # RAW bytes — over MemoryTransport it rides the frame dict natively (over a real
    # WSTransport it rides the shared binary frame); never base64.
    got, offset, frames = b"", 0, 0
    while True:
        r = await ka.send(
            a_id,
            {
                "type": "forward",
                "target": fb,
                "payload": {
                    "type": "read_stream",
                    "path": "x.bin",
                    "offset": offset,
                    "length": 4096,
                },
            },
        )
        assert isinstance(r, dict) and isinstance(r.get("bytes"), (bytes, bytearray)), r
        got += r["bytes"]
        offset = r["next_offset"]
        frames += 1
        if r["eof"]:
            break

    assert got == blob
    assert frames > 1, "multi-chunk: the cursor must have advanced across forwards"
    assert r["size"] == len(blob)


async def test_unwatch_remote_sends_unwatch_frame(two_kernels):
    """unwatch_remote sends `{type:'unwatch', src:<target>}` so the
    remote stops pushing events for that subscription."""
    ka, _ = two_kernels
    rec_a = await ka.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "ws_bridge.tools",
            "transport": "memory",
            "peer_id": "stand_in",
        },
    )
    a_id = rec_a["id"]
    mt_a, mt_peer = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    await ka.send(a_id, {"type": "boot"})

    await ka.send(a_id, {"type": "watch_remote", "target": "remote_core"})
    _watch = await asyncio.wait_for(mt_peer.recv(), timeout=2.0)

    r = await ka.send(a_id, {"type": "unwatch_remote", "target": "remote_core"})
    assert r == {"ok": True, "unwatched": "remote_core"}
    frame = await asyncio.wait_for(mt_peer.recv(), timeout=2.0)
    assert frame == {"type": "unwatch", "src": "remote_core"}


# WS integration (real two-kernel ws round-trip + streaming) is
# exercised by the integration tests at the repo root in
# `integration_tests/`. MemoryTransport coverage above proves the
# framing + correlation + lifecycle + event re-emission paths; the
# WS path on top is just `WSTransport` swapping in for
# `MemoryTransport`.
