"""webapp — FastAPI factory routes."""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from web.app import make_app

#: The web host's own authority (`127.0.0.1:<port>` on the loopback default) — any
#: other Host is refused, TestClient's `testserver` included.
OWN = "http://127.0.0.1:8088"


@pytest.fixture
async def web_agent(seeded_kernel):
    """A REAL web-host agent. The `/file/` route hands each fetch to this agent
    (`fetch_file`), which reads as itself, so a browser fetch is an ordinary agent send and BOTH gates
    apply. It therefore needs egress (`can_call`) on whatever it serves, exactly
    like any other caller — the browser inherits the web host's authority, not the
    operator's."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "id": "test_web",
                        "handler_module": "web.tools",
                    },
                }
            ]
        )
    )[0]["result"]
    # The port the Host guard names — set AFTER create, so the create's
    # boot binds nothing; the TestClient addresses `OWN`, this host's own authority.
    (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {"type": "update_agent", "id": rec["id"], "port": 8088},
                }
            ]
        )
    )[0]["result"]
    return rec["id"]


async def _grant_serving(kernel, web_id: str, target_id: str) -> None:
    """Wire the BOTH-ENDS edge that HTTP serving now requires: the web host's
    egress to the target, and the target's ingress for the web host. There is no
    `"*"` wildcard KEY (only a wildcard grant VALUE), so each served agent is
    named — which is the intent: what the browser can reach is an explicit list."""
    cur = kernel.ctx.agents[web_id].record.get("can_call") or {}
    (
        await kernel.send(
            [
                {
                    "target":  # operator origin: authority is conferred from above
                    kernel.id,
                    "payload": {
                        "type": "update_agent",
                        "id": web_id,
                        "can_call": {**cur, target_id: ["read_stream"]},
                    },
                }
            ]
        )
    )[0]["result"]


@pytest.fixture
async def served_bridge(seeded_kernel, web_agent):
    """A file_bridge wired to be HTTP-SERVED: the io leg opened (`ingress_rule`)
    AND the web host admitted (`callable_by`). Both are required — opening only the
    leg leaves it unreachable BY THE BROWSER while still readable in-kernel, which
    is the whole point of the two-sided gate."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "ingress_rule": "allow_all_ingress",
                        "callable_by": {web_agent: ["read_stream"]},
                    },
                }
            ]
        )
    )[0]["result"]
    await _grant_serving(seeded_kernel, web_agent, rec["id"])
    return rec["id"]


@pytest.fixture
def client(seeded_kernel, web_agent):
    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        yield c, seeded_kernel


def test_no_generated_docs_routes(client):
    """FastAPI's `/docs`, `/redoc` and `/openapi.json` are routes nobody declared —
    and the last describes the whole route table. A web agent serves none of them."""
    c, _ = client
    for path in ("/docs", "/redoc", "/openapi.json", "/docs/oauth2-redirect"):
        assert c.get(path).status_code == 404, path


def test_render_route_is_absent(client):
    """The host renders nothing of its own: `/` is undeclared, so it is a 404 —
    no tree index, nothing about the kernel leaks to an anonymous client."""
    c, _ = client
    r = c.get("/")
    assert r.status_code == 404
    assert "kernel_state" not in r.text


# ─── The web host renders no agent UI server-side — there is no `GET /<id>/`
#     render route. Frontend html agents (html_agent/gl views) are JS view-agents in
#     the TS kernel; the host only serves STATIC files (the file route below)
#     and carries the bus. So there's nothing render-side to exercise here.


# ─── /<file_bridge>/file/<path> blob proxy: a URL convention. Any agent that
#     answers `read_stream` becomes an HTTP file server.


async def test_file_proxy_serves_text(
    seeded_kernel, served_bridge, web_agent, tmp_path
):
    p = tmp_path / "hello.txt"
    p.write_text("hi from file")
    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        r = c.get(f"/{served_bridge}/file/hello.txt")
        assert r.status_code == 200
        assert r.text == "hi from file"
        assert "text/plain" in r.headers["content-type"]


async def test_file_proxy_serves_image_bytes(
    seeded_kernel, served_bridge, web_agent, tmp_path
):
    # Synthetic PNG with the magic header so the file_bridge agent flags it as image.
    img_bytes = b"\x89PNG\r\n\x1a\n" + b"FAKEDATA"
    (tmp_path / "img.png").write_bytes(img_bytes)
    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        r = c.get(f"/{served_bridge}/file/img.png")
        assert r.status_code == 200
        assert r.headers["content-type"] == "image/png"
        assert r.content == img_bytes


