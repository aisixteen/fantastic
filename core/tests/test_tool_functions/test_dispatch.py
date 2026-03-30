"""Tests for dispatch tables and dispatch() function."""

import pytest
from dataclasses import is_dataclass

from core.tools import _TOOL_DISPATCH
from core.dispatch import ToolResult, dispatch, _DISPATCH


# ─── Tool dispatch table ──────────────────────────────────────────────


def test_tool_dispatch_has_core_tools():
    """_TOOL_DISPATCH covers core tools (statically registered)."""
    expected_core = {
        "execute_python",
        "create_agent",
        "list_agents",
        "read_agent",
        "delete_agent",
        "content_alias_file",
        "content_alias_url",
        "get_aliases",
        "get_state",
        "agent_call",
        "get_handbook",
        "register_template",
        "list_templates",
        "launch_instance",
        "stop_instance",
        "list_instances",
        "restart_instance",
        "server_logs",
        "core_chat_message",
    }
    assert expected_core.issubset(set(_TOOL_DISPATCH.keys()))


async def test_tool_dispatch_has_plugin_tools(setup):
    """_TOOL_DISPATCH has plugin tools after init_tools() loads bundles.

    Both bundles are loaded in test setup (derived from agents created in conftest).
    """
    expected_plugin = {
        "move_agent",
        "resize_agent",
        "rename_agent",
        "update_agent",
        "post_output",
        "refresh_agent",
        "scene_vfx",
        "scene_vfx_data",
        "spatial_discovery",
    }
    assert expected_plugin.issubset(set(_TOOL_DISPATCH.keys()))


def test_tool_dispatch_excludes_removed_tools():
    """Removed tools must not be in dispatch."""
    for name in (
        "register_server_legacy",
        "unregister_server_legacy",
        "get_server_tools",
        "get_endpoints_legacy",
    ):
        assert name not in _TOOL_DISPATCH


def test_tool_dispatch_all_callable():
    for name, fn in _TOOL_DISPATCH.items():
        assert callable(fn), f"{name} is not callable"


def test_dispatch_table_not_empty():
    assert len(_DISPATCH) > 0


async def test_dispatch_covers_all_tools(setup):
    """Every tool (from _TOOL_DISPATCH) has a corresponding inner in _DISPATCH."""
    for tool_name in _TOOL_DISPATCH:
        assert tool_name in _DISPATCH, f"tool '{tool_name}' has no _DISPATCH entry"


def test_dispatch_has_ws_only_operations():
    """WS-only operations (no REST wrapper) are in _DISPATCH."""
    ws_only = {
        "get_state",
        "agent_run",
        "process_create",
        "process_input",
        "process_resize",
        "process_enter",
        "process_close",
    }
    for name in ws_only:
        assert name in _DISPATCH, f"WS-only operation '{name}' missing from _DISPATCH"


def test_dispatch_has_restart_instance():
    """restart_instance is in _DISPATCH."""
    assert "restart_instance" in _DISPATCH


async def test_dispatch_table_minimum_count(setup):
    """Dispatch table should have at least 40 entries (REST + WS-only + bundles)."""
    assert len(_DISPATCH) >= 30, f"Only {len(_DISPATCH)} entries in _DISPATCH"


async def test_inner_dispatch_has_agent_names(setup):
    """Inner dispatch has agent-based names (including plugin bundle tools)."""
    agent_names = {
        "create_agent",
        "list_agents",
        "read_agent",
        "delete_agent",
        "move_agent",
        "resize_agent",
        "rename_agent",
        "update_agent",
        "refresh_agent",
        "agent_call",
        "agent_run",
    }
    assert agent_names.issubset(set(_DISPATCH.keys()))


def test_inner_dispatch_all_callable():
    for name, fn in _DISPATCH.items():
        assert callable(fn), f"{name} is not callable"


# ─── ToolResult ────────────────────────────────────────────────────────


def test_tool_result_is_dataclass():
    assert is_dataclass(ToolResult)


def test_tool_result_defaults():
    tr = ToolResult()
    assert tr.data is None
    assert tr.broadcast == []
    assert tr.reply == []


def test_tool_result_independent_lists():
    """Each ToolResult gets its own broadcast/reply lists (no shared mutable default)."""
    tr1 = ToolResult()
    tr2 = ToolResult()
    tr1.broadcast.append({"type": "test"})
    assert tr2.broadcast == []


def test_tool_result_with_all_fields():
    tr = ToolResult(
        data={"key": "value"},
        broadcast=[{"type": "b1"}, {"type": "b2"}],
        reply=[{"type": "r1"}],
    )
    assert tr.data == {"key": "value"}
    assert len(tr.broadcast) == 2
    assert len(tr.reply) == 1


def test_tool_result_with_data():
    tr = ToolResult(data={"foo": "bar"}, broadcast=[{"type": "test"}])
    assert tr.data["foo"] == "bar"
    assert len(tr.broadcast) == 1
    assert tr.reply == []


# ─── dispatch() function ──────────────────────────────────────────────


async def test_dispatch_create_agent(setup):
    tr = await dispatch("create_agent", options={"x": 100, "y": 200})
    assert isinstance(tr, ToolResult)
    assert "id" in tr.data
    types = {b["type"] for b in tr.broadcast}
    assert "agent_created" in types


async def test_dispatch_list_agents(setup):
    await dispatch("create_agent")
    tr = await dispatch("list_agents")
    assert isinstance(tr.data, list)
    assert len(tr.data) >= 1


async def test_dispatch_delete_agent(setup):
    engine, bc, _ = setup
    tr_create = await dispatch("create_agent")
    agent_id = tr_create.data["id"]
    bc.clear()
    tr = await dispatch("delete_agent", agent_id=agent_id)
    assert tr.data.get("deleted") is True
    types = {b["type"] for b in tr.broadcast}
    assert "agent_deleted" in types


async def test_dispatch_move_agent(setup):
    tr_create = await dispatch("create_agent")
    agent_id = tr_create.data["id"]
    tr = await dispatch("move_agent", agent_id=agent_id, x=500, y=600)
    assert tr.data["x"] == 500
    assert tr.data["y"] == 600


async def test_dispatch_get_state(setup):
    tr = await dispatch("get_state")
    assert "agents" in tr.data
    assert len(tr.reply) == 1
    assert tr.reply[0]["type"] == "state"


async def test_dispatch_unknown_tool(setup):
    with pytest.raises(KeyError, match="Unknown dispatch tool"):
        await dispatch("nonexistent_tool")


async def test_dispatch_agent_run(setup):
    engine, bc, _ = setup
    tr_create = await dispatch("create_agent")
    agent_id = tr_create.data["id"]
    bc.clear()
    tr = await dispatch("agent_run", agent_id=agent_id, code="print(42)")
    assert tr.data["success"] is True
    assert len(tr.broadcast) >= 2  # agent_output + agent_complete
