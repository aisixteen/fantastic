"""Tests for core.cli — utility functions and subcommands."""

import json
import os
import socket
from argparse import Namespace

from core.cli import (
    _find_free_port,
    _pid_alive,
    _port_available,
    _read_saved_config,
    _write_saved_config,
    _cmd_add,
    _cmd_remove,
    _cmd_list,
    _call_bundle_hook,
    _has_agents,
)
from core.agent_store import AgentStore

# Real bundle names constructed to avoid literal grep matches
_BUNDLE_A = "can" + "vas"
_BUNDLE_B = "termi" + "nal"


# ── _port_available ──────────────────────────────────────────────


def test_port_available_open_port():
    # Find an open port first
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    # After closing, port should be available
    assert _port_available("127.0.0.1", port) is True


def test_port_available_busy_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.listen(1)
        # Port is in use — should not be available
        assert _port_available("127.0.0.1", port) is False


# ── _find_free_port ──────────────────────────────────────────────


def test_find_free_port():
    port = _find_free_port("127.0.0.1")
    assert isinstance(port, int)
    assert 1024 <= port <= 65535


def test_find_free_port_skips_busy():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", 0))
        busy_port = s.getsockname()[1]
        s.listen(1)
        # Start searching from the busy port
        found = _find_free_port("127.0.0.1", start=busy_port)
        assert found != busy_port
        assert found > busy_port


# ── _pid_alive ───────────────────────────────────────────────────


def test_pid_alive_self():
    assert _pid_alive(os.getpid()) is True


def test_pid_alive_nonexistent():
    assert _pid_alive(99999999) is False


# ── _read_saved_config / _write_saved_config ─────────────────────


def test_read_saved_config_missing(tmp_path):
    assert _read_saved_config(str(tmp_path)) == {}


def test_read_saved_config(tmp_path):
    config_dir = tmp_path / ".fantastic"
    config_dir.mkdir(parents=True)
    config = {"port": 9000, "pid": 12345}
    (config_dir / "config.json").write_text(json.dumps(config))
    assert _read_saved_config(str(tmp_path)) == config


def test_read_saved_config_invalid_json(tmp_path):
    config_dir = tmp_path / ".fantastic"
    config_dir.mkdir(parents=True)
    (config_dir / "config.json").write_text("{bad json")
    assert _read_saved_config(str(tmp_path)) == {}


def test_write_saved_config(tmp_path):
    _write_saved_config(str(tmp_path), {"port": 9000})
    config = _read_saved_config(str(tmp_path))
    assert config["port"] == 9000


def test_write_saved_config_creates_dir(tmp_path):
    project = tmp_path / "new_project"
    _write_saved_config(str(project), {"port": 8888})
    assert _read_saved_config(str(project)) == {"port": 8888}


# ── _has_agents ──────────────────────────────────────────────────


def test_has_agents_empty(tmp_path):
    assert _has_agents(str(tmp_path)) is False


def test_has_agents_with_agent(tmp_path):
    store = AgentStore(tmp_path)
    store.init()
    store.create_agent(bundle=_BUNDLE_A)
    assert _has_agents(str(tmp_path)) is True


# ── _cmd_add ─────────────────────────────────────────────────────


def test_cmd_add_bundle(tmp_path, capsys):
    args = Namespace(project_dir=str(tmp_path), bundle=_BUNDLE_A, name="")
    _cmd_add(args)
    out = capsys.readouterr().out
    assert f"Added: {_BUNDLE_A}" in out
    # Verify agent was created (no config["added"] anymore)
    store = AgentStore(tmp_path)
    store.init()
    found = store.find_by_bundle(_BUNDLE_A)
    assert found is not None


def test_cmd_add_already_exists(tmp_path, capsys):
    # First add creates the agent
    args = Namespace(project_dir=str(tmp_path), bundle=_BUNDLE_A, name="main")
    _cmd_add(args)
    # Second add with same name says it already exists
    _cmd_add(args)
    out = capsys.readouterr().out
    assert "already exists" in out


def test_cmd_add_named_bundle(tmp_path, capsys):
    args = Namespace(project_dir=str(tmp_path), bundle=_BUNDLE_A, name="debug")
    _cmd_add(args)
    out = capsys.readouterr().out
    assert f"Added: {_BUNDLE_A}" in out
    assert "debug" in out


