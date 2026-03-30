"""Content tools — aliases."""

import os
import secrets

from ..dispatch import ToolResult, register_dispatch, register_tool
from . import _state


@register_dispatch("content_alias_file")
async def _content_alias_file(file_path: str, persistent: bool = True) -> ToolResult:
    alias_id = secrets.token_hex(4)
    abs_path = os.path.abspath(file_path)
    try:
        rel = os.path.relpath(abs_path, str(_state._engine.project_dir))
        if not rel.startswith(".."):
            _state._engine.add_content_alias(
                alias_id,
                {
                    "type": "file",
                    "path": rel,
                    "relative": True,
                    "persistent": persistent,
                },
            )
        else:
            _state._engine.add_content_alias(
                alias_id, {"type": "file", "path": abs_path, "persistent": persistent}
            )
    except ValueError:
        _state._engine.add_content_alias(
            alias_id, {"type": "file", "path": abs_path, "persistent": persistent}
        )
    return ToolResult(data={"alias_path": f"/content/{alias_id}", "alias_id": alias_id})


@register_tool("content_alias_file")
async def content_alias_file(file_path: str, persistent: bool = True) -> str:
    """Create a URL alias for a local file. Returns the alias path to use in HTML (e.g. in <img src="...">).

    Args:
        file_path: Absolute or relative path to the file to serve.
        persistent: If True, alias survives server restart. Default True.
    """
    tr = await _content_alias_file(file_path, persistent)
    return tr.data["alias_path"]


@register_dispatch("content_alias_url")
async def _content_alias_url(url: str, persistent: bool = True) -> ToolResult:
    alias_id = secrets.token_hex(4)
    _state._engine.add_content_alias(
        alias_id, {"type": "url", "url": url, "persistent": persistent}
    )
    return ToolResult(data={"alias_path": f"/content/{alias_id}", "alias_id": alias_id})


@register_tool("content_alias_url")
async def content_alias_url(url: str, persistent: bool = True) -> str:
    """Create a URL alias that redirects to the given URL. Returns the alias path to use in HTML.

    Args:
        url: The URL to redirect to.
        persistent: If True, alias survives server restart. Default True.
    """
    tr = await _content_alias_url(url, persistent)
    return tr.data["alias_path"]


@register_dispatch("get_aliases")
async def _get_aliases() -> ToolResult:
    aliases = _state._engine.content_aliases
    result = []
    for alias_id, entry in aliases.items():
        info = {
            "alias_id": alias_id,
            "alias_path": f"/content/{alias_id}",
            "type": entry["type"],
            "persistent": entry.get("persistent", False),
        }
        if entry["type"] == "file":
            info["path"] = entry["path"]
            info["relative"] = entry.get("relative", False)
        elif entry["type"] == "url":
            info["url"] = entry["url"]
        result.append(info)
    return ToolResult(data={"aliases": result})


@register_tool("get_aliases")
async def get_aliases() -> list[dict]:
    """List all content aliases with their paths and persistence flags."""
    tr = await _get_aliases()
    return tr.data["aliases"]
