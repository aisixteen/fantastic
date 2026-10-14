"""WS proxy bridge for the kernel state stream.

`state_subscribe` and `state_unsubscribe` inbound frames mirror
`watch`/`unwatch` lifecycle. On subscribe, the proxy first sends a
`state_snapshot` frame, then registers a kernel callback that pumps
`state_event` frames per traffic / lifecycle event.
"""

from __future__ import annotations

import json

import pytest
from fastapi.testclient import TestClient

from web.app import make_app
from web_ws.tools import _make_endpoint

from _testkit import wire


@pytest.fixture
def client(seeded_kernel):
    # The web host's record: its port is the authority the Host guard admits
    # — the client names it on every request, the upgrade included.
    seeded_kernel.create(None, id="test_web", port=8088)
    app = make_app("test_web", seeded_kernel)
    # Sealed by default ⇒ open the leg so state_subscribe/call frames dispatch
    # (a sealed leg would deny them, and the snapshot/event frames would never flow).
    seeded_kernel.create(
        "web_ws.tools",
        id="test_web_ws",
        ingress_rule="allow_all_ingress",
    )
    # Deny-all: ingress opens WHO; the leg's edge to the root opens WHAT — the
    # browser's create/delete and the kernel-wide state stream, on both ends.
    wire(seeded_kernel, "test_web_ws", "kernel_state")
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("test_web_ws", seeded_kernel)
    )
    with TestClient(app, headers={"host": "127.0.0.1:8088"}) as c:
        yield c


def _drain_until(ws, predicate, max_frames=30):
    """Pull frames until `predicate(msg)` returns truthy. Returns the
    matching frame, or raises if not seen within max_frames."""
    for _ in range(max_frames):
        msg = json.loads(ws.receive_text())
        if predicate(msg):
            return msg
    raise AssertionError(f"no frame matched predicate within {max_frames} frames")


def test_state_subscribe_frame_sends_snapshot(client, seeded_kernel):
    with client.websocket_connect("/kernel_state/ws") as ws:
        ws.send_text(json.dumps({"type": "state_subscribe"}))
        snap = _drain_until(ws, lambda m: m.get("type") == "state_snapshot")
        assert "agents" in snap
        ids = {a["agent_id"] for a in snap["agents"]}
        assert "kernel_state" in ids and "cli" in ids
        for a in snap["agents"]:
            assert "name" in a and "backlog" in a


def test_state_event_frames_after_subscribe(client, seeded_kernel):
    with client.websocket_connect("/kernel_state/ws") as ws:
        ws.send_text(json.dumps({"type": "state_subscribe"}))
        _drain_until(ws, lambda m: m.get("type") == "state_snapshot")
        # Trigger traffic via a regular call frame.
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "payload": {"type": "reflect"},
                    "id": "trigger1",
                }
            )
        )
        evt = _drain_until(
            ws,
            lambda m: (
                m.get("type") == "state_event"
                and m.get("kind") == "send"
                and m.get("agent_id") == "kernel_state"
            ),
        )
        assert "backlog" in evt
        assert "name" in evt


def test_state_event_lifecycle_added(client, seeded_kernel):
    with client.websocket_connect("/kernel_state/ws") as ws:
        ws.send_text(json.dumps({"type": "state_subscribe"}))
        _drain_until(ws, lambda m: m.get("type") == "state_snapshot")
        # Spawn a new agent via kernel_state.create_agent.
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "test_web_ws",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                    "id": "create1",
                }
            )
        )
        added = _drain_until(
            ws,
            lambda m: m.get("type") == "state_event" and m.get("kind") == "added",
        )
        assert "agent_id" in added and "name" in added
        assert added["agent_id"].startswith("file_")


def test_state_event_lifecycle_removed(client, seeded_kernel):
    with client.websocket_connect("/kernel_state/ws") as ws:
        ws.send_text(json.dumps({"type": "state_subscribe"}))
        _drain_until(ws, lambda m: m.get("type") == "state_snapshot")
        # Create then delete in sequence; expect 'added' then 'removed'.
        # Created under the LEG itself: `delete_agent` is gated on its operand,
        # and an agent the leg made under itself is its own to delete.
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "test_web_ws",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                    "id": "c",
                }
            )
        )
        added = _drain_until(
            ws,
            lambda m: m.get("type") == "state_event" and m.get("kind") == "added",
        )
        new_id = added["agent_id"]
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "payload": {"type": "delete_agent", "id": new_id},
                    "id": "d",
                }
            )
        )
        removed = _drain_until(
            ws,
            lambda m: (
                m.get("type") == "state_event"
                and m.get("kind") == "removed"
                and m.get("agent_id") == new_id
            ),
        )
        assert removed["name"]


