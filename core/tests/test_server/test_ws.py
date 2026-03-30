"""Tests for WebSocket handlers."""

import os

from .conftest import _create_agent, _BUNDLE_A


def _collect_until(ws, target_types, max_msgs=10):
    """Collect WS messages until all target_types are seen. Return all collected."""
    msgs = []
    types_seen = set()
    for _ in range(max_msgs):
        msg = ws.receive_json()
        msgs.append(msg)
        types_seen.add(msg["type"])
        if target_types.issubset(types_seen):
            break
    return msgs, types_seen


def _get_created_agent(ws):
    """Send create_agent and return the agent dict from the broadcast."""
    ws.send_json({"type": "create_agent", "template": "bundle_b"})
    msgs, types = _collect_until(ws, {"agent_created"}, max_msgs=4)
    for m in msgs:
        if m["type"] == "agent_created":
            return m["agent"]
    raise RuntimeError("No agent_created broadcast received")


# ─── WebSocket: get_state ────────────────────────────────────────────────


def test_ws_get_state(app_client):
    with app_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "get_state"})
        msg = ws.receive_json()
        assert msg["type"] == "state"
        assert "state" in msg
        assert "agents" in msg["state"]
        # Container agent pre-created by fixture
        container_agents = [
            a for a in msg["state"]["agents"] if a.get("bundle") == _BUNDLE_A
        ]
        assert len(container_agents) == 1


# ─── WebSocket: agent CRUD ──────────────────────────────────────────────


def test_ws_create_agent(app_client):
    """Creating via create_agent should broadcast agent_created."""
    with app_client.websocket_connect("/ws") as ws:
        ws.send_json(
            {
                "type": "create_agent",
                "template": "bundle_b",
                "options": {"x": 100, "y": 200},
            }
        )
        msgs, types = _collect_until(ws, {"agent_created"}, max_msgs=4)
        assert "agent_created" in types
        for m in msgs:
            if m["type"] == "agent_created":
                assert m["agent"].get("bundle") == "bundle_b"
                assert m["agent"]["x"] == 100
                assert m["agent"]["y"] == 200
                break


def test_ws_move_agent(app_client):
    with app_client.websocket_connect("/ws") as ws:
        agent = _get_created_agent(ws)
        agent_id = agent["id"]

        ws.send_json({"type": "move_agent", "agent_id": agent_id, "x": 500, "y": 600})
        msgs, types = _collect_until(ws, {"agent_moved"}, max_msgs=4)
        for m in msgs:
            if m["type"] == "agent_moved":
                assert m["x"] == 500
                assert m["y"] == 600
                break


def test_ws_resize_agent(app_client):
    with app_client.websocket_connect("/ws") as ws:
        agent = _get_created_agent(ws)
        agent_id = agent["id"]

        ws.send_json(
            {"type": "resize_agent", "agent_id": agent_id, "width": 900, "height": 500}
        )
        msgs, types = _collect_until(ws, {"agent_resized"}, max_msgs=4)
        for m in msgs:
            if m["type"] == "agent_resized":
                assert m["width"] == 900
                assert m["height"] == 500
                break


def test_ws_delete_agent(app_client):
    with app_client.websocket_connect("/ws") as ws:
        agent = _get_created_agent(ws)
        agent_id = agent["id"]

        ws.send_json({"type": "delete_agent", "agent_id": agent_id})
        msgs, types = _collect_until(ws, {"agent_deleted"}, max_msgs=4)
        found = False
        for m in msgs:
            if m["type"] == "agent_deleted":
                assert m["agent_id"] == agent_id
                found = True
                break
        assert found


def test_ws_delete_nonexistent_fails(app_client):
    with app_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "delete_agent", "agent_id": "nonexistent"})
        msg = ws.receive_json()
        assert msg["type"] == "error"
        assert (
            "not found" in msg["error"].lower()
            or "cannot delete" in msg["error"].lower()
        )


# ─── WebSocket: code execution ───────────────────────────────────────────


def test_ws_agent_run(app_client):
    agent = _create_agent(app_client)
    with app_client.websocket_connect("/ws") as ws:
        ws.send_json(
            {
                "type": "agent_run",
                "agent_id": agent["id"],
                "code": "print('hello ws')",
            }
        )
        types_seen = set()
        for _ in range(10):
            msg = ws.receive_json()
            types_seen.add(msg["type"])
            has_output = "agent_output" in types_seen
            has_complete = "agent_complete" in types_seen
            if has_output and has_complete:
                break
        assert "agent_output" in types_seen
        assert "agent_complete" in types_seen


# ─── WebSocket: unknown message ──────────────────────────────────────────


def test_ws_unknown_message_type(app_client):
    with app_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "bogus_message"})
        msg = ws.receive_json()
        assert msg["type"] == "error"
        assert "Unknown message type" in msg["error"]


# ─── WebSocket: process operations ───────────────────────────────────────


def test_ws_process_create_and_close(app_client):
    with app_client.websocket_connect("/ws") as ws:
        agent = _get_created_agent(ws)
        tid = agent["id"]

        ws.send_json(
            {
                "type": "process_create",
                "agent_id": tid,
                "cols": 80,
                "rows": 24,
                "command": "/bin/sh",
                "args": ["-c", "sleep 10"],
                "welcome_command": None,
            }
        )
        msg = ws.receive_json()
        assert msg["type"] == "process_created"
        assert msg["agent_id"] == tid

        ws.send_json({"type": "process_close", "agent_id": tid})
        msg = ws.receive_json()
        assert msg["type"] == "process_closed"
        assert msg["agent_id"] == tid


def test_ws_process_restart_not_found(app_client):
    with app_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "process_restart", "agent_id": "nonexistent"})
        msg = ws.receive_json()
        assert msg["type"] == "error"


# ─── WebSocket: multiple clients see broadcasts ──────────────────────────


def test_ws_broadcast_to_multiple_clients(app_client):
    with app_client.websocket_connect("/ws") as ws1:
        with app_client.websocket_connect("/ws") as ws2:
            ws1.send_json(
                {
                    "type": "create_agent",
                    "template": "bundle_b",
                }
            )
            msg1 = ws1.receive_json()
            msg2 = ws2.receive_json()
            assert msg1["type"] == "agent_created"
            assert msg2["type"] == "agent_created"
            assert msg1["agent"]["id"] == msg2["agent"]["id"]


# ─── WebSocket: state includes scene_vfx ────────────────────────────────────


def test_ws_state_includes_scene_vfx(app_client, project_dir):
    """get_state includes scene_vfx_js when VFX file exists."""
    # Find the container agent dir (created by fixture)
    import json

    agents_dir = os.path.join(project_dir, ".fantastic", "agents")
    container_dir = None
    for entry in os.listdir(agents_dir):
        agent_json = os.path.join(agents_dir, entry, "agent.json")
        if os.path.exists(agent_json):
            data = json.loads(open(agent_json).read())
            if data.get("bundle") == _BUNDLE_A:
                container_dir = os.path.join(agents_dir, entry)
                break
    assert container_dir is not None, "Container agent not found"

    with open(os.path.join(container_dir, "scene_vfx.js"), "w") as f:
        f.write("console.log('vfx')")

    with app_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "get_state"})
        msg = ws.receive_json()
        assert msg["type"] == "state"
        assert msg["state"]["scene_vfx_js"] == "console.log('vfx')"
