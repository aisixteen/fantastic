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


def _accept(kernel, agent_id: str, foreign: str) -> None:
    """Open `agent_id`'s `callable_by` to a FOREIGN caller (`<guid>|*` or
    `<guid>|<agent>`): an inbound bridge call is judged by its qualified id,
    never by the leg that relays it."""
    agent = kernel.ctx.agents[agent_id]
    cb = dict(agent._meta.get("callable_by") or {})
    cb[foreign] = "*"
    kernel.update(agent_id, callable_by=cb)


async def _make_bridge(kernel, peer_id: str, transport: str = "memory") -> str:
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ws_bridge.tools",
                        "transport": transport,
                        "peer_id": peer_id,
                    },
                }
            ]
        )
    )[0]["result"]
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
    # Each kernel's instance guid — what a bridge stamps on its outbound frames
    # and what the peer's leg is PINNED to.
    ka.update(ka.id, guid="guid-a")
    kb_kern.update(kb_kern.id, guid="guid-b")
    _accept(ka, ka.id, "guid-b|*")
    _accept(kb_kern, kb_kern.id, "guid-a|*")
    # Allocate ids first so each can know its peer.
    rec_a = (
        await ka.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ws_bridge.tools",
                        "transport": "memory",
                        "peer_id": "PLACEHOLDER",
                        "peer_guid": "guid-b",
                    },
                }
            ]
        )
    )[0]["result"]
    a_id = rec_a["id"]
    rec_b = (
        await kb_kern.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ws_bridge.tools",
                        "transport": "memory",
                        "peer_id": a_id,
                        # B is the SERVING leg: its read_loop dispatches the call A forwards.
                        # Sealed-by-default ⇒ open it explicitly so inbound dispatch is permitted,
                        # and PIN the one peer whose agents it serves.
                        "ingress_rule": "allow_all_ingress",
                        "peer_guid": "guid-a",
                    },
                }
            ]
        )
    )[0]["result"]
    b_id = rec_b["id"]
    # Patch a_id's record with the real peer_id now that we know it.
    ka.update(a_id, peer_id=b_id)

    mt_a, mt_b = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    kb._test_transport_inject[b_id] = mt_b

    r_a = (await ka.send([{"target": a_id, "payload": {"type": "boot"}}]))[0]["result"]
    r_b = (await kb_kern.send([{"target": b_id, "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    assert r_a.get("booted") is True, r_a
    assert r_b.get("booted") is True, r_b
    return a_id, b_id


# ─── tests ───────────────────────────────────────────────────────


async def test_reflect_lists_verbs(two_kernels):
    ka, _ = two_kernels
    bid = await _make_bridge(ka, peer_id="ignored", transport="memory")
    r = (await ka.send([{"target": bid, "payload": {"type": "reflect"}}]))[0]["result"]
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
    r = (
        await ka.send(
            [
                {
                    "target": a_id,
                    "payload": {
                        "type": "forward",
                        "target": "kernel_state",
                        "payload": {"type": "reflect"},
                    },
                }
            ]
        )
    )[0]["result"]
    # The reply is what kernel B's root returns for reflect — the
    # uniform identity + tree (transports moved to the readme).
    assert isinstance(r, dict), f"non-dict reply: {r!r}"
    assert r["id"] == "kernel_state", f"reply not B's root reflect: {r}"
    assert r["tree"]["id"] == "kernel_state"
    assert "transports" not in r


async def test_forward_before_boot_errors(two_kernels):
    ka, _ = two_kernels
    bid = await _make_bridge(ka, peer_id="x", transport="memory")
    r = (
        await ka.send(
            [
                {
                    "target": bid,
                    "payload": {
                        "type": "forward",
                        "target": "kernel_state",
                        "payload": {"type": "reflect"},
                    },
                }
            ]
        )
    )[0]["result"]
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
    # DASH since the frame id became the execution id — ":" would land the
    # correlator in the kernel-minted pid namespace and be refused remotely.
    assert c1 == f"{a_id}-42"
    assert c2 == f"{b_id}-42"


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
    r = (
        await ka.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "delete_agent", "id": a_id},
                }
            ]
        )
    )[0]["result"]
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
    rec_a = (
        await ka.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ws_bridge.tools",
                        "transport": "memory",
                        "peer_id": "test_peer",
                    },
                }
            ]
        )
    )[0]["result"]
    a_id = rec_a["id"]
    mt_a, mt_test = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    r = (await ka.send([{"target": a_id, "payload": {"type": "boot"}}]))[0]["result"]
    assert r.get("booted") is True

    frame = {
        "type": "call",
        "id": "test_corr",
        "caller": "test-peer-g|probe",
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
    test_inbound_call_to_unknown_target_replies_error; `auth` sets both legs' rules."""
    meta = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "handler_module": "ws_bridge.tools",
        "transport": "memory",
        "peer_id": "test_peer",
        "peer_guid": "test-peer-g",
    }
    _accept(kernel, kernel.id, "test-peer-g|*")
    if auth is not None:
        # `auth` used to be a shorthand for both legs. It is gone — say which leg.
        # Rule names are MANIFOLD-SCOPED, so the same policy is a different name on
        # each side; an inbound-only policy presents no outbound credential.
        def _scope(spec, suffix):
            if isinstance(spec, dict):
                stem = spec["type"]
                if suffix == "_egress" and stem in ("deny", "allow_all"):
                    return "silent_egress"
                return {**spec, "type": stem + suffix}
            if suffix == "_egress" and spec in ("deny", "allow_all"):
                return "silent_egress"
            return spec + suffix

        meta["ingress_rule"] = _scope(auth, "_ingress")
        meta["egress_rule"] = _scope(auth, "_egress")
    rec = (await kernel.send([{"target": "kernel_state", "payload": meta}]))[0][
        "result"
    ]
    a_id = rec["id"]
    mt_a, mt_test = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    r = (await kernel.send([{"target": a_id, "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    assert r.get("booted") is True, r
    return a_id, mt_test


async def test_deny_inbound_refuses_inbound_call(two_kernels):
    """auth=deny_inbound replies `unauthorized` to a peer's inbound call WITHOUT
    dispatching it locally — the one-way / hub→spoke (master ignores spoke) case.
    Targets a REAL agent (kernel_state) so allow_all would have succeeded → proves
    the gate, not a missing target."""
    ka, _ = two_kernels
    _a_id, mt_test = await _boot_solo_bridge(ka, auth="deny")
    await mt_test.send(
        {
            "type": "call",
            "id": "c1",
            "caller": "test-peer-g|probe",
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
            "caller": "test-peer-g|probe",
            "target": "kernel_state",
            "payload": {"type": "reflect"},
        }
    )
    reply = await asyncio.wait_for(mt_test.recv(), timeout=2.0)
    assert reply["data"].get("reason") == "unauthorized", reply["data"]


_PASSWORD_AUTH = {"type": "password", "env": "FANTASTIC_GROUP_TOKEN"}


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
            "caller": "test-peer-g|probe",
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
            "caller": "test-peer-g|probe",
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
            [
                {
                    "target": a_id,
                    "payload": {
                        "type": "forward",
                        "target": "remote",
                        "payload": {"type": "reflect"},
                    },
                }
            ]
        )
    )
    frame = await asyncio.wait_for(mt_test.recv(), timeout=2.0)
    assert frame["type"] == "call" and frame["target"] == "remote"
    assert frame.get("auth_token") == "s3cret"  # the leg presents its group token
    # complete the forward so the task doesn't dangle
    await mt_test.send({"type": "reply", "id": frame["id"], "data": {"ok": True}})
    assert (await asyncio.wait_for(fwd, timeout=2.0))[0]["result"] == {"ok": True}


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
            [
                {
                    "target": a_id,
                    "payload": {
                        "type": "forward",
                        "target": "remote",
                        "payload": {"type": "reflect"},
                    },
                }
            ]
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
    rec_a = (
        await ka.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ws_bridge.tools",
                        "transport": "memory",
                        "peer_id": "stand_in",
                    },
                }
            ]
        )
    )[0]["result"]
    a_id = rec_a["id"]
    mt_a, mt_peer = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    (await ka.send([{"target": a_id, "payload": {"type": "boot"}}]))[0]["result"]

    # Fire a forward with a long timeout; if the error frame isn't
    # handled, this would hang ~30s. The test's wait_for(2s) guards.
    fwd = asyncio.create_task(
        ka.send(
            [
                {
                    "target": a_id,
                    "payload": {
                        "type": "forward",
                        "target": "whatever",
                        "payload": {"type": "reflect"},
                        "timeout": 30.0,
                    },
                }
            ]
        )
    )
    # Read A's outbound call frame to learn the corr id, then echo an
    # error frame with the same id (what a raising remote produces).
    call = await asyncio.wait_for(mt_peer.recv(), timeout=2.0)
    assert call["type"] == "call"
    await mt_peer.send({"type": "error", "id": call["id"], "error": "boom"})

    reply = (await asyncio.wait_for(fwd, timeout=2.0))[0]["result"]
    assert "error" in reply and "boom" in reply["error"], reply


