"""webapp — mounting a child's call surface, the two halves.

A leg DECLARES its routes as data (`get_routes`) and hands over the live
endpoints in-process (`route_endpoints(agent)`). `web` needs both; a leg that
declares a route and exposes no hook is broken, and says so at boot.
"""

from __future__ import annotations

import sys
import types

import pytest

from web.tools import _mount_all_surfaces, _servers, _spawn


async def _mk(kernel, handler_module: str, aid: str, **meta) -> str:
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "id": aid,
                        "handler_module": handler_module,
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == aid, rec
    return aid


@pytest.fixture
async def host(seeded_kernel):
    """A web host with no uvicorn serve loop — `_spawn` builds the app and the
    handle; nothing binds until `_start_serving`, which this never calls."""
    web_id = await _mk(
        seeded_kernel, "web.tools", "mount_web", parent_id=seeded_kernel.id, port=8999
    )
    yield web_id
    _servers.pop(web_id, None)


async def test_web_mounts_the_ws_and_rest_legs(seeded_kernel, host):
    await _mk(seeded_kernel, "web_ws.tools", "mount_ws", parent_id=host)
    await _mk(seeded_kernel, "web_rest.tools", "mount_rest", parent_id=host)
    handle = await _spawn(host, seeded_kernel)
    await _mount_all_surfaces(handle, seeded_kernel, host)

    assert set(handle.routes_by_child) == {"mount_ws", "mount_rest"}
    assert [r.path for r in handle.routes_by_child["mount_ws"]] == ["/{host_id}/ws"]
    assert [r.path for r in handle.routes_by_child["mount_rest"]] == [
        "/mount_rest/{target_id}",
        "/mount_rest/_reflect",
        "/mount_rest/_reflect/{target_id}",
    ]
    live = {r.path for r in handle.app.routes}
    assert {"/{host_id}/ws", "/mount_rest/_reflect"} <= live


async def test_a_leg_that_declares_a_route_without_the_hook_fails_loud(
    seeded_kernel, host
):
    """NO FALLBACK — half a surface never mounts silently."""
    mod = types.ModuleType("hookless_leg.tools")

    async def _handler(id, payload, kernel):
        if payload.get("type") == "get_routes":
            return {"routes": [{"kind": "http", "method": "GET", "path": f"/{id}/x"}]}
        return None

    mod.handler = _handler
    mod.VERBS = {}
    sys.modules["hookless_leg.tools"] = mod
    try:
        await _mk(seeded_kernel, "hookless_leg.tools", "mount_bad", parent_id=host)
        handle = await _spawn(host, seeded_kernel)
        with pytest.raises(RuntimeError, match="route_endpoints"):
            await _mount_all_surfaces(handle, seeded_kernel, host)
    finally:
        sys.modules.pop("hookless_leg.tools", None)


async def test_a_literal_route_beats_a_catch_all_mounted_before_it(seeded_kernel, host):
    """Mount order is folder order on a hydrated host; the literal must still win."""
    from starlette.routing import Match

    mod = types.ModuleType("literal_leg.tools")

    async def _handler(id, payload, kernel):
        if payload.get("type") == "get_routes":
            return {"routes": [{"kind": "websocket", "path": "/z_literal/ws"}]}
        return None

    async def _literal(websocket):
        await websocket.accept()
        await websocket.send_text("literal")
        await websocket.close()

    mod.handler = _handler
    mod.VERBS = {}
    mod.route_endpoints = lambda agent: {"/z_literal/ws": _literal}
    sys.modules["literal_leg.tools"] = mod
    try:
        # the catch-all leg sorts (and mounts) FIRST
        await _mk(
            seeded_kernel,
            "web_ws.tools",
            "a_ws",
            parent_id=host,
            ingress_rule="allow_all_ingress",
        )
        await _mk(seeded_kernel, "literal_leg.tools", "z_literal", parent_id=host)
        handle = await _spawn(host, seeded_kernel)
        await _mount_all_surfaces(handle, seeded_kernel, host)
        paths = [r.path for r in handle.app.routes]
        assert paths.index("/z_literal/ws") < paths.index("/{host_id}/ws"), paths
        # starlette's own first-FULL-match walk over the live table
        scope = {"type": "websocket", "path": "/z_literal/ws", "path_params": {}}
        first = next(
            r for r in handle.app.router.routes if r.matches(scope)[0] == Match.FULL
        )
        assert first.path == "/z_literal/ws", first.path
    finally:
        sys.modules.pop("literal_leg.tools", None)
