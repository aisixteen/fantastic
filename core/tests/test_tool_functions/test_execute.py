"""Tests for execute_python, get_state, and get_handbook."""

from core.tools._agents import _create_agent
from core.tools import (
    execute_python,
    get_state,
    get_handbook,
)


# ─── execute_python ──────────────────────────────────────────────────────


async def test_execute_python_tool(setup):
    engine, _, _ = setup
    tr = await _create_agent()
    result = await execute_python("print(1 + 2)", agent_id=tr.data["id"])
    assert "3" in result


async def test_execute_python_requires_agent_id(setup):
    result = await execute_python("print(1 + 2)")
    assert "[ERROR]" in result
    assert "agent_id is required" in result


async def test_execute_python_on_agent(setup):
    engine, _, _ = setup
    tr = await _create_agent()
    result = await execute_python("x = 42; print(x)", agent_id=tr.data["id"])
    assert "42" in result


async def test_execute_python_error(setup):
    engine, _, _ = setup
    tr = await _create_agent()
    result = await execute_python("raise ValueError('boom')", agent_id=tr.data["id"])
    assert "[ERROR]" in result
    assert "boom" in result


# ─── get_state ────────────────────────────────────────────────────


async def test_get_state_tool(setup):
    engine, _, _ = setup
    await _create_agent()
    result = await get_state()
    assert "agents" in result
    assert len(result["agents"]) >= 1


# ─── get_handbook ────────────────────────────────────────────────────────


async def test_get_handbook_from_project_dir(setup):
    engine, _, _ = setup
    # Create CLAUDE.md in project dir
    (engine.project_dir / "CLAUDE.md").write_text("# Test Canvas")
    skills_dir = engine.project_dir / "skills"
    skills_dir.mkdir()
    (skills_dir / "test-skill.md").write_text("# Test Skill\nSome content.")

    # Overview returns just CLAUDE.md
    result = await get_handbook()
    assert "# Test Canvas" in result

    # Specific skill returns skill doc
    result2 = await get_handbook(skill="test-skill")
    assert "# Test Skill" in result2


async def test_get_handbook_fallback_to_package(setup):
    """When project dir has no CLAUDE.md, falls back to package root."""
    result = await get_handbook()
    # Package root has CLAUDE.md from the repo
    assert "Fantastic" in result or "[ERROR]" not in result
