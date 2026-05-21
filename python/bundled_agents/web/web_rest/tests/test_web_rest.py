"""web_rest — REST verb-invocation surface."""

from __future__ import annotations

from fastapi import FastAPI
from fastapi.testclient import TestClient

from web_rest.tools import (
    _get_routes,
    _make_endpoint,
    _make_reflect_get,
    _make_reflect_root,
    _reflect,
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
    r = await _get_routes("rest_xyz", {}, seeded_kernel)
    routes = r["routes"]
    assert len(routes) == 3
    by_path = {(spec["method"], spec["path"]): spec for spec in routes}
    # Generic POST verb channel.
    assert ("POST", "/rest_xyz/{target_id}") in by_path
    # GET shortcut — default reflect (kernel/primer).
    assert ("GET", "/rest_xyz/_reflect") in by_path
    # GET shortcut — reflect any agent.
    assert ("GET", "/rest_xyz/_reflect/{target_id}") in by_path
    for spec in routes:
        assert spec["kind"] == "http"
        assert callable(spec["endpoint"])


def test_post_endpoint_mountable_on_fastapi(seeded_kernel):
    """Round-trip: drop the endpoint onto a fresh FastAPI app and prove
    a POST routes to kernel.send."""
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/{target_id}",
        _make_endpoint("rest_xyz", seeded_kernel),
        methods=["POST"],
    )
    with TestClient(app) as c:
        r = c.post("/rest_xyz/core", json={"type": "reflect"})
        assert r.status_code == 200
        body = r.json()
        # The substrate primer comes back — has transports + tree.
        assert "transports" in body
        assert "tree" in body


def test_post_endpoint_rejects_non_object_body(seeded_kernel):
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/{target_id}",
        _make_endpoint("rest_xyz", seeded_kernel),
        methods=["POST"],
    )
    with TestClient(app) as c:
        r = c.post("/rest_xyz/core", json=[1, 2, 3])
        assert r.status_code == 400
        assert "JSON object" in r.json()["error"]


def test_post_endpoint_rejects_bad_json(seeded_kernel):
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/{target_id}",
        _make_endpoint("rest_xyz", seeded_kernel),
        methods=["POST"],
    )
    with TestClient(app) as c:
        r = c.post(
            "/rest_xyz/core",
            content=b"not json {{{",
            headers={"content-type": "application/json"},
        )
        assert r.status_code == 400


# ─── GET _reflect shortcuts (browser-pastable) ──────────────────


def test_get_reflect_root_returns_primer(seeded_kernel):
    """GET /<rest_id>/_reflect (no target) defaults to kernel → primer."""
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
        # The substrate primer has transports + tree + available_bundles.
        assert "transports" in body
        assert "tree" in body
        assert "available_bundles" in body


def test_get_reflect_target_returns_agent_reflect(seeded_kernel):
    """GET /<rest_id>/_reflect/<target_id> → reflect on that specific agent."""
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/_reflect/{target_id}",
        _make_reflect_get("rest_xyz", seeded_kernel),
        methods=["GET"],
    )
    with TestClient(app) as c:
        r = c.get("/rest_xyz/_reflect/core")
        assert r.status_code == 200
        body = r.json()
        # core reflect IS the primer (root's reflect returns the primer).
        assert "transports" in body
        assert "tree" in body


def test_get_reflect_target_returns_error_for_missing_agent(seeded_kernel):
    """Unknown agent: kernel.send returns `{error: "no agent …"}`. The
    GET endpoint serializes it as 200 + JSON (no special-casing) — the
    caller checks the body's `error` field."""
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
    """`?readme=1` on the GET shortcut passes `return_readme:true` —
    the reply carries the agent's readme.md content."""
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/_reflect/{target_id}",
        _make_reflect_get("rest_xyz", seeded_kernel),
        methods=["GET"],
    )
    with TestClient(app) as c:
        # Without the flag: no readme key.
        plain = c.get("/rest_xyz/_reflect/core").json()
        assert "readme" not in plain
        # With ?readme=1: readme key present (core has a seeded readme).
        withr = c.get("/rest_xyz/_reflect/core?readme=1").json()
        assert "readme" in withr
        assert isinstance(withr["readme"], str)
        assert "Fantastic kernel" in withr["readme"]
