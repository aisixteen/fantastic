"""web_ws — call-surface contract."""

from __future__ import annotations

import json

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from web_ws.tools import _get_routes, _make_endpoint, _reflect, route_endpoints

from _testkit import wire
# twin: reflect_keys_match_python


async def test_reflect_describes_surface(seeded_kernel):
    r = await _reflect("ws_xyz", {}, seeded_kernel)
    assert r["id"] == "ws_xyz"
    assert r["path_pattern"] == "/{host_id}/ws"
    assert "verbs" not in r


async def test_get_routes_match_python(seeded_kernel):
    """DECLARATIVE — pure JSON data, byte-shaped like rust. No callable
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


# twin: a_leg_without_route_keeps_the_catch_all
@pytest.mark.parametrize("i,meta", [(0, {}), (1, {"route": None})])
async def test_a_leg_without_route_keeps_the_catch_all(seeded_kernel, i, meta):
    lid = f"ws_noroute_{i}"
    seeded_kernel.create("web_ws.tools", id=lid, **meta)
    assert await _get_routes(lid, {}, seeded_kernel) == {
        "routes": [{"kind": "websocket", "path": "/{host_id}/ws"}]
    }
    assert (await _reflect(lid, {}, seeded_kernel))["path_pattern"] == "/{host_id}/ws"
    assert set(route_endpoints(seeded_kernel.ctx.agents[lid])) == {"/{host_id}/ws"}


# twin: a_route_mounts_only_its_literal
async def test_a_route_mounts_only_its_literal(seeded_kernel):
    seeded_kernel.create("web_ws.tools", id="ws_lit", route="/page_ws/ws")
    assert await _get_routes("ws_lit", {}, seeded_kernel) == {
        "routes": [{"kind": "websocket", "path": "/page_ws/ws"}]
    }
    assert (await _reflect("ws_lit", {}, seeded_kernel))["path_pattern"] == "/page_ws/ws"
    assert set(route_endpoints(seeded_kernel.ctx.agents["ws_lit"])) == {"/page_ws/ws"}


# twin: validate_meta_refuses_a_route_that_is_not_a_literal_path
async def test_validate_meta_refuses_a_route_that_is_not_a_literal_path(seeded_kernel):
    from web_ws.tools import validate_meta

    for bad in ("/{x}/ws", "page/ws", "", 7):
        out = validate_meta({"route": bad}, {})
        assert out.startswith("route=") and "is not a literal mount path" in out, out
    assert validate_meta({"route": 7}, {}) == (
        'route=7 is not a literal mount path — a string starting with "/", with no "{" or "}"'
    )
    for good in ("/page_ws/ws", None):
        assert validate_meta({"route": good, "ingress_rule": "allow_all_ingress"}, {}) is None
    out = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "handler_module": "web_ws.tools",
                        "parent_id": seeded_kernel.id,
                        "route": "page/ws",
                    },
                }
            ]
        )
    )[0]["result"]
    assert "is not a literal mount path" in json.dumps(out, ensure_ascii=False), out


# Python-only (the Rust side is covered in tests_route_order.rs)
async def test_a_literal_route_names_its_host_from_the_first_segment(
    seeded_kernel, monkeypatch
):
    seen = []

    async def fake(ws, kernel, host, leg):
        seen.append((host, leg))
        await ws.close()

    monkeypatch.setattr("web_ws.tools._proxy_run", fake)
    app = FastAPI()
    app.add_api_websocket_route("/page_ws/ws", _make_endpoint("page_ws", seeded_kernel))
    app.add_api_websocket_route("/{host_id}/ws", _make_endpoint("cat_ws", seeded_kernel))
    with TestClient(app) as c:
        for path in ("/page_ws/ws", "/user/ws"):
            with c.websocket_connect(path) as ws:
                try:
                    ws.receive_text()
                except Exception:
                    pass
    assert seen == [("page_ws", "page_ws"), ("user", "cat_ws")]
