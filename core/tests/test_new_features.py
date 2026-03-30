"""Tests for new features: broadcast mode, relative aliases, requirements flag."""

import os

import pytest
from starlette.testclient import TestClient


@pytest.fixture
def project_dir(tmp_path):
    return str(tmp_path)


@pytest.fixture
def app_client(project_dir, monkeypatch):
    monkeypatch.setenv("PROJECT_DIR", project_dir)
    import importlib
    import core.server as server_mod

    importlib.reload(server_mod)
    client = TestClient(server_mod.app)
    with client:
        yield client


# ─── Feature 3: Broadcast mode ──────────────────────────────────────────


def test_broadcast_status_default_disabled(app_client):
    r = app_client.get("/api/broadcast/status")
    assert r.status_code == 200
    data = r.json()
    assert data["enabled"] is False
    assert data["viewers"] == 0


def test_broadcast_start(app_client):
    r = app_client.post("/api/broadcast/start")
    assert r.status_code == 200
    data = r.json()
    assert "token" in data
    assert len(data["token"]) > 0
    assert "/ws/broadcast?token=" in data["url"]


def test_broadcast_start_then_status(app_client):
    app_client.post("/api/broadcast/start")
    r = app_client.get("/api/broadcast/status")
    assert r.json()["enabled"] is True


def test_broadcast_stop(app_client):
    app_client.post("/api/broadcast/start")
    r = app_client.post("/api/broadcast/stop")
    assert r.status_code == 200
    assert r.json()["ok"] is True

    status = app_client.get("/api/broadcast/status").json()
    assert status["enabled"] is False


def test_broadcast_ws_rejected_when_disabled(app_client):
    """Viewer WS should be rejected when broadcast is not enabled."""
    with pytest.raises(Exception):
        with app_client.websocket_connect("/ws/broadcast?token=fake"):
            pass


def test_broadcast_ws_rejected_with_bad_token(app_client):
    """Viewer WS should be rejected with wrong token."""
    app_client.post("/api/broadcast/start")
    with pytest.raises(Exception):
        with app_client.websocket_connect("/ws/broadcast?token=wrong"):
            pass


def test_broadcast_ws_accepted_with_valid_token(app_client):
    """Viewer WS should connect and receive state when token is valid."""
    start = app_client.post("/api/broadcast/start").json()
    token = start["token"]
    with app_client.websocket_connect(f"/ws/broadcast?token={token}") as ws:
        msg = ws.receive_json()
        assert msg["type"] == "state"
        assert "agents" in msg["state"]


# ─── Feature 4: Relative content aliases ─────────────────────────────────


def test_relative_content_alias(app_client, project_dir):
    """Content alias for a file within project dir should store relative path."""
    # Create a test file in project dir
    test_file = os.path.join(project_dir, "test_image.png")
    with open(test_file, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + b"\x00" * 100)

    r = app_client.post(
        "/api/call",
        json={
            "tool": "content_alias_file",
            "args": {"file_path": test_file},
        },
    )
    assert r.status_code == 200
    alias_path = r.json()["result"]
    assert alias_path.startswith("/content/")

    # Verify the alias works
    r2 = app_client.get(alias_path)
    assert r2.status_code == 200


# ─── CLI requirements flag ──────────────────────────────────────────────


def test_cli_requirements_env_var(monkeypatch, tmp_path):
    """--requirements flag should set REQUIREMENTS_FILE env var."""
    import sys
    from unittest.mock import patch

    req_file = tmp_path / "requirements.txt"
    req_file.write_text("numpy\npandas\n")

    monkeypatch.setattr(
        sys,
        "argv",
        [
            "fantastic",
            "--requirements",
            str(req_file),
            "--project-dir",
            str(tmp_path),
            "serve",
        ],
    )

    # Patch uvicorn.run to prevent actually starting the server
    with patch("uvicorn.run"):
        from core.cli import main

        main()

    assert os.environ.get("REQUIREMENTS_FILE") == str(req_file)


# ─── Handbook uses _paths ────────────────────────────────────────────────


def test_handbook_endpoint(app_client):
    """Handbook endpoint should work with _paths resolver."""
    r = app_client.get("/api/handbook")
    assert r.status_code == 200
    data = r.json()
    assert "handbook" in data
    # Should return CLAUDE.md content
    if data["handbook"]:
        assert "Fantastic" in data["handbook"]
