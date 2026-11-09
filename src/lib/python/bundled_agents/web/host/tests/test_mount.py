"""webapp — mounting a child's call surface, the two halves.

A leg DECLARES its routes as data (`get_routes`) and hands over the live
endpoints in-process (`route_endpoints(agent)`). `web` needs both; a leg that
declares a route and exposes no hook is broken, and says so at boot.
"""

from __future__ import annotations

import types

import pytest

from web.tools import _mount_all_surfaces, _servers, _spawn
from kernel import register_unlisted


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
    """A web host with no uvicorn serve loop — the record only (`create`, which
    boots nothing); `_spawn` builds the app and the handle, and nothing binds until
    `_start_serving`, which this never calls."""
    web_id = seeded_kernel.create("web.tools", id="mount_web", port=8999)["id"]
    yield web_id
    _servers.pop(web_id, None)
# twin: web_with_ws_child_serves_ws
# twin: web_with_rest_child_serves_rest


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
    register_unlisted("hookless_leg.tools", mod)
    await _mk(seeded_kernel, "hookless_leg.tools", "mount_bad", parent_id=host)
    handle = await _spawn(host, seeded_kernel)
    with pytest.raises(RuntimeError, match="route_endpoints"):
        await _mount_all_surfaces(handle, seeded_kernel, host)


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
    register_unlisted("literal_leg.tools", mod)
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


async def test_a_reboot_replaces_a_legs_routes_and_unmount_removes_them_all(
    seeded_kernel, host
):
    """`boot` on a running host re-mounts every surface. A re-mount REPLACES the
    leg's routes: an appended second copy would leave `unmount` removing only the
    newest one while the first copy kept serving."""
    await _mk(seeded_kernel, "web_rest.tools", "mount_rest", parent_id=host)
    handle = await _spawn(host, seeded_kernel)
    await _mount_all_surfaces(handle, seeded_kernel, host)
    first = [r.path for r in handle.app.routes]
    await _mount_all_surfaces(handle, seeded_kernel, host)  # the re-boot body
    assert [r.path for r in handle.app.routes] == first
    out = (
        await seeded_kernel.send(
            [
                {
                    "target": host,
                    "payload": {"type": "unmount", "child_id": "mount_rest"},
                }
            ]
        )
    )[0]["result"]
    assert out == {"unmounted": "mount_rest"}
    left = [r.path for r in handle.app.routes if r.path.startswith("/mount_rest/")]
    assert left == [], left


# twin: boot_announces_say_to_cli
async def test_boot_announces_the_bound_host(seeded_kernel, monkeypatch):
    """ARCH2-10 — the boot announcement to `cli` names the host the server BINDS
    (`host` on the record), not a hardcoded `127.0.0.1`: a host bound elsewhere
    does not answer there. Nothing binds here — the serve loop is stubbed out, and
    a probe stands in for the seeded `cli` renderer."""
    from web import tools as web_tools

    said: list[str] = []
    mod = types.ModuleType("say_probe.tools")

    async def _handler(id, payload, kernel):
        if payload.get("type") == "say":
            said.append(payload["text"])
        return None

    mod.handler = _handler
    mod.VERBS = {}
    register_unlisted("say_probe.tools", mod)
    monkeypatch.setattr(web_tools, "_start_serving", lambda *a: None)
    try:
        gone = (
            await seeded_kernel.send(
                [{"target": "kernel", "payload": {"type": "delete_agent", "id": "cli"}}]
            )
        )[0]
        assert gone["status"] == "completed", gone
        await _mk(
            seeded_kernel,
            "say_probe.tools",
            "cli",
            parent_id=seeded_kernel.id,
            callable_by={"announce_web": "*"},
        )
        await _mk(
            seeded_kernel,
            "web.tools",
            "announce_web",
            parent_id=seeded_kernel.id,
            port=8998,
            host="192.0.2.7",
            can_call={"cli": "*"},
        )
        assert said == [
            "listening on http://192.0.2.7:8998/"
            " · rest/ws are children (reflect kernel for ids)"
        ], said
    finally:
        _servers.pop("announce_web", None)


def _dup_leg(name: str, spec: dict) -> None:
    """Register a leg bundle declaring ONE route `spec` with a live endpoint."""
    mod = types.ModuleType(f"{name}.tools")

    async def _handler(id, payload, kernel):
        if payload.get("type") == "get_routes":
            return {"routes": [spec]}
        return None

    async def _endpoint(*a, **k):
        return None

    mod.handler = _handler
    mod.VERBS = {}
    mod.route_endpoints = lambda agent: {spec["path"]: _endpoint}
    register_unlisted(f"{name}.tools", mod)


