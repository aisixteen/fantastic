"""
Tool server — exposes tools via REST and WebSocket.

Tools accessible via REST at POST /api/call and GET /api/schema.
Tools let clients execute Python via subprocess, create agents, inspect state,
read project files, and communicate across agents.

Architecture:
  - Each public operation has an inner function (_xxx) returning ToolResult.
  - Wrapper functions call the inner, fire broadcasts, format for callers.
  - _DISPATCH maps names → inner functions (registered via @register_dispatch).
  - _TOOL_DISPATCH maps names → wrapper functions (registered via @register_tool).
"""

from typing import Any

from ..dispatch import ToolResult, _DISPATCH as _DISPATCH, _TOOL_DISPATCH


def init_tools(engine, broadcast_fn, process_runner=None):
    """Wire the tools to the engine, broadcast, and process runner."""
    from . import _state

    _state._engine = engine
    _state._broadcast = broadcast_fn
    _state._process_runner = process_runner

    # Load bundle + project plugin + installed plugin tools
    from ._plugin_loader import (
        load_bundle_tools,
        load_project_plugins,
        load_installed_plugins,
    )
    from .._paths import bundled_agents_dir

    # Load all built-in bundles (handbooks, tools must always be available)
    bundle_result = load_bundle_tools(
        bundled_agents_dir(),
        engine,
        _fire_broadcasts,
        process_runner,
    )
    project_result = load_project_plugins(
        engine.project_dir,
        engine,
        _fire_broadcasts,
        process_runner,
    )
    plugin_result = load_installed_plugins(
        engine.project_dir,
        engine,
        _fire_broadcasts,
        process_runner,
    )
    _TOOL_DISPATCH.update(bundle_result.tools)
    _TOOL_DISPATCH.update(project_result.tools)
    _TOOL_DISPATCH.update(plugin_result.tools)
    # Track loaded bundles using _bundle_tool_names (keyed by directory name)
    from ._plugin_loader import _bundle_tool_names

    for bundle_name in _bundle_tool_names:
        _state._bundle_loaded[bundle_name] = True


async def _fire_broadcasts(tr: ToolResult) -> None:
    """Fire broadcast messages from a ToolResult."""
    from ._state import _broadcast

    for msg in tr.broadcast:
        await _broadcast(msg)


def _format_outputs(outputs: list[dict[str, Any]]) -> str:
    """Format code execution outputs as plain text for tool results."""
    parts = []
    for out in outputs:
        otype = out.get("output_type")
        if otype == "stream":
            parts.append(out.get("text", ""))
        elif otype == "error":
            tb = out.get("traceback", [])
            parts.append(
                "\n".join(tb) if tb else f"{out.get('ename')}: {out.get('evalue')}"
            )
        elif otype in ("execute_result", "display_data"):
            data = out.get("data", {})
            if "text/plain" in data:
                parts.append(data["text/plain"])
            elif "text/html" in data:
                parts.append(data["text/html"])
            elif "image/png" in data:
                parts.append("[image/png output]")
            else:
                parts.append(str(data))
    return "\n".join(parts)


# ─── Import submodules so @register_dispatch / @register_tool decorators fire ──

from . import _agents  # noqa: F401, E402
from . import _content  # noqa: F401, E402
from . import _process  # noqa: F401, E402
from . import _process_handlers  # noqa: F401, E402
from . import _conversation  # noqa: F401, E402
from . import _registry  # noqa: F401, E402
from . import _instances  # noqa: F401, E402
from . import _server_log  # noqa: F401, E402
from . import _bundles  # noqa: F401, E402

# Re-exports used by other modules (lifespan, recipients, tests)
from ._instance_tracking import (  # noqa: F401, E402
    _launched_processes,
    _load_tracked,
    _pid_alive,
    _instance_list_sync,
)
from ._server_log import install_log_buffer, SERVER_LOG_BUFFER_SIZE  # noqa: F401, E402

# Re-exports for tests and recipients that import from core.tools directly
from ._agents import execute_python, get_state  # noqa: F401, E402
from ._content import content_alias_file, content_alias_url, get_aliases  # noqa: F401, E402
from ._registry import get_handbook  # noqa: F401, E402
from ._instance_tracking import _save_tracked, _instance_id, _get_own_port  # noqa: F401, E402
from ._bundles import _add_bundle  # noqa: F401, E402
