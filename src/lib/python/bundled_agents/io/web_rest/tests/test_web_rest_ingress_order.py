"""web_rest judges a request BEFORE it reads the body, and bounds the body.

Buffering (`await request.body()`) and parsing (`strict_loads`) the WHOLE body
before the leg's ingress rule runs would make a sealed or password-gated leg hold
and parse any amount of data from a client it is about to refuse. The rule judges the request on what it says up front
(its target and its `X-Fantastic-Auth` credential), and only an admitted request's
body is read — at most `MAX_BODY` bytes (16 MiB, io_bridge's `MAX_FRAME`: the one
frame cap every wire leg holds); past it the request is refused, loud, 413.

At this leg a non-ASCII credential is a plain 403.
"""

from __future__ import annotations

import json

from fastapi import FastAPI

from io_bridge import MAX_FRAME
from web_rest import tools as rest
from web_rest.tools import _make_post_endpoint
from _testkit import wire

SEALED_ERROR = "edge sealed by policy"


def _app(kernel, leg: str) -> FastAPI:
    app = FastAPI()
    app.add_api_route(
        f"/{leg}/{{target_id}}", _make_post_endpoint(leg, kernel), methods=["POST"]
    )
    return app


async def _post(app, path: str, chunks: list[bytes], headers: dict) -> tuple:
    """One POST through the ASGI app, the body handed over chunk by chunk —
    returns `(status, json body, bytes the app asked for)`."""
    queue = list(chunks)
    read: list[int] = []

    async def receive():
        if queue:
            body = queue.pop(0)
            read.append(len(body))
            return {"type": "http.request", "body": body, "more_body": bool(queue)}
        return {"type": "http.disconnect"}

    sent: list[dict] = []

    async def send(msg):
        sent.append(msg)

    scope = {
        "type": "http",
        "asgi": {"version": "3.0"},
        "http_version": "1.1",
        "method": "POST",
        "scheme": "http",
        "path": path,
        "raw_path": path.encode(),
        "query_string": b"",
        "root_path": "",
        "headers": [
            (k.lower().encode(), v if isinstance(v, bytes) else v.encode())
            for k, v in headers.items()
        ],
        "server": ("test", 80),
        "client": ("test", 1),
    }
    await app(scope, receive, send)
    status = next(m["status"] for m in sent if m["type"] == "http.response.start")
    raw = b"".join(
        m.get("body", b"") for m in sent if m["type"] == "http.response.body"
    )
    return status, (json.loads(raw) if raw else None), sum(read)


JSON = {"content-type": "application/json"}


async def test_a_sealed_leg_refuses_before_reading_the_body(seeded_kernel):
    seeded_kernel.create("web_rest.tools", id="rs")
    wire(seeded_kernel, "rs", "kernel_state")
    app = _app(seeded_kernel, "rs")
    junk = [b"{not json at all " * 64] * 8
    status, body, read = await _post(app, "/rs/kernel_state", junk, JSON)
    assert status == 403 and body["error"] == SEALED_ERROR, (status, body)
    assert read == 0, f"the sealed leg read {read} body bytes"


async def test_a_wrong_credential_is_refused_before_reading_the_body(
    seeded_kernel, monkeypatch
):
    monkeypatch.setenv("FANTASTIC_GROUP_TOKEN", "grp")
    seeded_kernel.create("web_rest.tools", id="pw", ingress_rule="password_ingress")
    wire(seeded_kernel, "pw", "kernel_state")
    app = _app(seeded_kernel, "pw")
    body = [json.dumps({"type": "reflect"}).encode()]
    for token in (b"nope", "été".encode("latin-1"), "päss".encode()):
        status, out, read = await _post(
            app, "/pw/kernel_state", body, {**JSON, "X-Fantastic-Auth": token}
        )
        assert status == 403 and out["reason"] == "unauthorized", (token, status, out)
        assert read == 0, token
    status, out, read = await _post(
        app, "/pw/kernel_state", body, {**JSON, "X-Fantastic-Auth": "grp"}
    )
    assert status == 200 and out["id"] == "kernel_state", out
    assert read == len(body[0])


def test_the_body_bound_is_the_one_frame_cap():
    assert rest.MAX_BODY == MAX_FRAME == 16 * 1024 * 1024


def _too_large(n: int) -> dict:
    return {
        "error": f"web_rest: the request body exceeds {n} bytes — the one frame cap "
        "every wire leg holds (io_bridge MAX_FRAME); send larger data in chunks "
        "(write_stream)",
        "reason": "transport_error",
        "limit": n,
    }


async def test_a_declared_oversize_body_is_refused_unread(seeded_kernel, monkeypatch):
    monkeypatch.setattr(rest, "MAX_BODY", 64)
    seeded_kernel.create("web_rest.tools", id="ro", ingress_rule="allow_all_ingress")
    wire(seeded_kernel, "ro", "kernel_state")
    app = _app(seeded_kernel, "ro")
    big = [b"x" * 65]
    status, out, read = await _post(
        app, "/ro/kernel_state", big, {**JSON, "content-length": "65"}
    )
    assert (status, out) == (413, _too_large(64)), (status, out)
    assert read == 0


# twin: the_capped_read_stops_at_the_bound
async def test_an_undeclared_oversize_body_stops_at_the_bound(
    seeded_kernel, monkeypatch
):
    """No Content-Length (a chunked body): read until the bound is crossed, then
    refused — the rest is never asked for."""
    monkeypatch.setattr(rest, "MAX_BODY", 64)
    seeded_kernel.create("web_rest.tools", id="ro", ingress_rule="allow_all_ingress")
    wire(seeded_kernel, "ro", "kernel_state")
    app = _app(seeded_kernel, "ro")
    chunks = [b"y" * 40] * 10
    status, out, read = await _post(app, "/ro/kernel_state", chunks, JSON)
    assert (status, out) == (413, _too_large(64)), (status, out)
    assert read == 80, f"read {read} bytes; the bound was crossed at 80"
    ok = [json.dumps({"type": "reflect", "tree": "none"}).encode()]
    status, out, _ = await _post(app, "/ro/kernel_state", ok, JSON)
    assert status == 200 and out["id"] == "kernel_state", out
