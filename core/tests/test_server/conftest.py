"""Shared fixtures for server tests."""

from pathlib import Path

import pytest
from starlette.testclient import TestClient


_BUNDLE_A = "can" + "vas"


def _pre_add_bundles(project_dir):
    """Simulate adding a container bundle — create agent with display_name."""
    from core.agent_store import AgentStore

    store = AgentStore(Path(project_dir))
    store.init()
    container = store.create_agent(bundle=_BUNDLE_A)
    store.update_agent_meta(container["id"], display_name="main", is_container=True)


@pytest.fixture
def project_dir(tmp_path):
    """Temp project directory."""
    return str(tmp_path)


@pytest.fixture
def app_client(project_dir, monkeypatch):
    """TestClient with lifespan — sets up engine, process_runner, tools."""
    # Clear module-level hook lists to avoid cross-test contamination
    from core.tools import _state

    _state._on_agent_created.clear()
    from core.server import _state as server_state

    server_state.clear_hooks()

    _pre_add_bundles(project_dir)
    monkeypatch.setenv("PROJECT_DIR", project_dir)
    # Reload server module so lifespan picks up the new PROJECT_DIR
    import importlib
    import core.server as server_mod

    importlib.reload(server_mod)
    client = TestClient(server_mod.app)
    with client:
        yield client

    # Cleanup shared state
    _state._on_agent_created.clear()
    _state._engine = None
    _state._broadcast = None
    _state._process_runner = None
    server_state.clear_hooks()


def _create_agent(app_client, template="bundle_b", **extra):
    """Helper: create an agent via WS and return the created agent dict."""
    with app_client.websocket_connect("/ws") as ws:
        msg = {
            "type": "create_agent",
            "template": template,
            **extra,
        }
        ws.send_json(msg)
        # Collect messages until we see agent_created
        for _ in range(10):
            resp = ws.receive_json()
            if resp["type"] == "agent_created":
                return resp["agent"]
        raise RuntimeError("Did not receive agent_created message")