async def test_file_proxy_serves_pdf_bytes(
    seeded_kernel, served_bridge, web_agent, tmp_path
):
    """Generic binary path: PDF (and any other non-text non-image) is
    served via the file_bridge agent's `bytes` field with mime application/pdf.
    Without this, browser <iframe src="/<file>/file/foo.pdf"> 404s."""
    pdf = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\nbody\n%%EOF\n"
    (tmp_path / "doc.pdf").write_bytes(pdf)
    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        r = c.get(f"/{served_bridge}/file/doc.pdf")
        assert r.status_code == 200
        assert r.headers["content-type"] == "application/pdf"
        assert r.content == pdf


async def test_file_proxy_404_for_missing_path(seeded_kernel, served_bridge, web_agent):
    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        r = c.get(f"/{served_bridge}/file/nope.txt")
        assert r.status_code == 404


async def test_file_proxy_404_for_path_traversal(
    seeded_kernel, served_bridge, web_agent
):
    """file_bridge agent's path-safety bubbles up as `error` → route returns 404."""
    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        r = c.get(f"/{served_bridge}/file/../../etc/passwd")
        assert r.status_code == 404


async def test_file_proxy_404_for_missing_agent(seeded_kernel, web_agent):
    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        r = c.get("/nonexistent_xxx/file/anything.txt")
        assert r.status_code == 404


async def test_file_proxy_streams_large_file(
    seeded_kernel, served_bridge, web_agent, tmp_path
):
    """A file bigger than one read_stream chunk (256 KiB) pipes out via the
    SOURCE stream verb, byte-for-byte, with a correct content-length."""
    blob = bytes(range(256)) * 3000  # 768000 bytes — several chunks
    (tmp_path / "big.bin").write_bytes(blob)
    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        r = c.get(f"/{served_bridge}/file/big.bin")
    assert r.status_code == 200
    assert r.content == blob
    assert r.headers["content-length"] == str(len(blob))


async def test_file_proxy_sealed_agent_404(
    seeded_kernel, web_agent, tmp_path, monkeypatch
):
    """The serving allowance IS the agent's own gate — a SEALED file_bridge
    refuses read_stream/read, so its files 404 (never served)."""
    monkeypatch.chdir(tmp_path)
    seeded_kernel.create(
        "file_bridge.tools", allow=["."], id="sealed_fb", root="."
    )  # no rule → sealed
    # Grant the serving edge so the SEAL is what denies, not a missing grant —
    # otherwise this passes for the wrong reason and stops testing the io gate.
    (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "update_agent",
                        "id": "sealed_fb",
                        "callable_by": {web_agent: ["read_stream"]},
                    },
                }
            ]
        )
    )[0]["result"]
    (tmp_path / "secret.txt").write_text("nope")
    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        r = c.get("/sealed_fb/file/secret.txt")
    assert r.status_code == 404
    assert b"nope" not in r.content


async def test_private_but_internally_readable_is_expressible(
    seeded_kernel, web_agent, tmp_path, monkeypatch
):
    """A file_bridge can be readable by in-kernel agents and NOT by the browser —
    "other agents read these; they are the user's private chat history, not
    browser-facing content."

    The `/file/` route carries the web host's identity, never the OPERATOR's, so
    `ingress_rule` is not a single switch over both origins and `callable_by`
    reaches the HTTP path: opening a leg for an in-kernel reader does not also
    serve it to every browser.
    """
    monkeypatch.chdir(tmp_path)
    (tmp_path / "chat.txt").write_text("private chat history")
    reader = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "id": "in_kernel_reader",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": ".",
                        "can_call": {"transcripts": ["read_stream"]},
                    },
                }
            ]
        )
    )[0]["result"]
    # Leg OPEN, but only the in-kernel reader is admitted — `web` is NOT.
    seeded_kernel.create(
        "file_bridge.tools",
        allow=["."],
        id="transcripts",
        root=".",
        ingress_rule="allow_all_ingress",
        callable_by={"in_kernel_reader": ["read_stream"]},
    )
    await _grant_serving(seeded_kernel, web_agent, "transcripts")  # egress granted…

    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        r = c.get("/transcripts/file/chat.txt")
    assert r.status_code == 404, "the browser must NOT be served"
    assert b"private chat history" not in r.content

    from kernel import _current_sender  # …yet the named in-kernel reader still reads

    token = _current_sender.set(reader["id"])
    try:
        got = (
            await seeded_kernel.ctx.agents[reader["id"]].send(
                [
                    {
                        "target": "transcripts",
                        "payload": {"type": "read_stream", "path": "chat.txt"},
                    }
                ]
            )
        )[0]["result"]
    finally:
        _current_sender.reset(token)
    assert got.get("reason") != "unauthorized", f"in-kernel read was denied: {got!r}"
    assert b"private chat history" in bytes(got.get("bytes") or b"")


