"""Tests for agent CRUD tool inner functions."""

from core.tools._agents import (
    _create_agent,
    _list_agents,
    _read_agent,
    _delete_agent,
    _rename_agent,
    _update_agent,
    _post_output,
    _refresh_agent,
    _get_full_state,
    _execute_python,
)
from core.dispatch import ToolResult


async def test_create_agent(setup):
    tr = await _create_agent(options={"x": 100, "y": 200})
    assert isinstance(tr, ToolResult)
    assert "id" in tr.data
    assert tr.data["x"] == 100
    assert tr.data["y"] == 200
    types = {b["type"] for b in tr.broadcast}
    assert "agent_created" in types


async def test_create_agent_with_id(setup):
    tr = await _create_agent(agent_id="test1")
    assert tr.data["id"] == "test1"


async def test_create_agent_with_template(setup):
    tr = await _create_agent(template="bundle_b")
    assert tr.data.get("bundle") == "bundle_b"


async def test_create_agent_with_dimensions(setup):
    tr = await _create_agent(options={"width": 400, "height": 300})
    assert tr.data["width"] == 400
    assert tr.data["height"] == 300


async def test_list_agents(setup):
    await _create_agent()
    await _create_agent()
    tr = await _list_agents()
    assert isinstance(tr.data, list)
    assert len(tr.data) >= 2
    entry = tr.data[0]
    assert "agent_id" in entry


async def test_read_agent(setup):
    await _create_agent(agent_id="r1")
    tr = await _read_agent("r1")
    assert tr.data["agent_id"] == "r1"


async def test_read_agent_not_found(setup):
    tr = await _read_agent("nonexistent")
    assert "error" in tr.data


async def test_delete_agent(setup):
    engine, bc, _ = setup
    await _create_agent(agent_id="d1")
    bc.clear()
    tr = await _delete_agent("d1")
    assert tr.data["deleted"] is True
    types = {b["type"] for b in tr.broadcast}
    assert "agent_deleted" in types


async def test_delete_agent_locked(setup):
    engine, _, _ = setup
    await _create_agent(agent_id="locked1")
    engine.update_agent_meta("locked1", delete_lock=True)
    tr = await _delete_agent("locked1")
    assert "error" in tr.data
    assert "delete-locked" in tr.data["error"]


async def test_delete_agent_not_found(setup):
    tr = await _delete_agent("nonexistent")
    assert "error" in tr.data


async def test_rename_agent(setup):
    await _create_agent(agent_id="n1")
    tr = await _rename_agent("n1", "New Name")
    assert tr.data["display_name"] == "New Name"
    types = {b["type"] for b in tr.broadcast}
    assert "agent_updated" in types


async def test_update_agent(setup):
    await _create_agent(agent_id="u1")
    tr = await _update_agent("u1", options={"display_name": "Updated"})
    assert tr.data["display_name"] == "Updated"


async def test_update_agent_no_options(setup):
    tr = await _update_agent("u1")
    assert "error" in tr.data


async def test_post_output(setup):
    await _create_agent(agent_id="p1")
    tr = await _post_output("p1", "<p>hello</p>")
    assert tr.data["posted"] is True


async def test_post_output_not_found(setup):
    tr = await _post_output("nonexistent", "<p>x</p>")
    assert "error" in tr.data


async def test_refresh_agent(setup):
    await _create_agent(agent_id="f1")
    tr = await _refresh_agent("f1")
    assert tr.data["refreshed"] is True
    assert tr.data["action"] == "agent_refresh"


async def test_refresh_agent_not_found(setup):
    tr = await _refresh_agent("nonexistent")
    assert "error" in tr.data


async def test_get_full_state(setup):
    await _create_agent()
    tr = await _get_full_state()
    assert "agents" in tr.data


async def test_execute_python(setup):
    await _create_agent(agent_id="e1")
    tr = await _execute_python("print('hi')", agent_id="e1")
    assert "text" in tr.data
    assert "hi" in tr.data["text"]


async def test_execute_python_no_agent_id(setup):
    tr = await _execute_python("print('hi')")
    assert "error" in tr.data


async def test_execute_python_error(setup):
    await _create_agent(agent_id="e2")
    tr = await _execute_python("raise Exception('boom')", agent_id="e2")
    assert "error" in tr.data
