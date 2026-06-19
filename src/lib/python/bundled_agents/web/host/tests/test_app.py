"""webapp — FastAPI factory routes."""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from web.app import make_app


@pytest.fixture
def client(seeded_kernel):
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        yield c, seeded_kernel


def test_root_returns_html(client):
    c, _ = client
    r = c.get("/")
    assert r.status_code == 200
    assert "<html>" in r.text.lower() or "<!doctype" in r.text.lower()


# ─── The web host renders no agent UI server-side — there is no `GET /<id>/`
#     render route. Frontend panels (html_agent/gl views) are JS view-agents in
#     the TS kernel; the host only serves STATIC files (the `file` alias below)
#     and carries the bus. So there's nothing render-side to exercise here.


# ─── /<file_bridge>/file/<path> blob proxy: replaces content_alias_file
#     with a URL convention. Any agent that answers `read{path}` becomes
#     an HTTP file server.


async def test_file_proxy_serves_text(seeded_kernel, file_bridge, tmp_path):
    p = tmp_path / "hello.txt"
    p.write_text("hi from file")
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_bridge}/file/hello.txt")
        assert r.status_code == 200
        assert r.text == "hi from file"
        assert "text/plain" in r.headers["content-type"]


async def test_file_proxy_serves_image_bytes(seeded_kernel, file_bridge, tmp_path):
    # Synthetic PNG with the magic header so the file_bridge agent flags it as image.
    img_bytes = b"\x89PNG\r\n\x1a\n" + b"FAKEDATA"
    (tmp_path / "img.png").write_bytes(img_bytes)
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_bridge}/file/img.png")
        assert r.status_code == 200
        assert r.headers["content-type"] == "image/png"
        assert r.content == img_bytes


async def test_file_proxy_serves_pdf_bytes(seeded_kernel, file_bridge, tmp_path):
    """Generic binary path: PDF (and any other non-text non-image) is
    served via the file_bridge agent's `bytes` field with mime application/pdf.
    Without this, browser <iframe src="/<file>/file/foo.pdf"> 404s."""
    pdf = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\nbody\n%%EOF\n"
    (tmp_path / "doc.pdf").write_bytes(pdf)
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_bridge}/file/doc.pdf")
        assert r.status_code == 200
        assert r.headers["content-type"] == "application/pdf"
        assert r.content == pdf


async def test_file_proxy_404_for_missing_path(seeded_kernel, file_bridge):
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_bridge}/file/nope.txt")
        assert r.status_code == 404


async def test_file_proxy_404_for_path_traversal(seeded_kernel, file_bridge):
    """file_bridge agent's path-safety bubbles up as `error` → route returns 404."""
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_bridge}/file/../../etc/passwd")
        assert r.status_code == 404


async def test_file_proxy_404_for_missing_agent(seeded_kernel):
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get("/nonexistent_xxx/file/anything.txt")
        assert r.status_code == 404


async def test_file_proxy_streams_large_file(seeded_kernel, file_bridge, tmp_path):
    """A file bigger than one read_stream chunk (256 KiB) pipes out via the
    SOURCE stream verb, byte-for-byte, with a correct content-length."""
    blob = bytes(range(256)) * 3000  # 768000 bytes — several chunks
    (tmp_path / "big.bin").write_bytes(blob)
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_bridge}/file/big.bin")
    assert r.status_code == 200
    assert r.content == blob
    assert r.headers["content-length"] == str(len(blob))


async def test_file_proxy_sealed_agent_404(seeded_kernel, tmp_path, monkeypatch):
    """The serving allowance IS the agent's own gate — a SEALED file_bridge
    refuses read_stream/read, so its files 404 (never served)."""
    monkeypatch.chdir(tmp_path)
    seeded_kernel.create(
        "file_bridge.tools", id="sealed_fb", root="."
    )  # no rule → sealed
    (tmp_path / "secret.txt").write_text("nope")
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get("/sealed_fb/file/secret.txt")
    assert r.status_code == 404
    assert b"nope" not in r.content
