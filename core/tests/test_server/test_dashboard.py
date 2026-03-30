"""Tests for dashboard server routes (route registered by dashboard bundle)."""

import importlib
from pathlib import Path

import pytest
from starlette.testclient import TestClient

from core.agent_store import AgentStore


def _pre_add_dashboard(project_dir, name="main"):
    """Simulate `fantastic add dashboard --name {name}`."""
    store = AgentStore(Path(project_dir))
    store.init()
    agent = store.create_agent(bundle="dashboard")
    store.update_agent_meta(agent["id"], display_name=name)
    return agent


@pytest.fixture
def project_dir(tmp_path):
    return str(tmp_path)


@pytest.fixture
def dashboard_client(project_dir, monkeypatch):
    """TestClient with a dashboard agent pre-created (bundle registers its own route)."""
    from core.tools import _state

    _state._on_agent_created.clear()

    _pre_add_dashboard(project_dir)
    monkeypatch.setenv("PROJECT_DIR", project_dir)

    import core.server as server_mod

    importlib.reload(server_mod)
    client = TestClient(server_mod.app)
    with client:
        yield client

    _state._on_agent_created.clear()
    _state._engine = None
    _state._broadcast = None
    _state._process_runner = None


# ─── Dashboard route (self-registered by bundle) ─────────────────────


def test_dashboard_route_serves_html(dashboard_client):
    r = dashboard_client.get("/dashboard/main")
    assert r.status_code == 200
    assert "text/html" in r.headers["content-type"]
    assert "Dashboard" in r.text


def test_dashboard_route_not_found(dashboard_client):
    r = dashboard_client.get("/dashboard/nonexistent")
    assert r.status_code == 404