async def test_unknown_verb_errors(two_kernels):
    ka, _ = two_kernels
    bid = await _make_bridge(ka, peer_id="x", transport="memory")
    r = (await ka.send([{"target": bid, "payload": {"type": "garbage"}}]))[0]["result"]
    assert "error" in r and "unknown type" in r["error"]


async def test_boot_idempotent(two_kernels):
    ka, kb_kern = two_kernels
    a_id, b_id = await _wire_memory_pair(ka, kb_kern)
    r = (await ka.send([{"target": a_id, "payload": {"type": "boot"}}]))[0]["result"]
    assert r.get("already") is True


async def test_boot_memory_without_injection_errors(two_kernels):
    """Memory transport requires a peered MemoryTransport injected
    via the test seam. Without it, boot must error cleanly rather
    than silently succeed."""
    ka, _ = two_kernels
    bid = await _make_bridge(ka, peer_id="x", transport="memory")
    r = (await ka.send([{"target": bid, "payload": {"type": "boot"}}]))[0]["result"]
    assert "error" in r and "memory transport" in r["error"]


# ─── streaming (watch_remote + event re-emit) ────────────────────


async def test_watch_remote_sends_watch_frame(two_kernels):
    """watch_remote sends `{type:'watch', src:<target>}` over the
    transport. The remote-side (in tests: a raw MemoryTransport peer
    standing in for web_ws) receives the frame as the next-out item."""
    ka, _ = two_kernels
    rec_a = (
        await ka.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ws_bridge.tools",
                        "transport": "memory",
                        "peer_id": "stand_in",
                    },
                }
            ]
        )
    )[0]["result"]
    a_id = rec_a["id"]
    mt_a, mt_peer = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    r = (await ka.send([{"target": a_id, "payload": {"type": "boot"}}]))[0]["result"]
    assert r.get("booted") is True

    r = (
        await ka.send(
            [
                {
                    "target": a_id,
                    "payload": {"type": "watch_remote", "target": "remote_core"},
                }
            ]
        )
    )[0]["result"]
    assert r == {"ok": True, "watching": "remote_core"}

    frame = await asyncio.wait_for(mt_peer.recv(), timeout=2.0)
    assert frame == {"type": "watch", "src": "remote_core"}