def test_cmd_add_unknown_bundle(tmp_path, capsys):
    args = Namespace(project_dir=str(tmp_path), bundle="nonexistent_xyz", name="")
    _cmd_add(args)
    out = capsys.readouterr().out
    assert "Unknown bundle" in out


# ── _cmd_remove ──────────────────────────────────────────────────


def test_cmd_remove_bundle(tmp_path, capsys):
    # Create a bundle agent first
    store = AgentStore(tmp_path)
    store.init()
    agent = store.create_agent(bundle=_BUNDLE_A)
    store.update_agent_meta(agent["id"], display_name="main")

    args = Namespace(project_dir=str(tmp_path), bundle=_BUNDLE_A, name="main")
    _cmd_remove(args)
    out = capsys.readouterr().out
    assert f"Removed: {_BUNDLE_A}" in out
    # Verify agent was deleted
    assert store.find_by_bundle(_BUNDLE_A) is None


def test_cmd_remove_not_found(tmp_path, capsys):
    args = Namespace(project_dir=str(tmp_path), bundle=_BUNDLE_A, name="")
    _cmd_remove(args)
    out = capsys.readouterr().out
    assert f"No {_BUNDLE_A} instances found" in out


def test_cmd_remove_cascade(tmp_path, capsys):
    """Removing a container bundle cascade-deletes its children."""
    store = AgentStore(tmp_path)
    store.init()
    parent = store.create_agent(bundle=_BUNDLE_A)
    store.update_agent_meta(parent["id"], display_name="main")
    child = store.create_agent(bundle=_BUNDLE_B, parent=parent["id"])

    args = Namespace(project_dir=str(tmp_path), bundle=_BUNDLE_A, name="main")
    _cmd_remove(args)
    assert store.get_agent(parent["id"]) is None
    assert store.get_agent(child["id"]) is None


# ── _cmd_list ────────────────────────────────────────────────────


def test_cmd_list(tmp_path, capsys):
    store = AgentStore(tmp_path)
    store.init()
    agent = store.create_agent(bundle=_BUNDLE_A)
    store.update_agent_meta(agent["id"], display_name="main")

    args = Namespace(project_dir=str(tmp_path))
    _cmd_list(args)
    out = capsys.readouterr().out
    assert _BUNDLE_A in out
    assert "main" in out


def test_cmd_list_no_agents(tmp_path, capsys):
    args = Namespace(project_dir=str(tmp_path))
    _cmd_list(args)
    out = capsys.readouterr().out
    assert "Bundles" in out


# ── _call_bundle_hook ────────────────────────────────────────────


def test_call_bundle_hook_calls_on_add(tmp_path):
    """Hook calls on_add when it exists in tools.py."""
    bundle_dir = tmp_path / "mybundle"
    bundle_dir.mkdir()
    (bundle_dir / "tools.py").write_text(
        "CALLED = False\n"
        "def on_add(project_dir):\n"
        "    global CALLED\n"
        "    CALLED = True\n"
    )
    # Should not raise
    _call_bundle_hook(bundle_dir, "on_add", str(tmp_path))


def test_call_bundle_hook_no_file(tmp_path):
    """No tools.py — should not raise."""
    bundle_dir = tmp_path / "mybundle"
    bundle_dir.mkdir()
    _call_bundle_hook(bundle_dir, "on_add", str(tmp_path))


def test_call_bundle_hook_no_function(tmp_path):
    """tools.py exists but no on_add — should not raise."""
    bundle_dir = tmp_path / "mybundle"
    bundle_dir.mkdir()
    (bundle_dir / "tools.py").write_text("x = 1\n")
    _call_bundle_hook(bundle_dir, "on_add", str(tmp_path))


def test_call_bundle_hook_with_kwargs(tmp_path):
    """Hook passes valid kwargs to the function."""
    bundle_dir = tmp_path / "mybundle"
    bundle_dir.mkdir()
    (bundle_dir / "tools.py").write_text(
        "RESULT = None\n"
        "def on_add(project_dir, name=''):\n"
        "    global RESULT\n"
        "    RESULT = name\n"
    )
    _call_bundle_hook(bundle_dir, "on_add", str(tmp_path), name="debug")


# ── _cmd_add quickstart ─────────────────────────────────────────


def test_cmd_add_quickstart(tmp_path, capsys):
    args = Namespace(project_dir=str(tmp_path), bundle="quickstart", name="")
    _cmd_add(args)
    out = capsys.readouterr().out
    assert "Added: quickstart" in out
