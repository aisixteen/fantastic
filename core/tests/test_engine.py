"""Tests for the core engine — agent-based orchestration with subprocess execution."""

import pytest

from core.engine import Engine


@pytest.fixture
async def engine(tmp_path):
    """Engine with .fantastic/ in a temp directory."""
    e = Engine(project_dir=str(tmp_path))
    await e.start()
    yield e
    await e.stop()


@pytest.mark.asyncio
async def test_create_agent(engine):
    agent = engine.create_agent(agent_id="a1")
    assert agent["id"] == "a1"


@pytest.mark.asyncio
async def test_create_agent_bundles(engine):
    agent_b = engine.create_agent(agent_id="a1", bundle="bundle_b")
    html = engine.create_agent(agent_id="a2")

    assert agent_b.get("bundle") == "bundle_b"
    assert html.get("bundle") is None  # no bundle for plain agents


@pytest.mark.asyncio
async def test_execute_code(engine):
    engine.create_agent(agent_id="a1")
    result = await engine.execute_code("a1", "print('hello from engine')")
    assert result["success"] is True
    assert any("hello from engine" in o.get("text", "") for o in result["outputs"])


@pytest.mark.asyncio
async def test_resolve_agent(engine):
    engine.create_agent(agent_id="a1")
    result = await engine.resolve_agent("a1", "print('hi')")
    assert result["success"] is True
    assert result["agent_id"] == "a1"
    assert result["code"] == "print('hi')"


@pytest.mark.asyncio
async def test_execute_code_nonexistent_agent(engine):
    with pytest.raises(ValueError, match="not found"):
        await engine.execute_code("nonexistent", "print('x')")


@pytest.mark.asyncio
async def test_delete_agent(engine):
    engine.create_agent(agent_id="a1")
    assert engine.delete_agent("a1") is True
    assert engine.get_agent("a1") is None


@pytest.mark.asyncio
async def test_get_state(engine):
    engine.create_agent(agent_id="a1")
    engine.create_agent(agent_id="a2")
    state = engine.get_state()
    assert "agents" in state
    assert len(state["agents"]) == 2


@pytest.mark.asyncio
async def test_update_agent_meta(engine):
    engine.create_agent(agent_id="a1")
    assert engine.update_agent_meta("a1", display_name="Hello") is True
    agent = engine.get_agent("a1")
    assert agent["display_name"] == "Hello"


@pytest.mark.asyncio
async def test_content_aliases(engine):
    engine.add_content_alias(
        "test1", {"type": "url", "url": "http://example.com", "persistent": True}
    )
    assert "test1" in engine.content_aliases
    assert engine.content_aliases["test1"]["url"] == "http://example.com"
    engine.remove_content_alias("test1")
    assert "test1" not in engine.content_aliases


@pytest.mark.asyncio
async def test_server_registry(engine):
    entry = engine.register_server("a1", "http://localhost:8000", name="test")
    assert entry["agent_id"] == "a1"
    assert entry["url"] == "http://localhost:8000"
    servers = engine.list_servers()
    assert len(servers) == 1
    assert engine.unregister_server("a1") is True
    assert len(engine.list_servers()) == 0


@pytest.mark.asyncio
async def test_html_agent_with_url(engine):
    agent = engine.create_agent(agent_id="h1", url="https://example.com")
    assert agent["url"] == "https://example.com"
    assert "iframe" in agent["html_content"]


# ─── resolve_working_dir ──────────────────────────────────────────


@pytest.mark.asyncio
async def test_resolve_working_dir_default(engine):
    engine.create_agent(agent_id="a1")
    wd = engine.resolve_working_dir("a1")
    assert wd == engine.project_dir


@pytest.mark.asyncio
async def test_resolve_working_dir_absolute(engine, tmp_path):
    engine.create_agent(agent_id="c1", bundle="bundle_a")
    target = tmp_path / "other"
    target.mkdir()
    engine.update_agent_meta("c1", working_dir=str(target))
    wd = engine.resolve_working_dir("c1")
    assert wd == target


@pytest.mark.asyncio
async def test_resolve_working_dir_relative(engine, tmp_path):
    engine.create_agent(agent_id="c1", bundle="bundle_a")
    (tmp_path / "notebooks").mkdir()
    engine.update_agent_meta("c1", working_dir="notebooks")
    wd = engine.resolve_working_dir("c1")
    assert wd == tmp_path / "notebooks"


@pytest.mark.asyncio
async def test_resolve_working_dir_child_inherits(engine, tmp_path):
    engine.create_agent(agent_id="c1", bundle="bundle_a")
    (tmp_path / "notebooks").mkdir()
    engine.update_agent_meta("c1", working_dir="notebooks")
    engine.create_agent(agent_id="t1", parent="c1")
    wd = engine.resolve_working_dir("t1")
    assert wd == tmp_path / "notebooks"


@pytest.mark.asyncio
async def test_file_operations(engine, tmp_path):
    (tmp_path / "test.txt").write_text("hello")
    result = engine.read_file("test.txt")
    assert result["kind"] == "text"
    assert result["content"] == "hello"

    files = engine.list_files()
    names = [f["name"] for f in files]
    assert "test.txt" in names
