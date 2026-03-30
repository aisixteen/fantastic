"""Tests for list_templates and register_template."""

import json

from core.tools._registry import (
    _register_template,
    _list_templates,
)


# ─── list_templates ──────────────────────────────────────────────────


async def test_list_templates_returns_builtin(setup):
    """list_templates returns at least the built-in bundles."""
    tr = await _list_templates()
    assert isinstance(tr.data, list)
    assert len(tr.data) >= 1  # at least one built-in bundle


# ─── register_template ───────────────────────────────────────────────


async def test_register_template_missing_dir(setup):
    """Missing directory → error."""
    tr = await _register_template(path="nonexistent/plugin")
    assert "error" in tr.data


async def test_register_template_no_template_json(setup):
    """Directory exists but no template.json → error."""
    engine, _, _ = setup
    plugin_dir = engine.project_dir / "plugins" / "bad"
    plugin_dir.mkdir(parents=True)
    tr = await _register_template(path="plugins/bad")
    assert "error" in tr.data


async def test_register_template_valid(setup):
    """Valid template with tools → registered successfully."""
    engine, _, _ = setup
    plugin_dir = engine.project_dir / "plugins" / "my_widget"
    plugin_dir.mkdir(parents=True)
    (plugin_dir / "template.json").write_text(
        json.dumps(
            {
                "name": "my_widget",
                "bundle": "my_widget",
            }
        )
    )
    (plugin_dir / "tools.py").write_text(
        "def register_tools(engine, fire_broadcasts, process_runner=None):\n"
        "    async def widget_hello() -> str:\n"
        "        '''Say hello from widget.'''\n"
        "        return 'hello from widget'\n"
        "    return {'widget_hello': widget_hello}\n"
    )
    tr = await _register_template(path="plugins/my_widget")
    assert "error" not in tr.data
    assert tr.data["name"] == "my_widget"
    assert "widget_hello" in tr.data["tools"]
    # Broadcast sent
    assert any(b["type"] == "template_registered" for b in tr.broadcast)


async def test_register_template_without_tools(setup):
    """Template without tools.py → registered but no tools."""
    engine, _, _ = setup
    plugin_dir = engine.project_dir / "plugins" / "static_tmpl"
    plugin_dir.mkdir(parents=True)
    (plugin_dir / "template.json").write_text(
        json.dumps(
            {
                "name": "static_tmpl",
                "bundle": "static_tmpl",
            }
        )
    )
    tr = await _register_template(path="plugins/static_tmpl")
    assert "error" not in tr.data
    assert tr.data["tools"] == []