def test_state_unsubscribe_stops_frames(client, seeded_kernel):
    with client.websocket_connect("/kernel_state/ws") as ws:
        ws.send_text(json.dumps({"type": "state_subscribe"}))
        _drain_until(ws, lambda m: m.get("type") == "state_snapshot")
        ws.send_text(json.dumps({"type": "state_unsubscribe"}))
        # After unsubscribe, kernel-side subscriber count should drop.
        # Easier check: trigger traffic + make sure no state_event lands
        # within a small frame budget. (We allow other event types.)
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "payload": {"type": "reflect"},
                    "id": "1",
                }
            )
        )
        # Drain a few frames; assert no state_event among them.
        seen_state = False
        for _ in range(8):
            try:
                msg = json.loads(ws.receive_text())
            except Exception:
                break
            if msg.get("type") == "state_event":
                seen_state = True
                break
            if msg.get("type") == "reply" and msg.get("id") == "1":
                break
        assert not seen_state


def test_ws_close_unsubscribes_state_callback(seeded_kernel):
    """When the WS closes, the proxy's finally block unregisters every
    state subscriber tied to that connection."""
    # The web host's record: its port is the authority the Host guard admits
    # — the client names it on every request, the upgrade included.
    seeded_kernel.create(None, id="test_web", port=8088)
    app = make_app("test_web", seeded_kernel)
    # Sealed by default ⇒ open the leg so state_subscribe dispatches.
    seeded_kernel.create(
        "web_ws.tools",
        id="test_web_ws",
        ingress_rule="allow_all_ingress",
    )
    # Deny-all: ingress opens WHO; the leg's edge to the root opens WHAT — the
    # browser's create/delete and the kernel-wide state stream, on both ends.
    wire(seeded_kernel, "test_web_ws", "kernel_state")
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("test_web_ws", seeded_kernel)
    )
    pre = len(seeded_kernel.ctx.state_subscribers)
    with TestClient(app, headers={"host": "127.0.0.1:8088"}) as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
            ws.send_text(json.dumps({"type": "state_subscribe"}))
            _drain_until(ws, lambda m: m.get("type") == "state_snapshot")
            assert len(seeded_kernel.ctx.state_subscribers) == pre + 1
        # WS context-manager exit closes the socket.
    assert len(seeded_kernel.ctx.state_subscribers) == pre


def test_state_event_carries_display_name_when_present(client, seeded_kernel):
    """Lifecycle 'added' carries the agent's display_name; falls back to id."""
    with client.websocket_connect("/kernel_state/ws") as ws:
        ws.send_text(json.dumps({"type": "state_subscribe"}))
        _drain_until(ws, lambda m: m.get("type") == "state_snapshot")
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "test_web_ws",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "display_name": "my-store",
                    },
                    "id": "c",
                }
            )
        )
        added = _drain_until(
            ws,
            lambda m: m.get("type") == "state_event" and m.get("kind") == "added",
        )
        assert added["name"] == "my-store"


def test_state_subscribe_without_the_grant_is_refused(seeded_kernel):
    """The leg is gated as ITSELF, never as the operator: an open ingress rule
    and grants for other verbs do not buy the kernel-wide stream. The refusal
    is an error frame, no snapshot follows, and no subscriber is registered.
    (The handler ran with no sender, so the kernel read an operator and served
    the snapshot; only the ambient context of a production serve gated it.)"""
    # The web host's record: its port is the authority the Host guard admits
    # — the client names it on every request, the upgrade included.
    seeded_kernel.create(None, id="test_web", port=8088)
    app = make_app("test_web", seeded_kernel)
    seeded_kernel.create(
        "web_ws.tools",
        id="test_web_ws",
        ingress_rule="allow_all_ingress",
    )
    # every verb on the root but the stream — the connect-time auto-watch is
    # granted, so its refusal is not the error frame read below
    wire(seeded_kernel, "test_web_ws", "kernel_state", ["create_agent", "watch"])
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("test_web_ws", seeded_kernel)
    )
    pre = len(seeded_kernel.ctx.state_subscribers)
    with TestClient(app, headers={"host": "127.0.0.1:8088"}) as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
            ws.send_text(json.dumps({"type": "state_subscribe", "id": "s1"}))
            msg = _drain_until(
                ws, lambda m: m.get("type") in ("error", "state_snapshot")
            )
            assert msg["type"] == "error", msg
            assert msg["id"] == "s1"
            assert msg["reason"] == "unauthorized"
            assert "state_subscribe" in msg["error"]
            assert len(seeded_kernel.ctx.state_subscribers) == pre
