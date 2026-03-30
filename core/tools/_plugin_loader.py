"""Load tool plugins from bundles and project plugins."""

import importlib.util
import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from ..dispatch import _DISPATCH

logger = logging.getLogger(__name__)

# Track which tool names belong to which bundle (for remove_bundle cleanup)
_bundle_tool_names: dict[str, list[str]] = {}


@dataclass
class BundleLoadResult:
    """Result of loading one or more bundles."""

    tools: dict[str, Any] = field(default_factory=dict)  # tool_name → callable


def load_single_bundle(
    entry: Path,
    engine,
    fire_broadcasts: Callable,
    process_runner=None,
) -> BundleLoadResult:
    """Load a single bundle directory. Calls register_tools if tools.py exists."""
    result = BundleLoadResult()
    tools_file = entry / "tools.py"
    if not tools_file.exists():
        return result
    try:
        spec = importlib.util.spec_from_file_location(
            f"bundle_{entry.name}_tools",
            tools_file,
        )
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        if hasattr(mod, "register_tools"):
            tools = mod.register_tools(
                engine,
                fire_broadcasts,
                process_runner,
            )
            result.tools.update(tools)
            _bundle_tool_names[entry.name] = list(tools.keys())
            logger.info("Loaded %d tools from bundle '%s'", len(tools), entry.name)
        if hasattr(mod, "register_dispatch"):
            inner = mod.register_dispatch()
            _DISPATCH.update(inner)
    except Exception:
        logger.exception("Failed to load tools from %s", tools_file)
    return result


def load_bundle_tools(
    bundles_dir: Path,
    engine,
    fire_broadcasts: Callable,
    process_runner=None,
    added: set[str] | None = None,
) -> BundleLoadResult:
    """Scan bundles_dir/*/tools.py — load each bundle's tools.

    If `added` is provided, only load bundles whose directory name is in the set.
    If `added` is None, load all bundles (backward compat).
    """
    combined = BundleLoadResult()
    if not bundles_dir.exists():
        return combined
    for entry in sorted(bundles_dir.iterdir()):
        if not entry.is_dir():
            continue
        if added is not None and entry.name not in added:
            continue
        result = load_single_bundle(entry, engine, fire_broadcasts, process_runner)
        combined.tools.update(result.tools)
    return combined


def load_project_plugins(
    project_dir: Path,
    engine,
    fire_broadcasts: Callable,
    process_runner=None,
) -> BundleLoadResult:
    """Scan project_dir/plugins/*/tools.py."""
    plugins_dir = project_dir / "plugins"
    if not plugins_dir.exists():
        return BundleLoadResult()
    return load_bundle_tools(plugins_dir, engine, fire_broadcasts, process_runner)


def load_installed_plugins(
    project_dir: Path,
    engine,
    fire_broadcasts: Callable,
    process_runner=None,
) -> BundleLoadResult:
    """Load tools from .fantastic/plugins/*/tools.py."""
    from .._install import list_plugin_dirs

    combined = BundleLoadResult()
    for plugin_dir in list_plugin_dirs(project_dir):
        result = load_single_bundle(plugin_dir, engine, fire_broadcasts, process_runner)
        combined.tools.update(result.tools)
    return combined
