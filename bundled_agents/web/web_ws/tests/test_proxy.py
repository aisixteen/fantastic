"""web_ws — WS frame protocol (exercises web._proxy via the web_ws surface)."""

from __future__ import annotations

import json

import pytest
from fastapi.testclient import TestClient

from web.app import make_app
from web_ws.tools import _make_endpoint


@pytest.fixture
def client(seeded_kernel):
    app = make_app("test_web", seeded_kernel)
    # Mount what web_ws.get_routes would publish — lets the proxy
    # tests use client.websocket_connect("/core/ws") without spinning
    # up a real `web` agent + child registration cycle.
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("test_web_ws", seeded_kernel)
    )
    with TestClient(app) as c:
        yield c


def _read_until_reply(ws, call_id, max_frames=10):
    """Drain frames from ws until the reply for `call_id` arrives. Auto-watch
    of the host agent may send `event` frames before the reply lands."""
    for _ in range(max_frames):
        msg = json.loads(ws.receive_text())
        if msg.get("type") == "reply" and msg.get("id") == call_id:
            return msg
    raise AssertionError(f"no reply for id={call_id} within {max_frames} frames")


def test_ws_call_returns_reply(client, seeded_kernel):
    """Send `reflect` to root via WS — the substrate primer comes back
    (transports, agents tree, etc.)."""
    with client.websocket_connect("/core/ws") as ws:
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "core",
                    "payload": {"type": "reflect"},
                    "id": "1",
                }
            )
        )
        msg = _read_until_reply(ws, "1")
        # Root primer keys.
        assert "transports" in msg["data"]
        assert "tree" in msg["data"]
        assert "available_bundles" in msg["data"]


def test_ws_emit_no_reply(client, seeded_kernel):
    with client.websocket_connect("/core/ws") as ws:
        ws.send_text(
            json.dumps(
                {"type": "emit", "target": "core", "payload": {"type": "marker"}}
            )
        )
        # No reply expected; just verify connection still alive via a follow-up call
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "core",
                    "payload": {"type": "reflect"},
                    "id": "x",
                }
            )
        )
        msg = _read_until_reply(ws, "x")
        assert msg["type"] == "reply"


def test_ws_watch_routes_events(client, seeded_kernel):
    with client.websocket_connect("/core/ws") as ws:
        ws.send_text(json.dumps({"type": "watch", "src": "core"}))
        ws.send_text(
            json.dumps(
                {
                    "type": "call",
                    "target": "core",
                    "payload": {"type": "reflect"},
                    "id": "1",
                }
            )
        )
        msg = _read_until_reply(ws, "1")
        assert msg["data"] is not None


def test_ws_call_state_event_carries_host_as_sender(client, seeded_kernel):
    """External WS `call` frames must tag state events with the
    web_ws surface's id so telemetry rays know where to start. Without
    this, browser-driven traffic looks "senderless" and the agent-vis
    never draws sender→recipient wires.
    """
    events: list[dict] = []
    unsub = seeded_kernel.add_state_subscriber(events.append)
    try:
        with client.websocket_connect("/core/ws") as ws:
            ws.send_text(
                json.dumps(
                    {
                        "type": "call",
                        "target": "core",
                        "payload": {"type": "reflect"},
                        "id": "1",
                    }
                )
            )
            _read_until_reply(ws, "1")
    finally:
        unsub()
    sends_to_core = [
        e for e in events if e.get("kind") == "send" and e.get("agent_id") == "core"
    ]
    assert sends_to_core, "no state event observed for the call"
    # Every external WS-driven send to core must originate from the
    # surface's id ("test_web_ws" in this fixture).
    senders = {e.get("sender") for e in sends_to_core}
    assert senders == {"test_web_ws"}, f"expected sender='test_web_ws', got {senders}"
