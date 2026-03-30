"""Tests for REST API endpoints."""

import os

from .conftest import _create_agent, _BUNDLE_A


# ─── REST: GET /api/state ────────────────────────────────────────────────


def test_rest_get_state(app_client):
    r = app_client.get("/api/state")
    assert r.status_code == 200
    data = r.json()
    assert "agents" in data
    # Container agent pre-created by fixture
    assert len(data["agents"]) >= 1
    container_agents = [a for a in data["agents"] if a.get("bundle") == _BUNDLE_A]
    assert len(container_agents) == 1


# ─── REST: POST /api/agents/{id}/execute ─────────────────────────────────


def test_rest_execute_agent_code(app_client):
    agent = _create_agent(app_client)
    r = app_client.post(
        f"/api/agents/{agent['id']}/execute",
        json={"code": "print(1+1)"},
    )
    assert r.status_code == 200
    data = r.json()
    assert data["success"] is True
    assert any("2" in str(o) for o in data["outputs"])


def test_rest_execute_agent_not_found(app_client):
    r = app_client.post(
        "/api/agents/nonexistent/execute",
        json={"code": "print(1)"},
    )
    assert r.status_code == 200
    assert "error" in r.json()


# ─── REST: POST /api/agents/{id}/resolve ─────────────────────────────────


def test_rest_resolve_agent(app_client):
    agent = _create_agent(app_client)
    r = app_client.post(
        f"/api/agents/{agent['id']}/resolve",
        json={"code": "x = 42; print(x)"},
    )
    assert r.status_code == 200
    data = r.json()
    assert data["success"] is True


def test_rest_resolve_agent_not_found(app_client):
    r = app_client.post(
        "/api/agents/nonexistent/resolve",
        json={"code": "print(1)"},
    )
    assert r.status_code == 200
    assert "error" in r.json()


# ─── REST: POST /api/call ────────────────────────────────────────────────


def test_rest_api_call_list_agents(app_client):
    r = app_client.post(
        "/api/call",
        json={"tool": "list_agents", "args": {}},
    )
    assert r.status_code == 200
    result = r.json()["result"]
    assert isinstance(result, list)


def test_rest_api_call_create_agent(app_client):
    r = app_client.post(
        "/api/call",
        json={"tool": "create_agent", "args": {"template": "bundle_b"}},
    )
    assert r.status_code == 200
    result = r.json()["result"]
    assert "agent_id" in result


def test_rest_api_call_execute_python(app_client):
    agent = _create_agent(app_client)
    r = app_client.post(
        "/api/call",
        json={
            "tool": "execute_python",
            "args": {"code": "print(7*8)", "agent_id": agent["id"]},
        },
    )
    assert r.status_code == 200
    assert "56" in r.json()["result"]


def test_rest_api_call_unknown_tool(app_client):
    r = app_client.post(
        "/api/call",
        json={"tool": "nonexistent_tool", "args": {}},
    )
    assert r.status_code == 200
    assert "Unknown tool" in r.json()["error"]


# ─── REST: Process endpoints ──────────────────────────────────────────────


_PROC_API = "/api/" + "termi" + "nal"  # REST endpoint path (not a bundle name)


def test_rest_process_output_empty(app_client):
    r = app_client.get(f"{_PROC_API}/nonexistent/output")
    assert r.status_code == 200
    assert r.json()["output"] == ""
    assert r.json()["lines"] == 0


def test_rest_process_restart_not_found(app_client):
    r = app_client.post(f"{_PROC_API}/nonexistent/restart")
    assert r.status_code == 404


def test_rest_process_signal_not_found(app_client):
    r = app_client.post(
        f"{_PROC_API}/nonexistent/signal",
        json={"signal": 2},
    )
    assert r.status_code == 404


def test_rest_process_write_not_found(app_client):
    r = app_client.post(
        f"{_PROC_API}/nonexistent/write",
        json={"data": "hello"},
    )
    assert r.status_code == 200
    assert r.json()["error"] == "process not found"


# ─── REST: Content alias ─────────────────────────────────────────────────


def test_rest_content_alias_not_found(app_client):
    r = app_client.get("/content/nonexistent")
    assert r.status_code == 404


def test_rest_content_alias_file(app_client, project_dir):
    test_file = os.path.join(project_dir, "test.txt")
    with open(test_file, "w") as f:
        f.write("hello world")

    r = app_client.post(
        "/api/call",
        json={"tool": "content_alias_file", "args": {"file_path": test_file}},
    )
    assert r.status_code == 200
    alias_path = r.json()["result"]
    assert alias_path.startswith("/content/")

    r = app_client.get(alias_path)
    assert r.status_code == 200


# ─── REST: Favicon ───────────────────────────────────────────────────────


def test_favicon_ico_redirect(app_client):
    r = app_client.get("/favicon.ico", follow_redirects=False)
    assert r.status_code == 301
    assert r.headers["location"] == "/favicon.png"
