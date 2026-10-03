"""web_ws — call-surface contract."""

from __future__ import annotations

import json

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from web_ws.tools import _get_routes, _make_endpoint, _reflect, route_endpoints

from _testkit import wire


async def test_reflect_describes_surface(seeded_kernel):
    r = await _reflect("ws_xyz", {}, seeded_kernel)
    assert r["id"] == "ws_xyz"
    assert r["path_pattern"] == "/{host_id}/ws"
    assert "get_routes" in r["verbs"]


async def test_get_routes_returns_one_websocket(seeded_kernel):
    """DECLARATIVE — pure JSON data, byte-shaped like rust/swift. No callable
    rides the reply, so it renders over REST and persists into the ledger."""
    r = await _get_routes("ws_xyz", {}, seeded_kernel)
    assert r == {"routes": [{"kind": "websocket", "path": "/{host_id}/ws"}]}
    assert json.loads(json.dumps(r)) == r


def test_route_endpoints_hands_over_the_live_endpoint(seeded_kernel):
    """The other half: `web` gets the callable in-process, keyed by the path
    `get_routes` declared."""
    seeded_kernel.create("web_ws.tools", id="ws_hook")
    eps = route_endpoints(seeded_kernel.ctx.agents["ws_hook"])
    assert set(eps) == {"/{host_id}/ws"}
    assert callable(eps["/{host_id}/ws"])


def test_endpoint_mountable_on_fastapi(seeded_kernel):
    """Round-trip: drop the endpoint onto a fresh FastAPI app and prove
    a `call` frame round-trips through the proxy."""
    # Sealed by default ⇒ open the leg so the inbound `call` dispatches.
    seeded_kernel.create("web_ws.tools", id="test_ws", ingress_rule="allow_all_ingress")
    wire(seeded_kernel, "test_ws", "kernel_state")
    app = FastAPI()
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("test_ws", seeded_kernel)
    )
    with TestClient(app) as c:
        with c.websocket_connect("/kernel_state/ws") as ws:
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
            for _ in range(8):
                msg = json.loads(ws.receive_text())
                if msg.get("type") == "reply" and msg.get("id") == "1":
                    data = msg.get("data") or {}
                    assert data["id"] == "kernel_state" and "tree" in data
                    return
            pytest.fail("no reply frame received")
