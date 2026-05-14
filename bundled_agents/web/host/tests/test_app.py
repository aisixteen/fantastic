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


def test_transport_js_served(client):
    c, _ = client
    r = c.get("/_fantastic/transport.js")
    assert r.status_code == 200
    assert "fantastic_transport" in r.text
    # Universal page-reload listener — drift guard. ANY agent that
    # emits {type:'reload_html'} on its inbox triggers location.reload()
    # in every served page (html_agent.set_html, canvas reload button,
    # future bundles that opt in).
    assert "reload_html" in r.text
    assert "location.reload" in r.text


def test_agent_index_404_for_missing(client):
    c, _ = client
    r = c.get("/nonexistent_xxx/")
    assert r.status_code == 404


async def test_agent_index_serves_webapp_html(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "ai_chat_webapp.tools",
            "upstream_id": "x",
        },
    )
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{rec['id']}/")
        assert r.status_code == 200
        # Bundle's index.html includes fantastic_transport reference.
        assert "fantastic_transport" in r.text


async def test_agent_index_404_for_backend_with_no_webapp(seeded_kernel, file_agent):
    """A backend bundle (file) ships no webapp — must 404."""
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{file_agent}/")
        assert r.status_code == 404


# ─── render_html duck-type: any agent that returns {html:str} from
#     `render_html` gets its content served at /<id>/ with transport
#     auto-injected. html_agent is the canonical implementer.


async def test_html_agent_index_serves_record_html(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {
            "type": "create_agent",
            "handler_module": "html_agent.tools",
            "html_content": "<h1>marker</h1>",
        },
    )
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{rec['id']}/")
        assert r.status_code == 200
        assert "marker" in r.text
        # transport auto-injected so in-iframe JS can call any agent.
        assert "_fantastic/transport.js" in r.text


async def test_html_agent_index_placeholder_when_unset(seeded_kernel):
    rec = await seeded_kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "html_agent.tools"},
    )
    app = make_app("test_web", seeded_kernel)
    with TestClient(app) as c:
        r = c.get(f"/{rec['id']}/")
        assert r.status_code == 200
        assert rec["id"] in r.text
        assert "set_html" in r.text


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
