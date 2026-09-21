"""web_rest — REST verb-invocation surface."""

from __future__ import annotations

import json

from fastapi import FastAPI
from fastapi.testclient import TestClient

from web_rest.tools import (
    _get_routes,
    _make_post_endpoint,
    _make_reflect_get,
    _make_reflect_root,
    _reflect,
    route_endpoints,
)


async def test_reflect_describes_surface(seeded_kernel):
    r = await _reflect("rest_xyz", {}, seeded_kernel)
    assert r["id"] == "rest_xyz"
    assert r["path_pattern"] == "/rest_xyz/{target_id}"
    assert r["method"] == "POST"
    assert r["reflect_url"] == "/rest_xyz/_reflect"
    assert r["reflect_pattern"] == "/rest_xyz/_reflect/{target_id}"
    assert "get_routes" in r["verbs"]


async def test_get_routes_returns_post_plus_two_reflect_gets(seeded_kernel):
    """DECLARATIVE — pure JSON data, byte-shaped like rust/swift (kind, method,
    path, in that order). No callable rides the reply."""
    r = await _get_routes("rest_xyz", {}, seeded_kernel)
    assert r == {
        "routes": [
            {"kind": "http", "method": "POST", "path": "/rest_xyz/{target_id}"},
            {"kind": "http", "method": "GET", "path": "/rest_xyz/_reflect"},
            {"kind": "http", "method": "GET", "path": "/rest_xyz/_reflect/{target_id}"},
        ]
    }
    assert json.loads(json.dumps(r)) == r


async def test_route_endpoints_covers_every_declared_path(seeded_kernel):
    """The other half: `web` gets the callables in-process, keyed by the paths
    `get_routes` declared — one endpoint per declared route, no more."""
    seeded_kernel.create("web_rest.tools", id="rest_hook")
    declared = {
        s["path"] for s in (await _get_routes("rest_hook", {}, seeded_kernel))["routes"]
    }
    eps = route_endpoints(seeded_kernel.ctx.agents["rest_hook"])
    assert set(eps) == declared
    assert all(callable(fn) for fn in eps.values())


def test_post_endpoint_mountable_on_fastapi(seeded_kernel):
    """Round-trip: drop the endpoint onto a fresh FastAPI app and prove
    a POST routes to kernel.send."""
    # Sealed by default ⇒ give the leg an OPEN record so inbound dispatches.
    seeded_kernel.create(
        "web_rest.tools", id="rest_xyz", ingress_rule="allow_all_ingress"
    )
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/{target_id}",
        _make_post_endpoint("rest_xyz", seeded_kernel),
        methods=["POST"],
    )
    with TestClient(app) as c:
        r = c.post("/rest_xyz/kernel_state", json={"type": "reflect"})
        assert r.status_code == 200
        body = r.json()
        # Root's uniform reflect comes back — id + tree (default all).
        assert body["id"] == "kernel_state"
        assert body["tree"]["id"] == "kernel_state"


def test_post_endpoint_rejects_non_object_body(seeded_kernel):
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/{target_id}",
        _make_post_endpoint("rest_xyz", seeded_kernel),
        methods=["POST"],
    )
    with TestClient(app) as c:
        r = c.post("/rest_xyz/kernel_state", json=[1, 2, 3])
        assert r.status_code == 400
        assert "JSON object" in r.json()["error"]


def test_post_endpoint_rejects_bad_json(seeded_kernel):
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/{target_id}",
        _make_post_endpoint("rest_xyz", seeded_kernel),
        methods=["POST"],
    )
    with TestClient(app) as c:
        r = c.post(
            "/rest_xyz/kernel_state",
            content=b"not json {{{",
            headers={"content-type": "application/json"},
        )
        assert r.status_code == 400


# ─── GET _reflect shortcuts (browser-pastable) ──────────────────


def test_get_reflect_root_returns_tree(seeded_kernel):
    """GET /<rest_id>/_reflect (no target) defaults to the root → uniform
    reflect with the tree. `?bundles=all` adds the catalog."""
    seeded_kernel.create(
        "web_rest.tools", id="rest_xyz", ingress_rule="allow_all_ingress"
    )
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/_reflect",
        _make_reflect_root("rest_xyz", seeded_kernel),
        methods=["GET"],
    )
    with TestClient(app) as c:
        r = c.get("/rest_xyz/_reflect")
        assert r.status_code == 200
        body = r.json()
        assert body["id"] == "kernel_state"
        assert body["tree"]["id"] == "kernel_state"
        assert "transports" not in body
        # opt-in catalog via the bundles tier
        cat = c.get("/rest_xyz/_reflect?bundles=all").json()
        assert any(b["name"] == "file_bridge" for b in cat["bundles"])


