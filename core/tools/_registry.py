"""Handbook, template listing, and register_template."""

import logging

from ..dispatch import ToolResult, register_dispatch, register_tool
from . import _fire_broadcasts
from . import _state

logger = logging.getLogger(__name__)


# ─── Template management ──────────────────────────────────────────────────


@register_dispatch("list_templates")
async def _list_templates() -> ToolResult:
    from ..bundles import BundleStore
    from .._paths import bundled_agents_dir

    store = BundleStore(bundled_agents_dir())
    builtin = store.list_bundles()
    # Also include project plugins that have template.json
    plugins_dir = _state._engine.project_dir / "plugins"
    if plugins_dir.exists():
        proj_store = BundleStore(plugins_dir)
        builtin.extend(proj_store.list_bundles())
    return ToolResult(data=builtin)


@register_tool("list_templates")
async def list_templates() -> list[dict]:
    """List all registered agent templates (built-in bundles + project plugins).

    Returns template.json contents for each discovered template.
    """
    tr = await _list_templates()
    return tr.data


@register_dispatch("register_template")
async def _register_template(path: str = "") -> ToolResult:
    """Register a template bundle from a project-relative directory path.

    The directory must contain template.json. Optionally tools.py for tools.
    """
    bundle_dir = _state._engine.project_dir / path
    if not bundle_dir.is_dir():
        return ToolResult(data={"error": f"Directory not found: {path}"})
    template_file = bundle_dir / "template.json"
    if not template_file.exists():
        return ToolResult(data={"error": f"No template.json in {path}"})

    import json

    tmpl = json.loads(template_file.read_text())
    name = tmpl.get("name", bundle_dir.name)

    from ._plugin_loader import load_single_bundle
    from . import _TOOL_DISPATCH, _fire_broadcasts

    result = load_single_bundle(
        bundle_dir, _state._engine, _fire_broadcasts, _state._process_runner
    )
    _TOOL_DISPATCH.update(result.tools)

    return ToolResult(
        data={
            "name": name,
            "path": path,
            "tools": list(result.tools.keys()),
        },
        broadcast=[{"type": "template_registered", "name": name}],
    )


@register_tool("register_template")
async def register_template(path: str) -> dict:
    """Register a template bundle from a project-relative directory path.

    The directory must contain template.json. Optionally tools.py for tools.

    Args:
        path: Project-relative path to the bundle directory (e.g. "plugins/my-widget").
    """
    tr = await _register_template(path=path)
    if "error" not in tr.data:
        await _fire_broadcasts(tr)
    return tr.data


@register_dispatch("get_handbook")
async def _get_handbook(skill: str = "") -> ToolResult:
    from .._paths import skills_dir, claude_md_path

    search_dirs = [_state._engine.project_dir / "skills", skills_dir()]
    if skill:
        for sd in search_dirs:
            skill_file = sd / f"{skill}.md"
            if skill_file.exists():
                return ToolResult(
                    data={"text": f"# SKILL: {skill}\n\n{skill_file.read_text()}"}
                )
        # Dynamic available list from search dirs
        available = set()
        for sd in search_dirs:
            if sd.exists():
                available.update(p.stem for p in sd.glob("*.md"))
        avail_str = ", ".join(sorted(available)) if available else "(none)"
        return ToolResult(
            data={"error": f"Skill '{skill}' not found. Available: {avail_str}"}
        )

    for candidate in [_state._engine.project_dir / "CLAUDE.md", claude_md_path()]:
        if candidate.exists():
            return ToolResult(data={"text": candidate.read_text()})
    return ToolResult(data={"error": "No handbook files found"})


@register_tool("get_handbook")
async def get_handbook(skill: str = "") -> str:
    """Get the handbook.

    Without arguments: returns CLAUDE.md (overview, tool index, architecture).
    With skill name: returns that specific skill doc.

    Skills are provided by bundles — use bundle-specific handbooks instead:
        get_handbook_canvas(skill="canvas-management")
        get_handbook_terminal(skill="terminal-control")

    Examples:
        get_handbook()                        # overview (CLAUDE.md)
    """
    tr = await _get_handbook(skill)
    if "error" in tr.data:
        return f"[ERROR] {tr.data['error']}"
    return tr.data["text"]