def _VIA_HOST(host: str) -> dict:
    """Let the web host (the `mount` verb's sender) ask the leg for its routes."""
    return {"callable_by": {host: ["get_routes"]}}


async def _host_may_ask(kernel, host: str, *legs: str) -> None:
    out = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "update_agent",
                        "id": host,
                        "can_call": {leg: ["get_routes"] for leg in legs},
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" not in out, out


def _ws(path: str) -> dict:
    return {"kind": "websocket", "path": path}


# twin: two_children_declaring_the_same_literal_route_path_are_refused_at_boot
async def test_two_children_declaring_the_same_literal_route_path_are_refused_at_boot(
    seeded_kernel, host
):
    _dup_leg("dup_a", _ws("/shared/ws"))
    _dup_leg("dup_b", _ws("/shared/ws"))
    await _mk(seeded_kernel, "dup_a.tools", "a_dup", parent_id=host)
    await _mk(seeded_kernel, "dup_b.tools", "b_dup", parent_id=host)
    handle = await _spawn(host, seeded_kernel)
    with pytest.raises(RuntimeError, match="/shared/ws") as exc:
        await _mount_all_surfaces(handle, seeded_kernel, host)
    assert "a_dup" in str(exc.value) and "b_dup" in str(exc.value), exc.value
    assert "b_dup" not in handle.routes_by_child


# twin: a_duplicate_literal_route_is_refused_on_mount_too
async def test_a_duplicate_literal_route_is_refused_on_mount_too(seeded_kernel, host):
    from starlette.routing import Match

    _dup_leg("dup_a", _ws("/shared/ws"))
    _dup_leg("dup_b", _ws("/shared/ws"))
    await _mk(seeded_kernel, "dup_a.tools", "a_dup", parent_id=host, **_VIA_HOST(host))
    await _host_may_ask(seeded_kernel, host, "a_dup", "b_dup")
    handle = await _spawn(host, seeded_kernel)
    await _mount_all_surfaces(handle, seeded_kernel, host)
    await _mk(seeded_kernel, "dup_b.tools", "b_dup", parent_id=host, **_VIA_HOST(host))
    out = (
        await seeded_kernel.send(
            [{"target": host, "payload": {"type": "mount", "child_id": "b_dup"}}]
        )
    )[0]["result"]
    err = out["error"]
    assert "/shared/ws" in err and "a_dup" in err and "b_dup" in err, out
    assert "b_dup" not in handle.routes_by_child
    scope = {"type": "websocket", "path": "/shared/ws", "path_params": {}}
    first = next(
        r for r in handle.app.router.routes if r.matches(scope)[0] == Match.FULL
    )
    assert first in handle.routes_by_child["a_dup"]


# twin: a_second_catch_all_ws_leg_is_refused_at_boot_naming_both
async def test_a_second_catch_all_ws_leg_is_refused_at_boot_naming_both(
    seeded_kernel, host
):
    for aid in ("a_ws", "b_ws"):
        await _mk(
            seeded_kernel,
            "web_ws.tools",
            aid,
            parent_id=host,
            ingress_rule="allow_all_ingress",
        )
    handle = await _spawn(host, seeded_kernel)
    with pytest.raises(RuntimeError) as exc:
        await _mount_all_surfaces(handle, seeded_kernel, host)
    msg = str(exc.value)
    for part in ("/{host_id}/ws", "a_ws", "b_ws", "catch-all"):
        assert part in msg, msg
    assert "b_ws" not in handle.routes_by_child
    assert handle.task is None


# twin: a_second_catch_all_ws_leg_is_refused_on_mount_too
async def test_a_second_catch_all_ws_leg_is_refused_on_mount_too(seeded_kernel, host):
    from starlette.routing import Match

    _dup_leg("dup_a", _ws("/{host_id}/ws"))
    _dup_leg("dup_b", _ws("/{host_id}/ws"))
    await _mk(seeded_kernel, "dup_a.tools", "a_dup", parent_id=host, **_VIA_HOST(host))
    await _host_may_ask(seeded_kernel, host, "a_dup", "b_dup")
    handle = await _spawn(host, seeded_kernel)
    await _mount_all_surfaces(handle, seeded_kernel, host)
    await _mk(seeded_kernel, "dup_b.tools", "b_dup", parent_id=host, **_VIA_HOST(host))
    out = (
        await seeded_kernel.send(
            [{"target": host, "payload": {"type": "mount", "child_id": "b_dup"}}]
        )
    )[0]["result"]
    err = out["error"]
    assert "/{host_id}/ws" in err and "a_dup" in err and "b_dup" in err, out
    assert "b_dup" not in handle.routes_by_child
    scope = {"type": "websocket", "path": "/anything/ws", "path_params": {}}
    first = next(
        r for r in handle.app.router.routes if r.matches(scope)[0] == Match.FULL
    )
    assert first in handle.routes_by_child["a_dup"]


