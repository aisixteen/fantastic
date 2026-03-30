"""Bundle management — inner functions only (no tool decorators).

Inner functions shared between CLI subcommands and conversation input.
Bundles are derived from agents — no config["added"] list.
"""

import logging

from .. import conversation
from ..dispatch import ToolResult, register_dispatch, register_tool

logger = logging.getLogger(__name__)


@register_dispatch("add_bundle")
@register_tool("add_bundle")
async def _add_bundle(
    bundle_name: str, name: str = "", working_dir: str = "", from_source: str = ""
) -> ToolResult:
    """Add a bundle: validate, call on_add hook, hot-load tools if server running."""
    from . import _state, _fire_broadcasts
    from .._paths import bundled_agents_dir
    from ..bundles import BundleStore

    if not _state._engine:
        return ToolResult(data={"error": "No engine available"})

    # External plugin install (files only, no agent)
    if from_source:
        from .._install import install_plugin

        try:
            install_plugin(_state._engine.project_dir, from_source, bundle_name)
        except RuntimeError as e:
            return ToolResult(data={"error": str(e)})
        conversation.say("fantastic", f"plugin '{bundle_name}' installed")
        return ToolResult(data={"installed": bundle_name})

    # Resolve bundle dir: built-in first, then installed plugins
    store = BundleStore(bundled_agents_dir())
    bundle = store.get_bundle(bundle_name)
    if bundle:
        bundle_dir = bundled_agents_dir() / bundle_name
    else:
        from .._install import get_plugin_dir

        plugin_dir = get_plugin_dir(_state._engine.project_dir, bundle_name)
        if plugin_dir.exists() and (plugin_dir / "template.json").exists():
            bundle_dir = plugin_dir
        else:
            available = [
                b.get("bundle", b.get("name", "?")) for b in store.list_bundles()
            ]
            return ToolResult(
                data={"error": f"Unknown bundle: {bundle_name}", "available": available}
            )

    # Snapshot agent IDs before on_add so we can detect new agents
    before_ids = {a["id"] for a in _state._engine.store.list_agents()}

    # Call bundle's on_add hook (creates agent, registers hooks, etc.)
    _call_bundle_hook_sync(
        bundle_dir,
        "on_add",
        str(_state._engine.project_dir),
        name=name,
        working_dir=working_dir,
    )

    # Broadcast agent_created for any agents created by on_add
    try:
        from ..server import broadcast

        for agent in _state._engine.store.list_agents():
            if agent["id"] not in before_ids:
                await broadcast({"type": "agent_created", "agent": agent})
    except Exception:
        logger.debug("Could not broadcast new agents (server may not be running)")

    # Hot-load tools if not already loaded
    if bundle_name not in _state._bundle_loaded:
        from ._plugin_loader import load_single_bundle
        from . import _TOOL_DISPATCH

        result = load_single_bundle(
            bundle_dir,
            _state._engine,
            _fire_broadcasts,
            _state._process_runner,
        )
        _TOOL_DISPATCH.update(result.tools)
        if result.tools:
            _state._bundle_loaded[bundle_name] = True

    # Hot-swap web UI + run new lifespan hooks (e.g. URL announcement)
    try:
        from ..server import remount_web_ui, broadcast
        from ..server._state import _lifespan_hooks, _lifespan_hooks_ran

        remount_web_ui()
        # Run any newly registered lifespan hooks that haven't run yet
        import core.server._state as _srv_state

        for hook in _lifespan_hooks[_lifespan_hooks_ran:]:
            await hook(_srv_state, broadcast)
        _srv_state._lifespan_hooks_ran = len(_lifespan_hooks)
    except Exception:
        logger.debug("Could not hot-swap web UI (server may not be running)")

    conversation.say(bundle_name, "bundle loaded")
    return ToolResult(data={"added": bundle_name, "name": name or "main"})