async def test_event_frame_re_emits_on_bridge_inbox_only_when_subscribed(two_kernels):
    """A remote `{type:'event', src, payload}` is re-emitted on the bridge agent's
    own inbox (payload + `src`) ONLY for a `src` this leg subscribed to with
    `watch_remote`. Nothing flows before the subscription, nothing without a
    `src`, nothing after `unwatch_remote` (audit 2026-09-28: a peer could push
    arbitrary payloads to the bridge's local watchers)."""
    ka, _ = two_kernels
    rec_a = (
        await ka.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ws_bridge.tools",
                        "transport": "memory",
                        "peer_id": "stand_in",
                    },
                }
            ]
        )
    )[0]["result"]
    a_id = rec_a["id"]
    mt_a, mt_peer = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    r = (await ka.send([{"target": a_id, "payload": {"type": "boot"}}]))[0]["result"]
    assert r.get("booted") is True

    # `_watcher_ids` is the substrate's fanout target set — any id in it gets a
    # copy of every payload emitted (and sent) on the bridge.
    ka.ctx.ensure_inbox("test_watcher")
    ka.ctx.agents[a_id]._watcher_ids.add("test_watcher")
    watcher_inbox = ka.ctx.inboxes["test_watcher"]

    async def _next():
        return await asyncio.wait_for(watcher_inbox.get(), timeout=2.0)

    async def _remote(verb, target):
        await ka.send([{"target": a_id, "payload": {"type": verb, "target": target}}])
        assert (await mt_peer.recv())["src"] == target
        assert (await _next())["type"] == verb  # the watcher mirrors the send too

    def _ev(src, **payload):
        frame = {"type": "event", "payload": {"type": "token", **payload}}
        if src is not None:
            frame["src"] = src
        return frame

    # before any subscription: nothing flows (an unasked src, a src-less event)
    await mt_peer.send(_ev("ghost", text="early"))
    await mt_peer.send(_ev(None, text="no-src"))
    await _remote("watch_remote", "ai")
    await _remote("watch_remote", "fence")
    # the transport is ordered: whatever arrives FIRST after these tells the truth
    await mt_peer.send(_ev(None, text="no-src"))
    await mt_peer.send(_ev("ghost", text="unasked"))
    await mt_peer.send(_ev("ai", text="hi"))
    assert await _next() == {"type": "token", "text": "hi", "src": "ai"}

    # after an unwatch: nothing for that src, the other subscription still flows
    await _remote("unwatch_remote", "ai")
    await mt_peer.send(_ev("ai", text="late"))
    await mt_peer.send(_ev("fence", text="fenced"))
    assert await _next() == {"type": "token", "text": "fenced", "src": "fence"}
    assert watcher_inbox.empty()


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
    rec = (
        await kb_kern.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": "bfiles",
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]
    fb = rec["id"]
    # Deny-all: the receiving bridge relays the inbound call to `fb` AS ITSELF, so
    # its local reach is an explicit grant (ingress_rule says WHO may call in; the
    # bridge's can_call says WHERE into this kernel those calls may land).
    (
        await kb_kern.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "update_agent",
                        "id": b_id,
                        "can_call": {fb: "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    # Two-sided deny-all: `fb` must ACCEPT the FOREIGN caller the bridge relays —
    # judged by its qualified `<guid>|<agent>`, never by the leg.
    fb_agent = kb_kern.ctx.agents.get(fb)
    cb = dict(fb_agent._meta.get("callable_by") or {})
    cb["guid-a|*"] = "*"
    (
        await kb_kern.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "update_agent", "id": fb, "callable_by": cb},
                }
            ]
        )
    )[0]["result"]
    blob = bytes(range(256)) * 50  # 12800 bytes, binary
    w = (
        await kb_kern.send(
            [
                {
                    "target": fb,
                    "payload": {
                        "type": "write_stream",
                        "path": "x.bin",
                        "bytes": blob,
                        "truncate": True,
                    },
                }
            ]
        )
    )[0]["result"]
    assert w["size"] == len(blob)

    # A pulls it over the bridge, threading the cursor across forwards. The chunk is
    # RAW bytes — over MemoryTransport it rides the frame dict natively (over a real
    # WSTransport it rides the shared binary frame); never base64.
    got, offset, frames = b"", 0, 0
    while True:
        r = (
            await ka.send(
                [
                    {
                        "target": a_id,
                        "payload": {
                            "type": "forward",
                            "target": fb,
                            "payload": {
                                "type": "read_stream",
                                "path": "x.bin",
                                "offset": offset,
                                "length": 4096,
                            },
                        },
                    }
                ]
            )
        )[0]["result"]
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
    rec_a = (
        await ka.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "ws_bridge.tools",
                        "transport": "memory",
                        "peer_id": "stand_in",
                    },
                }
            ]
        )
    )[0]["result"]
    a_id = rec_a["id"]
    mt_a, mt_peer = MemoryTransport.pair()
    kb._test_transport_inject[a_id] = mt_a
    (await ka.send([{"target": a_id, "payload": {"type": "boot"}}]))[0]["result"]

    (
        await ka.send(
            [
                {
                    "target": a_id,
                    "payload": {"type": "watch_remote", "target": "remote_core"},
                }
            ]
        )
    )[0]["result"]
    _watch = await asyncio.wait_for(mt_peer.recv(), timeout=2.0)

    r = (
        await ka.send(
            [
                {
                    "target": a_id,
                    "payload": {"type": "unwatch_remote", "target": "remote_core"},
                }
            ]
        )
    )[0]["result"]
    assert r == {"ok": True, "unwatched": "remote_core"}
    frame = await asyncio.wait_for(mt_peer.recv(), timeout=2.0)
    assert frame == {"type": "unwatch", "src": "remote_core"}


# WS integration (real two-kernel ws round-trip + streaming) is
# exercised by the integration tests at the repo root in
# `integration_tests/`. MemoryTransport coverage above proves the
# framing + correlation + lifecycle + event re-emission paths; the
# WS path on top is just `WSTransport` swapping in for
# `MemoryTransport`.