# twin: tests_route_order.rs (fantastic-web)
async def test_a_web_ws_route_leg_beside_a_catch_all_leg_gets_its_own_url(
    seeded_kernel, host
):
    import json

    from fastapi.testclient import TestClient
    from starlette.routing import Match

    from _testkit import wire

    await _mk(
        seeded_kernel,
        "web_ws.tools",
        "a_ws",
        parent_id=host,
        ingress_rule="allow_all_ingress",
    )
    await _mk(
        seeded_kernel,
        "web_ws.tools",
        "z_page_ws",
        parent_id=host,
        ingress_rule="allow_all_ingress",
        route="/z_page_ws/ws",
    )
    handle = await _spawn(host, seeded_kernel)
    await _mount_all_surfaces(handle, seeded_kernel, host)

    def first(path):
        scope = {"type": "websocket", "path": path, "path_params": {}}
        return next(
            r for r in handle.app.router.routes if r.matches(scope)[0] == Match.FULL
        )

    r = first("/z_page_ws/ws")
    assert r.path == "/z_page_ws/ws" and r in handle.routes_by_child["z_page_ws"]
    r = first("/user/ws")
    assert r.path == "/{host_id}/ws" and r in handle.routes_by_child["a_ws"]
    assert (
        sum(1 for r in handle.app.router.routes if getattr(r, "path", "") == "/{host_id}/ws")
        == 1
    )

    wire(seeded_kernel, "z_page_ws", "z_page_ws")
    wire(seeded_kernel, "a_ws", "a_ws")

    def reply(ws, call_id):
        for _ in range(10):
            msg = json.loads(ws.receive_text())
            if msg.get("type") == "reply" and msg.get("id") == call_id:
                return msg["data"]
        raise AssertionError(f"no reply for id={call_id}")

    def ask(c, path, target, call_id):
        with c.websocket_connect(path) as ws:
            ws.send_text(
                json.dumps(
                    {
                        "type": "call",
                        "target": target,
                        "payload": {"type": "reflect"},
                        "id": call_id,
                    }
                )
            )
            return reply(ws, call_id)

    with TestClient(handle.app, headers={"host": "127.0.0.1:8999"}) as c:
        d = ask(c, "/z_page_ws/ws", "z_page_ws", "r1")
        assert d["id"] == "z_page_ws" and d["path_pattern"] == "/z_page_ws/ws", d
        d = ask(c, "/a_ws/ws", "a_ws", "r2")
        assert d["id"] == "a_ws" and d["path_pattern"] == "/{host_id}/ws", d


# twin: the_same_literal_path_with_different_kinds_or_methods_is_allowed
async def test_the_same_literal_path_with_different_kinds_or_methods_is_allowed(
    seeded_kernel, host
):
    path = "/shared/_reflect"
    _dup_leg("dup_a", _ws(path))
    _dup_leg("dup_b", {"kind": "http", "method": "GET", "path": path})
    _dup_leg("dup_c", {"kind": "http", "method": "POST", "path": path})
    for aid, mod in (("a_dup", "dup_a"), ("b_dup", "dup_b"), ("c_dup", "dup_c")):
        await _mk(seeded_kernel, f"{mod}.tools", aid, parent_id=host)
    handle = await _spawn(host, seeded_kernel)
    await _mount_all_surfaces(handle, seeded_kernel, host)
    for aid in ("a_dup", "b_dup", "c_dup"):
        assert [r.path for r in handle.routes_by_child[aid]] == [path], aid


# twin: a_reboot_of_one_child_does_not_conflict_with_itself
async def test_a_reboot_of_one_child_does_not_conflict_with_itself(
    seeded_kernel, host
):
    _dup_leg("dup_a", _ws("/shared/ws"))
    await _mk(seeded_kernel, "dup_a.tools", "a_dup", parent_id=host, **_VIA_HOST(host))
    await _host_may_ask(seeded_kernel, host, "a_dup")
    handle = await _spawn(host, seeded_kernel)
    await _mount_all_surfaces(handle, seeded_kernel, host)
    out = (
        await seeded_kernel.send(
            [{"target": host, "payload": {"type": "mount", "child_id": "a_dup"}}]
        )
    )[0]["result"]
    assert "error" not in out, out
    assert out["mounted"] == ["/shared/ws"]