async def test_gate_denial_404_carries_the_hint_but_a_missing_file_does_not(
    seeded_kernel, web_agent, served_bridge, tmp_path, monkeypatch
):
    """This kernel teaches through denials, `/file/` included: a bare 404 for every
    cause would make curling a misconfigured URL teach nothing (an io leg opened
    without `web` granted on its call gate looks like success). A MISSING FILE stays bodyless: only the operator's own misconfiguration is
    useless kept secret, and existence is not."""
    monkeypatch.chdir(tmp_path)
    (tmp_path / "site.css").write_text("body{}")
    seeded_kernel.create(
        "file_bridge.tools",
        allow=["."],
        id="ungranted",
        root=".",
        ingress_rule="allow_all_ingress",  # leg OPEN, call gate never admits `web`
    )
    await _grant_serving(seeded_kernel, web_agent, "ungranted")

    app = make_app(web_agent, seeded_kernel)
    with TestClient(app, base_url=OWN) as c:
        denied = c.get("/ungranted/file/site.css")
        # The missing-file half must go through a GRANTED bridge, or it is just a
        # second gate denial and proves nothing about existence leaking.
        missing = c.get(f"/{served_bridge}/file/nope.css")

    assert denied.status_code == 404
    body = denied.json()
    assert body["reason"] == "unauthorized"
    assert body["side"] == "ingress", "the denial must name WHICH gate refused"
    assert "web" in body["error"], f"it must name the rejected caller: {body['error']}"
    assert body["hint"], "and carry the recipe that fixes it"

    # No existence leak: a file that simply is not there says nothing at all.
    assert missing.status_code == 404
    assert missing.content == b""


async def test_browser_reachable_tracks_the_property_not_any_single_field(
    seeded_kernel, web_agent, served_bridge, tmp_path, monkeypatch
):
    """`browser_reachable` must agree with what a GET actually returns, for every
    combination — that is the whole point of computing it instead of describing it.

    A model that sets `ingress_rule=allow_all_ingress` on a public directory without
    granting the web host on its call gate, re-reads the record and sees "allow_all"
    will report the browser can reach it. It cannot. A single field cannot answer the question; three
    gates decide it, and this asserts the computed answer never diverges from the
    real one."""
    monkeypatch.chdir(tmp_path)
    (tmp_path / "f.txt").write_text("bytes")

    async def posture(**meta):
        (
            await seeded_kernel.send(
                [
                    {
                        "target": seeded_kernel.id,
                        "payload": {"type": "delete_agent", "id": "g"},
                    }
                ]
            )
        )[0]["result"]
        seeded_kernel.create("file_bridge.tools", allow=["."], id="g", root=".", **meta)
        await _grant_serving(seeded_kernel, web_agent, "g")  # egress always granted
        listed = (
            "g"
            in (
                (
                    await seeded_kernel.send(
                        [{"target": web_agent, "payload": {"type": "reflect"}}]
                    )
                )[0]["result"]
            )["browser_reachable"]
        )
        app = make_app(web_agent, seeded_kernel)
        with TestClient(app, base_url=OWN) as c:
            served = c.get("/g/file/f.txt").status_code == 200
        assert listed == served, (
            f"browser_reachable said {listed}, a real GET said {served} — the "
            f"computed answer diverged from the property for meta={meta}"
        )
        return served

    admit = {web_agent: ["read_stream"]}
    # leg open + admitted → reachable; drop EITHER and it must disappear from both.
    assert await posture(ingress_rule="allow_all_ingress", callable_by=admit) is True
    assert (
        await posture(ingress_rule="allow_all_ingress") is False
    )  # the six-run failure
    assert await posture(callable_by=admit) is False  # leg sealed
    assert await posture() is False

    # And the bridge the fixture wired correctly is listed.
    rec = (
        await seeded_kernel.send(
            [{"target": web_agent, "payload": {"type": "reflect"}}]
        )
    )[0]["result"]
    assert served_bridge in rec["browser_reachable"]
    assert "callable_by" in rec["routes"]["GET /<agent_id>/file/<path>"]
