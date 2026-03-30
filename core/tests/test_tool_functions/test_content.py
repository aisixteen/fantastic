"""Tests for post_output, content aliases, get_aliases."""

from core.tools._agents import _create_agent, _post_output
from core.tools._content import (
    _content_alias_file,
    _content_alias_url,
    _get_aliases,
)
from core.tools import (
    content_alias_file,
    content_alias_url,
    get_aliases,
    _TOOL_DISPATCH,
)


# ─── post_output ─────────────────────────────────────────────────────────


async def test_post_output_tool(setup):
    engine, bc, _ = setup
    post_output = _TOOL_DISPATCH["post_output"]
    tr = await _create_agent()
    agent_id = tr.data["id"]
    result = await post_output(agent_id, "<h1>Hello</h1>")
    assert "posted" in result.lower()
    agent = engine.get_agent(agent_id)
    assert agent["output_html"] == "<h1>Hello</h1>"


async def test_post_output_not_found(setup):
    post_output = _TOOL_DISPATCH["post_output"]
    result = await post_output("nonexistent", "<p>x</p>")
    assert "[ERROR]" in result


# ─── content aliases ─────────────────────────────────────────────────────


async def test_content_alias_file_tool(setup):
    engine, _, _ = setup
    result = await content_alias_file("/tmp/test.png")
    assert result.startswith("/content/")
    alias_id = result.split("/")[-1]
    assert alias_id in engine.content_aliases
    assert engine.content_aliases[alias_id]["type"] == "file"
    assert engine.content_aliases[alias_id]["persistent"] is True


async def test_content_alias_url_tool(setup):
    engine, _, _ = setup
    result = await content_alias_url("https://example.com/lib.js")
    assert result.startswith("/content/")
    alias_id = result.split("/")[-1]
    assert engine.content_aliases[alias_id]["type"] == "url"
    assert engine.content_aliases[alias_id]["url"] == "https://example.com/lib.js"
    assert engine.content_aliases[alias_id]["persistent"] is True


async def test_content_alias_file_persistent(setup):
    engine, _, _ = setup
    result = await content_alias_file("/tmp/test.png", persistent=True)
    alias_id = result.split("/")[-1]
    assert engine.content_aliases[alias_id]["persistent"] is True


async def test_content_alias_url_persistent(setup):
    engine, _, _ = setup
    result = await content_alias_url("https://example.com/lib.js", persistent=True)
    alias_id = result.split("/")[-1]
    assert engine.content_aliases[alias_id]["persistent"] is True


async def test_default_alias_survives_reload(setup):
    """Default aliases (persistent=True) survive reload from disk."""
    engine, _, _ = setup
    await content_alias_file("/tmp/persist.png")  # default persistent=True
    await content_alias_file("/tmp/ephemeral.png", persistent=False)
    assert len(engine.content_aliases) == 2
    # Simulate restart: clear cache, reload from disk
    engine._content_aliases = None
    aliases = engine.content_aliases
    assert len(aliases) == 1
    remaining = list(aliases.values())[0]
    assert remaining["path"] == "/tmp/persist.png"


async def test_get_aliases_empty(setup):
    result = await get_aliases()
    assert result == []


async def test_get_aliases_returns_all(setup):
    engine, _, _ = setup
    await content_alias_file("/tmp/a.png", persistent=True)
    await content_alias_url("https://example.com/b.js")
    result = await get_aliases()
    assert len(result) == 2
    types = {a["type"] for a in result}
    assert types == {"file", "url"}
    for a in result:
        assert "alias_id" in a
        assert "alias_path" in a
        assert "persistent" in a
        if a["type"] == "file":
            assert "path" in a
        elif a["type"] == "url":
            assert "url" in a


async def test_get_aliases_persistent_flag(setup):
    await content_alias_file("/tmp/a.png", persistent=True)
    await content_alias_url("https://example.com/b.js", persistent=False)
    result = await get_aliases()
    persistent_flags = {a["alias_id"]: a["persistent"] for a in result}
    assert True in persistent_flags.values()
    assert False in persistent_flags.values()


