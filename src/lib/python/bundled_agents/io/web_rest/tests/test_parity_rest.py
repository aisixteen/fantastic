"""web_rest — twins of the Rust `fantastic-web` / `fantastic-web-rest` tests (#170).

Rust is the spec; each test is named `test_<rust_test>`.
"""

from __future__ import annotations

from fastapi import FastAPI
from fastapi.testclient import TestClient

from web_rest.tools import _get_routes, handler, route_endpoints
from _testkit import wire


async def _mounted(kernel, rest_id: str) -> FastAPI:
    """The leg's whole surface on a fresh app, in the order it DECLARES (what the
    web host does at boot)."""
    app = FastAPI()
    eps = route_endpoints(kernel.ctx.agents[rest_id])
    for spec in (await _get_routes(rest_id, {}, kernel))["routes"]:
        app.add_api_route(spec["path"], eps[spec["path"]], methods=[spec["method"]])
    return app


async def test_unknown_verb_is_unhandled_refusal(seeded_kernel):
    reply = await handler("rest", {"type": "shutdown"}, seeded_kernel)
    assert reply == {
        "error": "web_rest: unknown type 'shutdown'",
        "reason": "unhandled",
        "verb": "shutdown",
    }
    assert list(reply) == ["error", "reason", "verb"]


async def test_post_to_reflect_literal_dispatches_to_that_target(seeded_kernel):
    seeded_kernel.create("web_rest.tools", id="rest", ingress_rule="allow_all_ingress")
    wire(seeded_kernel, "rest", "kernel_state")
    app = await _mounted(seeded_kernel, "rest")
    with TestClient(app) as c:
        # `POST /rest/_reflect` hits the POST route's `{target_id}` (the literal
        # `_reflect` route is a GET, so it cannot shadow it)
        r = c.post("/rest/_reflect", json={"type": "reflect"})
        assert r.status_code == 200, "dispatches to target `_reflect`"
        body = r.json()
        assert body["reason"] == "unknown_agent", body
        # the GET on the same path IS the reflect route (it reflects the root)
        g = c.get("/rest/_reflect")
        assert g.status_code == 200 and g.json().get("id") == "kernel_state", g.text


async def test_json_leg_write_stream_string_bytes_is_missing_arg(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    (tmp_path / "served").mkdir()
    seeded_kernel.create(
        "web_rest.tools",
        id="rest",
        ingress_rule="allow_all_ingress",
        can_call={"fb": ["*"]},
    )
    wire(seeded_kernel, "rest", "kernel_state")
    seeded_kernel.create(
        "file_bridge.tools",
        allow=["."],
        id="fb",
        root="served",
        ingress_rule="allow_all_ingress",
        callable_by={"rest": ["*"]},
    )
    app = await _mounted(seeded_kernel, "rest")
    with TestClient(app) as c:
        r = c.post(
            "/rest/fb",
            json={"type": "write_stream", "path": "ws_text.txt", "bytes": "abc"},
        )
    assert r.status_code == 200
    body = r.json()
    assert body["reason"] == "missing_arg", body
    assert body["error"] == "write_stream: bytes (one raw chunk) required", body
    assert not (tmp_path / "served" / "ws_text.txt").exists(), (
        "the refused write landed"
    )