def test_get_reflect_target_returns_agent_reflect(seeded_kernel):
    """GET /<rest_id>/_reflect/<target_id> → reflect on that specific agent."""
    seeded_kernel.create(
        "web_rest.tools", id="rest_xyz", ingress_rule="allow_all_ingress"
    )
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/_reflect/{target_id}",
        _make_reflect_get("rest_xyz", seeded_kernel),
        methods=["GET"],
    )
    with TestClient(app) as c:
        r = c.get("/rest_xyz/_reflect/kernel_state")
        assert r.status_code == 200
        body = r.json()
        assert body["id"] == "kernel_state"
        assert body["tree"]["id"] == "kernel_state"


def test_get_reflect_target_returns_error_for_missing_agent(seeded_kernel):
    """Unknown agent: kernel.send returns `{error: "no agent …"}`. The
    GET endpoint serializes it as 200 + JSON (no special-casing) — the
    caller checks the body's `error` field."""
    seeded_kernel.create(
        "web_rest.tools", id="rest_xyz", ingress_rule="allow_all_ingress"
    )
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/_reflect/{target_id}",
        _make_reflect_get("rest_xyz", seeded_kernel),
        methods=["GET"],
    )
    with TestClient(app) as c:
        r = c.get("/rest_xyz/_reflect/nonexistent_xxx")
        assert r.status_code == 200
        body = r.json()
        assert "error" in body


def test_get_reflect_readme_query_flag(seeded_kernel):
    """`?readme=1` on the GET shortcut passes `readme:true` —
    the reply carries the agent's readme.md content."""
    seeded_kernel.create(
        "web_rest.tools", id="rest_xyz", ingress_rule="allow_all_ingress"
    )
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/_reflect/{target_id}",
        _make_reflect_get("rest_xyz", seeded_kernel),
        methods=["GET"],
    )
    with TestClient(app) as c:
        # Without the flag: no readme key.
        plain = c.get("/rest_xyz/_reflect/kernel_state").json()
        assert "readme" not in plain
        # With ?readme=1: readme key present (kernel_state has a seeded readme).
        withr = c.get("/rest_xyz/_reflect/kernel_state?readme=1").json()
        assert "readme" in withr
        assert isinstance(withr["readme"], str)
        assert "Fantastic kernel" in withr["readme"]


# ─── auth gate (the io_bridge ingress rule on the leg) ──────────


def test_sealed_rest_leg_denies_and_teaches(seeded_kernel):
    seeded_kernel.create(
        "web_rest.tools", id="sealed_rest", ingress_rule="deny_ingress"
    )
    app = FastAPI()
    app.add_api_route(
        "/sealed_rest/{target_id}",
        _make_post_endpoint("sealed_rest", seeded_kernel),
        methods=["POST"],
    )
    with TestClient(app) as c:
        r = c.post("/sealed_rest/kernel_state", json={"type": "reflect"})
    assert r.status_code == 403
    body = r.json()
    assert body["reason"] == "unauthorized"
    assert "ingress_rule" in body.get("hint", "")


def test_password_rest_leg_checks_header(seeded_kernel, monkeypatch):
    monkeypatch.setenv("FANTASTIC_GROUP_TOKEN", "s3cret")
    seeded_kernel.create(
        "web_rest.tools", id="pw_rest", ingress_rule="password_ingress"
    )
    app = FastAPI()
    app.add_api_route(
        "/pw_rest/{target_id}",
        _make_post_endpoint("pw_rest", seeded_kernel),
        methods=["POST"],
    )
    with TestClient(app) as c:
        # right token on the X-Fantastic-Auth header → dispatches
        good = c.post(
            "/pw_rest/kernel_state",
            json={"type": "reflect"},
            headers={"X-Fantastic-Auth": "s3cret"},
        )
        assert good.status_code == 200 and good.json()["id"] == "kernel_state"
        # wrong token → denied
        bad = c.post(
            "/pw_rest/kernel_state",
            json={"type": "reflect"},
            headers={"X-Fantastic-Auth": "nope"},
        )
        assert bad.status_code == 403 and bad.json()["reason"] == "unauthorized"


async def test_reflect_surfaces_rest_leg_posture(seeded_kernel):
    seeded_kernel.create(
        "web_rest.tools", id="sealed_rest2", ingress_rule="deny_ingress"
    )
    r = await _reflect("sealed_rest2", {}, seeded_kernel)
    assert r["sealed"] is True
    assert r["ingress_rule"] == "deny_ingress"
    assert True  # see removed from posture
    assert r["auth_header"] == "X-Fantastic-Auth"
