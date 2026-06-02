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


# ─── /<file_agent>/file/<path> blob proxy: replaces content_alias_file
#     with a URL convention. Any agent that answers `read{path}` becomes
#     an HTTP file server.


async def test_file_proxy_serves_text(seeded_kernel, file_agent, tmp_path):
    p = tmp_path / "hello.txt"
    p.write_text("hi from file")
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_agent}/file/hello.txt")
        assert r.status_code == 200
        assert r.text == "hi from file"
        assert "text/plain" in r.headers["content-type"]


async def test_file_proxy_serves_image_bytes(seeded_kernel, file_agent, tmp_path):
    # Synthetic PNG with the magic header so the file agent flags it as image.
    img_bytes = b"\x89PNG\r\n\x1a\n" + b"FAKEDATA"
    (tmp_path / "img.png").write_bytes(img_bytes)
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_agent}/file/img.png")
        assert r.status_code == 200
        assert r.headers["content-type"] == "image/png"
        assert r.content == img_bytes


async def test_file_proxy_serves_pdf_bytes(seeded_kernel, file_agent, tmp_path):
    """Generic binary path: PDF (and any other non-text non-image) is
    served via the file agent's `bytes` field with mime application/pdf.
    Without this, browser <iframe src="/<file>/file/foo.pdf"> 404s."""
    pdf = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\nbody\n%%EOF\n"
    (tmp_path / "doc.pdf").write_bytes(pdf)
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_agent}/file/doc.pdf")
        assert r.status_code == 200
        assert r.headers["content-type"] == "application/pdf"
        assert r.content == pdf


async def test_file_proxy_404_for_missing_path(seeded_kernel, file_agent):
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_agent}/file/nope.txt")
        assert r.status_code == 404


async def test_file_proxy_404_for_path_traversal(seeded_kernel, file_agent):
    """file agent's path-safety bubbles up as `error` → route returns 404."""
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_agent}/file/../../etc/passwd")
        assert r.status_code == 404


async def test_file_proxy_404_for_missing_agent(seeded_kernel):
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get("/nonexistent_xxx/file/anything.txt")
        assert r.status_code == 404
