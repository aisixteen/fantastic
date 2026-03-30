"""Tests for the plugin loader — load_bundle_tools & load_project_plugins."""

from core.tools._plugin_loader import (
    load_bundle_tools,
    load_project_plugins,
    load_single_bundle,
    BundleLoadResult,
)


# ── BundleLoadResult ────────────────────────────────────────────────


def test_bundle_load_result_defaults():
    r = BundleLoadResult()
    assert r.tools == {}


# ── load_bundle_tools ────────────────────────────────────────────────


async def test_bundles_dir_missing(tmp_path):
    """Non-existent bundles dir → empty result."""
    result = load_bundle_tools(tmp_path / "no_such_dir", None, None)
    assert result.tools == {}


async def test_subdir_without_tools(tmp_path):
    """Subdir exists but has no tools.py → skipped."""
    (tmp_path / "empty_bundle").mkdir()
    result = load_bundle_tools(tmp_path, None, None)
    assert result.tools == {}


async def test_plain_file_skipped(tmp_path):
    """Plain file (not subdir) inside bundles dir → skipped."""
    (tmp_path / "readme.txt").write_text("hi")
    result = load_bundle_tools(tmp_path, None, None)
    assert result.tools == {}


async def test_valid_plugin_loaded(tmp_path):
    """Valid plugin with register_tools → tools loaded."""
    bundle = tmp_path / "my_bundle"
    bundle.mkdir()
    (bundle / "tools.py").write_text(
        "def register_tools(engine, fire_broadcasts, process_runner=None):\n"
        "    return {'my_tool': lambda: 'ok'}\n"
    )
    result = load_bundle_tools(tmp_path, None, None)
    assert "my_tool" in result.tools
    assert callable(result.tools["my_tool"])


async def test_syntax_error_logged_not_fatal(tmp_path, caplog):
    """Plugin with syntax error → logged, other plugins still load."""
    bad = tmp_path / "bad_bundle"
    bad.mkdir()
    (bad / "tools.py").write_text("def register_tools(:\n")  # SyntaxError

    good = tmp_path / "good_bundle"
    good.mkdir()
    (good / "tools.py").write_text(
        "def register_tools(engine, fire_broadcasts, process_runner=None):\n"
        "    return {'good_tool': lambda: 'ok'}\n"
    )
    result = load_bundle_tools(tmp_path, None, None)
    # good plugin loaded despite bad one
    assert "good_tool" in result.tools
    assert "Failed to load tools from" in caplog.text


async def test_no_register_tools_skipped(tmp_path):
    """Module exists but has no register_tools function → skipped."""
    bundle = tmp_path / "no_func"
    bundle.mkdir()
    (bundle / "tools.py").write_text("HELLO = 42\n")
    result = load_bundle_tools(tmp_path, None, None)
    assert result.tools == {}


async def test_multiple_bundles_merged(tmp_path):
    """Multiple bundles → all tools merged."""
    for name, tool_name in [("alpha", "tool_a"), ("beta", "tool_b")]:
        bundle = tmp_path / name
        bundle.mkdir()
        (bundle / "tools.py").write_text(
            f"def register_tools(engine, fire_broadcasts, process_runner=None):\n"
            f"    return {{'{tool_name}': lambda: '{tool_name}'}}\n"
        )
    result = load_bundle_tools(tmp_path, None, None)
    assert "tool_a" in result.tools
    assert "tool_b" in result.tools


async def test_later_bundle_overrides_earlier(tmp_path):
    """When two bundles register the same key, later (sorted) wins."""
    for name, value in [("aaa_first", "first"), ("zzz_last", "last")]:
        bundle = tmp_path / name
        bundle.mkdir()
        (bundle / "tools.py").write_text(
            f"def register_tools(engine, fire_broadcasts, process_runner=None):\n"
            f"    return {{'shared': lambda: '{value}'}}\n"
        )
    result = load_bundle_tools(tmp_path, None, None)
    # sorted order → zzz_last wins
    assert result.tools["shared"]() == "last"


# ── load_single_bundle ──────────────────────────────────────────────


async def test_single_bundle_no_tools(tmp_path):
    """Directory without tools.py → empty result."""
    bundle = tmp_path / "empty"
    bundle.mkdir()
    result = load_single_bundle(bundle, None, None)
    assert result.tools == {}


async def test_single_bundle_with_dispatch(tmp_path):
    """Bundle with register_dispatch → inner dispatch registered."""
    bundle = tmp_path / "with_dispatch"
    bundle.mkdir()
    (bundle / "tools.py").write_text(
        "def register_tools(engine, fire_broadcasts, process_runner=None):\n"
        "    return {'dt': lambda: 'ok'}\n"
        "def register_dispatch():\n"
        "    return {'inner_dt': lambda: 'dispatch'}\n"
    )
    result = load_single_bundle(bundle, None, None)
    assert "dt" in result.tools


# ── load_project_plugins ─────────────────────────────────────────────


async def test_project_plugins_no_dir(tmp_path):
    """No plugins/ dir in project → empty result."""
    result = load_project_plugins(tmp_path, None, None)
    assert result.tools == {}


async def test_project_plugins_with_valid_plugin(tmp_path):
    """plugins/ dir with valid plugin → loads via load_bundle_tools."""
    plugins = tmp_path / "plugins" / "my_plugin"
    plugins.mkdir(parents=True)
    (plugins / "tools.py").write_text(
        "def register_tools(engine, fire_broadcasts, process_runner=None):\n"
        "    return {'proj_tool': lambda: 'project'}\n"
    )
    result = load_project_plugins(tmp_path, None, None)
    assert "proj_tool" in result.tools