async def _remove_bundle(bundle_name: str, name: str = "") -> ToolResult:
    """Remove a bundle instance. For multi-instance bundles, name is required."""
    from . import _state

    if not _state._engine:
        return ToolResult(data={"error": "No engine available"})

    # Find agents with this bundle
    all_agents = _state._engine.store.list_agents()
    bundle_agents = [a for a in all_agents if a.get("bundle") == bundle_name]

    if not bundle_agents:
        return ToolResult(data={"error": f"No {bundle_name} instances found"})

    # Name required if multiple instances exist
    if not name and len(bundle_agents) > 1:
        names = [a.get("display_name", a["id"]) for a in bundle_agents]
        return ToolResult(data={"error": f"Specify --name. Active: {', '.join(names)}"})

    # Filter by name if provided
    if name:
        targets = [a for a in bundle_agents if a.get("display_name") == name]
        if not targets:
            names = [a.get("display_name", a["id"]) for a in bundle_agents]
            return ToolResult(
                data={
                    "error": f"No {bundle_name} '{name}' found. Active: {', '.join(names)}"
                }
            )
    else:
        targets = bundle_agents

    # Cascade delete each matched agent (kills children + processes)
    from ._agents import _delete_agent, _fire_broadcasts

    for agent in targets:
        # Unlock and cascade delete
        _state._engine.store.update_agent_meta(agent["id"], delete_lock=False)
        tr = await _delete_agent(agent["id"])
        await _fire_broadcasts(tr)

    # If no more agents with this bundle → unload tools
    remaining = [
        a for a in _state._engine.store.list_agents() if a.get("bundle") == bundle_name
    ]
    if not remaining:
        from . import _TOOL_DISPATCH
        from ._plugin_loader import _bundle_tool_names

        for tool_name in _bundle_tool_names.get(bundle_name, []):
            _TOOL_DISPATCH.pop(tool_name, None)
        _bundle_tool_names.pop(bundle_name, None)
        _state._bundle_loaded.pop(bundle_name, None)

    # Hot-swap web UI
    try:
        from ..server import remount_web_ui

        remount_web_ui()
    except Exception:
        logger.debug("Could not hot-swap web UI (server may not be running)")
    try:
        from ..server import broadcast

        await broadcast({"type": "reload"})
    except Exception:
        logger.debug("Could not broadcast reload")

    conversation.say(
        "fantastic", f"Removed {bundle_name}" + (f" '{name}'" if name else "")
    )
    return ToolResult(data={"removed": bundle_name, "name": name})


async def _list_bundles() -> ToolResult:
    """List all bundles with instances."""
    from . import _state
    from .._paths import bundled_agents_dir
    from ..bundles import BundleStore
    from .._install import list_plugin_dirs
    import json

    store = BundleStore(bundled_agents_dir())
    bundles = store.list_bundles()

    # Gather agent instances per bundle
    agents = _state._engine.store.list_agents() if _state._engine else []
    bundle_instances: dict[str, list[dict]] = {}
    for a in agents:
        b = a.get("bundle")
        if b:
            bundle_instances.setdefault(b, []).append(a)

    result = []
    seen_names = set()
    for b in bundles:
        bname = b.get("bundle", b.get("name", "?"))
        seen_names.add(bname)
        instances = bundle_instances.get(bname, [])
        entry = {
            "name": bname,
            "added": len(instances) > 0,
            "instances": [
                {
                    "id": a["id"],
                    "display_name": a.get("display_name", ""),
                    "children": len(_state._engine.store.list_children(a["id"]))
                    if _state._engine
                    else 0,
                }
                for a in instances
            ],
        }
        result.append(entry)

    # Installed plugins
    if _state._engine:
        for pdir in list_plugin_dirs(_state._engine.project_dir):
            try:
                tmpl = json.loads((pdir / "template.json").read_text())
                pname = tmpl.get("bundle", pdir.name)
            except Exception:
                pname = pdir.name
            if pname in seen_names:
                continue
            seen_names.add(pname)
            instances = bundle_instances.get(pname, [])
            result.append(
                {
                    "name": pname,
                    "added": len(instances) > 0,
                    "plugin": True,
                    "instances": [
                        {
                            "id": a["id"],
                            "display_name": a.get("display_name", ""),
                            "children": len(
                                _state._engine.store.list_children(a["id"])
                            ),
                        }
                        for a in instances
                    ],
                }
            )

    return ToolResult(data=result)


def _call_bundle_hook_sync(bundle_dir, hook_name, project_dir, **kwargs):
    """Call a hook function on a bundle's tools.py if it exists."""
    import importlib.util

    tools_file = bundle_dir / "tools.py"
    if not tools_file.exists():
        return
    try:
        spec = importlib.util.spec_from_file_location(
            f"bundle_{bundle_dir.name}_hook",
            tools_file,
        )
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        fn = getattr(mod, hook_name, None)
        if fn:
            import inspect

            sig = inspect.signature(fn)
            # Pass kwargs that the hook accepts
            valid_kwargs = {k: v for k, v in kwargs.items() if k in sig.parameters}
            fn(project_dir, **valid_kwargs)
    except Exception as e:
        logger.warning(f"Hook {hook_name} failed for {bundle_dir.name}: {e}")
