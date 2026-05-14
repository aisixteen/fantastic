"""web_ws — call-surface contract."""

from __future__ import annotations

import json

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from web_ws.tools import _get_routes, _make_endpoint, _reflect


async def test_reflect_describes_surface(seeded_kernel):
    r = await _reflect("ws_xyz", {}, seeded_kernel)
    assert r["id"] == "ws_xyz"
    assert r["path_pattern"] == "/{host_id}/ws"
    assert "get_routes" in r["verbs"]


async def test_get_routes_returns_one_websocket(seeded_kernel):
    r = await _get_routes("ws_xyz", {}, seeded_kernel)
    routes = r["routes"]
    assert len(routes) == 1
    spec = routes[0]
    assert spec["kind"] == "websocket"
    assert spec["path"] == "/{host_id}/ws"
    assert callable(spec["endpoint"])


def test_endpoint_mountable_on_fastapi(seeded_kernel):
    """Round-trip: drop the endpoint onto a fresh FastAPI app and prove
    a `call` frame round-trips through the proxy."""
    app = FastAPI()
    app.add_api_websocket_route(
        "/{host_id}/ws", _make_endpoint("test_ws", seeded_kernel)
    )
    with TestClient(app) as c:
        with c.websocket_connect("/core/ws") as ws:
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
            for _ in range(8):
                msg = json.loads(ws.receive_text())
                if msg.get("type") == "reply" and msg.get("id") == "1":
                    assert "transports" in (msg.get("data") or {})
                    return
            pytest.fail("no reply frame received")
